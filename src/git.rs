//! Working-tree status for agent sessions: what an agent has changed and not
//! committed or pushed. Read with `git status` / `git diff --shortstat` under
//! `GIT_OPTIONAL_LOCKS=0`, so marina never takes the index lock out from under
//! an agent that's committing.
//!
//! Status is per directory and cached; the sampler refreshes a few stale
//! entries per tick so a cockpit full of sessions never runs a burst of git.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// How long a directory's status is trusted.
const TTL: Duration = Duration::from_secs(20);
/// At most this many `git` refreshes per sampler tick.
const PER_TICK: usize = 3;
const TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GitStatus {
    /// Tracked files with uncommitted changes (staged or not).
    pub changed: u32,
    pub untracked: u32,
    pub insertions: u32,
    pub deletions: u32,
    /// Commits not on the upstream; `None` without an upstream.
    pub ahead: Option<u32>,
    pub behind: Option<u32>,
}

impl GitStatus {
    /// Nothing uncommitted, untracked, or unpushed.
    pub fn is_clean(&self) -> bool {
        self.changed == 0 && self.untracked == 0 && self.ahead.unwrap_or(0) == 0
    }

    /// Compact form for a column: `●4 ↑2`, `✓` when clean.
    pub fn short(&self) -> String {
        if self.is_clean() {
            return "✓".into();
        }
        let mut parts = Vec::new();
        if self.changed + self.untracked > 0 {
            parts.push(format!("●{}", self.changed + self.untracked));
        }
        if let Some(n) = self.ahead.filter(|&n| n > 0) {
            parts.push(format!("↑{n}"));
        }
        parts.join(" ")
    }

    /// `4 files +120 −30 · 2 untracked · 2 unpushed`, or `clean`.
    pub fn describe(&self) -> String {
        if self.is_clean() {
            return match self.behind.filter(|&n| n > 0) {
                Some(n) => format!("clean · {n} behind upstream"),
                None => "clean".into(),
            };
        }
        let mut parts = Vec::new();
        if self.changed > 0 {
            parts.push(format!(
                "{} file{} +{} −{}",
                self.changed,
                if self.changed == 1 { "" } else { "s" },
                self.insertions,
                self.deletions
            ));
        }
        if self.untracked > 0 {
            parts.push(format!("{} untracked", self.untracked));
        }
        if let Some(n) = self.ahead.filter(|&n| n > 0) {
            parts.push(format!("{n} unpushed"));
        }
        parts.join(" · ")
    }
}

/// Parse `git status --porcelain=v2 --branch` (plus `diff --shortstat`).
pub fn parse(status: &str, shortstat: &str) -> GitStatus {
    let mut s = GitStatus::default();
    for line in status.lines() {
        if let Some(ab) = line.strip_prefix("# branch.ab ") {
            let mut it = ab.split_whitespace();
            s.ahead = it
                .next()
                .and_then(|a| a.trim_start_matches('+').parse().ok());
            s.behind = it
                .next()
                .and_then(|b| b.trim_start_matches('-').parse().ok());
        } else if line.starts_with("1 ") || line.starts_with("2 ") || line.starts_with("u ") {
            s.changed += 1;
        } else if line.starts_with("? ") {
            s.untracked += 1;
        }
    }
    // ` 4 files changed, 120 insertions(+), 30 deletions(-)`
    for part in shortstat.split(',') {
        let n = part
            .split_whitespace()
            .next()
            .and_then(|n| n.parse().ok())
            .unwrap_or(0);
        if part.contains("insertion") {
            s.insertions = n;
        } else if part.contains("deletion") {
            s.deletions = n;
        }
    }
    s
}

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0");
    crate::exec::output(cmd, TIMEOUT)
}

/// Status of the work tree containing `dir`; `None` outside a repo.
pub fn status(dir: &Path) -> Option<GitStatus> {
    let st = git(
        dir,
        &[
            "status",
            "--porcelain=v2",
            "--branch",
            "--untracked-files=normal",
        ],
    )?;
    // An unborn branch has no HEAD to diff against; counts still stand.
    let short = git(dir, &["diff", "--shortstat", "HEAD"]).unwrap_or_default();
    Some(parse(&st, &short))
}

