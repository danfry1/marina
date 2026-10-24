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
/// At most this many `git` refreshes per sampler tick (run in parallel).
const PER_TICK: usize = 6;
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
        let fresh: Vec<(PathBuf, Option<GitStatus>)> = std::thread::scope(|s| {
            let jobs: Vec<_> = due
                .into_iter()
                .take(self.per_tick)
                .map(|d| s.spawn(move || (d.clone(), status(d))))
                .collect();
            jobs.into_iter().filter_map(|j| j.join().ok()).collect()
        });
        let now = Instant::now();
        for (d, st) in fresh {
            self.entries.insert(d, (now, st));
        }
        self.entries
            .iter()
            .filter_map(|(d, (_, s))| s.map(|s| (d.clone(), s)))
            .collect()
    }
}

/// Where a session's work stands, for landing it (`L`): its tree and
/// branch, and the main checkout's branch it would merge into.
#[derive(Clone, Debug, PartialEq)]
pub struct Landing {
    pub tree: PathBuf,
    /// The repository's main checkout; `tree` itself unless that's a
    /// linked worktree.
    pub main: PathBuf,
    pub branch: Option<String>,
    /// The main checkout's branch.
    pub base: Option<String>,
    /// Uncommitted or untracked files in `tree`.
    pub dirty: u32,
    /// The main checkout has uncommitted changes to tracked files.
    pub main_dirty: bool,
    /// Commits on the branch that `base` hasn't got, and the reverse.
    pub ahead: u32,
    pub behind: u32,
    /// It has an upstream and nothing unpushed.
    pub pushed: bool,
}

impl Landing {
    pub fn linked(&self) -> bool {
        self.tree != self.main
    }

    /// Why its branch can't be fast-forwarded into the main checkout's,
    /// if it can't.
    pub fn merge_blocker(&self) -> Option<String> {
        let (Some(b), Some(base)) = (&self.branch, &self.base) else {
            return Some("it isn't on a branch".into());
        };
        if !self.linked() || b == base {
            return Some(format!("it works on {base} itself"));
        }
        if self.dirty > 0 {
            return Some(format!("{} uncommitted file(s) — commit first", self.dirty));
        }
        if self.ahead == 0 {
            return Some(format!("{b} has nothing {base} hasn't"));
        }
        if self.behind > 0 {
            return Some(format!(
                "{base} has {} commit(s) {b} hasn't — rebase it first",
                self.behind
            ));
        }
        if self.main_dirty {
            return Some(format!(
                "the main checkout has uncommitted changes on {base}"
            ));
        }
        None
    }

    /// Why its worktree can't be removed without losing anything, if it
    /// can't.
    pub fn remove_blocker(&self) -> Option<String> {
        if !self.linked() {
            return Some("it's the repository's main checkout".into());
        }
        if self.dirty > 0 {
            return Some(format!("{} uncommitted file(s) would be lost", self.dirty));
        }
        if self.ahead > 0 && !self.pushed {
            return Some(format!(
                "{} commit(s) are only on this branch — merge or push first",
                self.ahead
            ));
        }
        None
    }
}

/// Where the work in the tree containing `dir` stands; `None` outside a
/// repository.
pub fn landing(dir: &Path) -> Option<Landing> {
    let tree = PathBuf::from(git(dir, &["rev-parse", "--show-toplevel"])?.trim());
    let common = git(
        &tree,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    let common = PathBuf::from(common.trim());
    let main = match common.file_name() {
        Some(n) if n == ".git" => common.parent()?.to_path_buf(),
        _ => tree.clone(), // a bare repository's worktree: treat as its own
    };
    let branch_of = |d: &Path| {
        git(d, &["symbolic-ref", "--short", "-q", "HEAD"])
            .map(|b| b.trim().to_string())
            .filter(|b| !b.is_empty())
    };
    let branch = branch_of(&tree);
    let base = branch_of(&main);
    let dirty = git(
        &tree,
        &["status", "--porcelain", "--untracked-files=normal"],
    )
    .map(|s| s.lines().count() as u32)
    .unwrap_or(0);
    let main_dirty = git(&main, &["status", "--porcelain", "--untracked-files=no"])
        .is_some_and(|s| !s.trim().is_empty());
    let (behind, ahead) = match &base {
        Some(base) => git(
            &tree,
            &[
                "rev-list",
                "--left-right",
                "--count",
                &format!("{base}...HEAD"),
            ],
        )
        .and_then(|s| {
            let mut n = s.split_whitespace().map(|n| n.parse::<u32>().ok());
            Some((n.next()??, n.next()??))
        })
        .unwrap_or((0, 0)),
        None => (0, 0),
    };
    let pushed =
        git(&tree, &["rev-list", "--count", "@{u}..HEAD"]).is_some_and(|n| n.trim() == "0");
    Some(Landing {
        tree,
        main,
        branch,
        base,
        dirty,
        main_dirty,
        ahead,
        behind,
        pushed,
    })
}

/// Run a git command that changes something; `Err` carries git's message.
fn git_do(dir: &Path, args: &[&str]) -> Result<(), String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("git: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        Err(err
            .lines()
            .next()
            .unwrap_or("git failed")
            .trim()
            .to_string())
    }
}

/// Fast-forward the main checkout's branch to `l`'s branch — after
/// checking again that nothing has changed since `l` was read.
pub fn merge_ff(l: &Landing) -> Result<String, String> {
    let now = landing(&l.tree).ok_or("its tree is gone")?;
    if let Some(why) = now.merge_blocker() {
        return Err(format!("not merging: {why}"));
    }
    let (Some(b), Some(base)) = (&now.branch, &now.base) else {
        return Err("not on a branch".into());
    };
    git_do(&now.main, &["merge", "--ff-only", "--quiet", b])?;
    Ok(format!(
        "{base} fast-forwarded to {b} (+{} commit(s))",
        now.ahead
    ))
}

/// Remove `l`'s worktree (never forced), and its branch when that's merged
/// — after checking again that nothing would be lost.
pub fn remove_worktree(l: &Landing) -> Result<String, String> {
    let now = landing(&l.tree).ok_or("its tree is gone")?;
    if let Some(why) = now.remove_blocker() {
        return Err(format!("not removing: {why}"));
    }
    let path = now.tree.display().to_string();
    git_do(&now.main, &["worktree", "remove", &path])?;
    match (&now.branch, now.ahead) {
        // merged: the branch has nothing left; `-d` refuses otherwise anyway
        (Some(b), 0) if now.base.as_ref() != Some(b) => {
            match git_do(&now.main, &["branch", "-d", b]) {
                Ok(()) => Ok(format!("removed {path} and its merged branch {b}")),
                Err(e) => Ok(format!("removed {path}; kept branch {b} ({e})")),
            }
        }
        (Some(b), _) => Ok(format!("removed {path}; branch {b} is kept (pushed)")),
        (None, _) => Ok(format!("removed {path}")),
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
    fn landing_a_worktree_merges_only_forward_and_removes_only_what_is_safe() {
        let base = std::env::temp_dir().join(format!("marina-land-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let repo = base.join("repo");
        let wt = base.join("repo-wt");
        std::fs::create_dir_all(&repo).unwrap();
        let run = |dir: &Path, args: &[&str]| {
            let out = Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(args)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        run(&repo, &["init", "-q", "-b", "main"]);
        std::fs::write(repo.join("a.txt"), "one\n").unwrap();
        run(&repo, &["add", "."]);
        run(&repo, &["commit", "-qm", "init"]);
        run(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature",
                wt.to_str().unwrap(),
            ],
        );

        // nothing done yet: nothing to merge; removable (nothing to lose)
        let l = landing(&wt).unwrap();
        assert!(l.linked());
        assert_eq!(l.branch.as_deref(), Some("feature"));
        assert_eq!(l.base.as_deref(), Some("main"));
        assert!(l.merge_blocker().is_some());
        assert_eq!(l.remove_blocker(), None);

        // uncommitted work: neither
        std::fs::write(wt.join("b.txt"), "new\n").unwrap();
        let l = landing(&wt).unwrap();
        assert_eq!(l.dirty, 1);
        assert!(l.merge_blocker().unwrap().contains("uncommitted"));
        assert!(l.remove_blocker().unwrap().contains("lost"));

        // committed, unpushed: mergeable, not removable
        run(&wt, &["add", "."]);
        run(&wt, &["commit", "-qm", "b"]);
        let l = landing(&wt).unwrap();
        assert_eq!((l.ahead, l.behind, l.dirty), (1, 0, 0));
        assert_eq!(l.merge_blocker(), None);
        assert!(l.remove_blocker().unwrap().contains("only on this branch"));
        assert!(
            remove_worktree(&l).is_err(),
            "refuses again at the moment of acting"
        );
        assert!(wt.exists());

        // main moved on: no fast-forward
        std::fs::write(repo.join("c.txt"), "c\n").unwrap();
        run(&repo, &["add", "."]);
        run(&repo, &["commit", "-qm", "c"]);
        let l2 = landing(&wt).unwrap();
        assert!(l2.merge_blocker().unwrap().contains("rebase"));
        assert!(merge_ff(&l).is_err(), "re-checked, not the stale reading");
        run(&repo, &["reset", "-q", "--hard", "HEAD~1"]);

        // merge, then remove: the worktree and its merged branch go
        merge_ff(&landing(&wt).unwrap()).unwrap();
        assert!(repo.join("b.txt").exists());
        let msg = remove_worktree(&landing(&wt).unwrap()).unwrap();
        assert!(msg.contains("merged branch feature"), "{msg}");
        assert!(!wt.exists());
        let _ = std::fs::remove_dir_all(&base);
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