/// The uncommitted diff of the work tree at `dir`, as lines — `--stat`
/// summary first, then the patch. For the TUI's `D` pane.
pub fn diff_lines(dir: &Path) -> Vec<String> {
    let stat = git(dir, &["diff", "--stat", "HEAD"]).unwrap_or_default();
    let patch = git(dir, &["-c", "color.ui=never", "diff", "HEAD"]).unwrap_or_default();
    let untracked = git(dir, &["ls-files", "--others", "--exclude-standard"]).unwrap_or_default();
    let mut lines: Vec<String> = stat.lines().map(String::from).collect();
    if !untracked.trim().is_empty() {
        lines.push(String::new());
        lines.push("untracked:".into());
        lines.extend(untracked.lines().map(|l| format!("  {l}")));
    }
    if !patch.is_empty() {
        lines.push(String::new());
        lines.extend(patch.lines().map(String::from));
    }
    if lines.is_empty() {
        lines.push("no uncommitted changes".into());
    }
    lines
}

/// Per-directory status cache, refreshed a few entries per tick.
pub struct Cache {
    entries: HashMap<PathBuf, (Instant, Option<GitStatus>)>,
    per_tick: usize,
}

impl Default for Cache {
    fn default() -> Self {
        Cache {
            entries: HashMap::new(),
            per_tick: PER_TICK,
        }
    }
}

impl Cache {
    /// Refresh everything on the next `get` — for one-shot CLI snapshots.
    pub fn eager(&mut self) {
        self.per_tick = usize::MAX;
    }

    /// Statuses for `dirs`: cached values, after refreshing up to
    /// `PER_TICK` missing or expired ones (missing first). Directories no
    /// longer asked about are dropped.
    pub fn get(&mut self, dirs: &[PathBuf]) -> HashMap<PathBuf, GitStatus> {
        self.entries.retain(|d, _| dirs.contains(d));
        let mut due: Vec<&PathBuf> = dirs
            .iter()
            .filter(|d| {
                self.entries
                    .get(*d)
                    .is_none_or(|(at, _)| at.elapsed() > TTL)
            })
            .collect();
        due.sort_by_key(|d| self.entries.get(*d).map(|(at, _)| *at));
        due.dedup();
        for d in due.into_iter().take(self.per_tick) {
            self.entries.insert(d.clone(), (Instant::now(), status(d)));
        }
        self.entries
            .iter()
            .filter_map(|(d, (_, s))| s.map(|s| (d.clone(), s)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_porcelain_v2_and_shortstat() {
        let st = "\
# branch.oid 0123
# branch.head main
# branch.upstream origin/main
# branch.ab +2 -1
1 .M N... 100644 100644 100644 a b src/x.rs
2 R. N... 100644 100644 100644 a b R100 new.rs\told.rs
u UU N... 100644 100644 100644 100644 a b c conflict.rs
? notes.md
? tmp/
";
        let s = parse(st, " 3 files changed, 120 insertions(+), 30 deletions(-)\n");
        assert_eq!(
            s,
            GitStatus {
                changed: 3,
                untracked: 2,
                insertions: 120,
                deletions: 30,
                ahead: Some(2),
                behind: Some(1),
            }
        );
        assert_eq!(s.short(), "●5 ↑2");
        assert_eq!(s.describe(), "3 files +120 −30 · 2 untracked · 2 unpushed");
    }

    #[test]
    fn clean_trees_and_missing_upstreams() {
        let s = parse("# branch.head main\n", "");
        assert!(s.is_clean());
        assert_eq!((s.ahead, s.short()), (None, "✓".to_string()));
        let s = parse("# branch.ab +0 -3\n", "");
        assert_eq!(s.describe(), "clean · 3 behind upstream");
        let s = parse(" 1 file changed, 1 insertion(+)", "");
        assert!(s.is_clean(), "shortstat alone doesn't make a file changed");
    }

    #[test]
    fn status_of_a_real_repo() {
        let dir = std::env::temp_dir().join(format!("marina-gitstatus-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let run = |args: &[&str]| {
            Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(args)
                .output()
                .unwrap()
        };
        run(&["init", "-q", "-b", "main"]);
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        run(&["add", "."]);
        run(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            "init",
        ]);
        assert!(status(&dir).unwrap().is_clean());
        std::fs::write(dir.join("a.txt"), "one\ntwo\n").unwrap();
        std::fs::write(dir.join("b.txt"), "new\n").unwrap();
        let s = status(&dir).unwrap();
        assert_eq!((s.changed, s.untracked, s.insertions), (1, 1, 1));
        let diff = diff_lines(&dir).join("\n");
        assert!(diff.contains("+two") && diff.contains("b.txt"), "{diff}");
        let _ = std::fs::remove_dir_all(&dir);
        assert!(status(Path::new("/")).is_none(), "not a repo");
    }
}
