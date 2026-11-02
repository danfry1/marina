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

/// One changed file in a work tree's uncommitted diff.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileDiff {
    pub path: String,
    /// The path before a rename.
    pub from: Option<String>,
    /// `M` modified, `A` added, `D` deleted, `R` renamed, `?` untracked.
    pub status: char,
    pub added: usize,
    pub removed: usize,
    /// The patch body: `@@` hunk headers and ` `/`+`/`-` lines, plus `\`
    /// notes (no newline at end, binary file, truncated).
    pub lines: Vec<String>,
}

/// Untracked files read for the pane: at most this many, each up to this
/// many bytes, so a stray build directory can't stall it.
const UNTRACKED_FILES: usize = 100;
const UNTRACKED_BYTES: u64 = 64 * 1024;

/// The uncommitted changes of the work tree at `dir`, file by file —
/// tracked changes against `HEAD`, then untracked files as all-added.
/// For the TUI's `d` pane.
pub fn diff_files(dir: &Path) -> Vec<FileDiff> {
    // pin the output shape whatever the user's config says: no external
    // diff tool, no colour, a/ b/ prefixes, paths unescaped
    let patch = git(
        dir,
        &[
            "-c",
            "core.quotePath=false",
            "diff",
            "--no-ext-diff",
            "--no-color",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            "HEAD",
        ],
    )
    .unwrap_or_default();
    let untracked = git(
        dir,
        &[
            "-c",
            "core.quotePath=false",
            "ls-files",
            "--others",
            "--exclude-standard",
        ],
    )
    .unwrap_or_default();
    let mut files = parse_patch(&patch);
    for (i, rel) in untracked.lines().filter(|l| !l.is_empty()).enumerate() {
        files.push(untracked_file(dir, rel, i < UNTRACKED_FILES));
    }
    files
}

/// Split `git diff` output into files.
pub fn parse_patch(patch: &str) -> Vec<FileDiff> {
    let mut files = Vec::new();
    let mut cur: Option<FileDiff> = None;
    let mut in_hunk = false;
    for line in patch.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            files.extend(cur.take());
            cur = Some(FileDiff {
                path: path_from_header(rest),
                status: 'M',
                ..Default::default()
            });
            in_hunk = false;
            continue;
        }
        let Some(f) = cur.as_mut() else { continue };
        if in_hunk {
            match line.as_bytes().first() {
                Some(b'+') => f.added += 1,
                Some(b'-') => f.removed += 1,
                Some(b' ' | b'\\' | b'@') => {}
                None => {} // an empty context line with its space stripped
                _ => in_hunk = false,
            }
            if in_hunk {
                f.lines.push(if line.is_empty() {
                    " ".into()
                } else {
                    line.into()
                });
                continue;
            }
        }
        if line.starts_with("@@") {
            in_hunk = true;
            f.lines.push(line.into());
        } else if line.starts_with("new file mode") {
            f.status = 'A';
        } else if line.starts_with("deleted file mode") {
            f.status = 'D';
        } else if let Some(p) = line.strip_prefix("rename from ") {
            f.status = 'R';
            f.from = Some(unquote(p));
        } else if let Some(p) = line.strip_prefix("rename to ") {
            f.path = unquote(p);
        } else if let Some(p) = line.strip_prefix("+++ ") {
            if let Some(p) = unquote(p).strip_prefix("b/") {
                f.path = p.to_string();
            }
        } else if line.starts_with("Binary files ") || line == "GIT binary patch" {
            f.lines.push("\\ binary file".into());
        }
    }
    files.extend(cur);
    files
}

/// `a/x b/x` (or quoted) from a `diff --git` line → `x`. The `+++` and
/// `rename to` lines override it; this is what a deletion or a
/// mode-only change has.
fn path_from_header(rest: &str) -> String {
    let rest = rest.trim();
    // unquoted and unrenamed: the two halves are `a/X` and `b/X`
    if rest.len() % 2 == 1 {
        let mid = rest.len() / 2;
        if rest.is_char_boundary(mid) {
            let (a, b) = (&rest[..mid], &rest[mid + 1..]);
            if let (Some(a), Some(b)) = (a.strip_prefix("a/"), b.strip_prefix("b/")) {
                if a == b {
                    return a.to_string();
                }
            }
        }
    }
    match rest.rfind(" b/").or_else(|| rest.rfind(" \"b/")) {
        Some(i) => unquote(&rest[i + 1..]).trim_start_matches("b/").to_string(),
        None => rest.to_string(),
    }
}

/// Undo git's C-style quoting of a path (`"a\"b"`).
fn unquote(p: &str) -> String {
    let Some(inner) = p.strip_prefix('"').and_then(|p| p.strip_suffix('"')) else {
        return p.to_string();
    };
    let mut out = String::new();
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some(c) => out.push(c),
            None => {}
        }
    }
    out
}

/// An untracked file as an all-added diff (or a note, when it's binary,
/// big, or one too many to read).
fn untracked_file(dir: &Path, rel: &str, read: bool) -> FileDiff {
    use std::io::Read;
    let mut f = FileDiff {
        path: rel.to_string(),
        status: '?',
        ..Default::default()
    };
    let path = dir.join(rel);
    let regular = std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_file());
    if !read || !regular {
        f.lines.push("\\ not read".into());
        return f;
    }
    let mut bytes = Vec::new();
    let truncated = std::fs::File::open(&path)
        .and_then(|h| h.take(UNTRACKED_BYTES + 1).read_to_end(&mut bytes))
        .map(|n| n as u64 > UNTRACKED_BYTES);
    let truncated = match truncated {
        Ok(t) => t,
        Err(_) => {
            f.lines.push("\\ couldn't read it".into());
            return f;
        }
    };
    if bytes.contains(&0) {
        f.lines.push("\\ binary file".into());
        return f;
    }
    bytes.truncate(UNTRACKED_BYTES as usize);
    let text = String::from_utf8_lossy(&bytes);
    let body: Vec<&str> = text.lines().collect();
    f.added = body.len();
    if !body.is_empty() {
        f.lines.push(format!("@@ -0,0 +1,{} @@", body.len()));
        f.lines.extend(body.iter().map(|l| format!("+{l}")));
    }
    if truncated {
        f.lines.push("\\ the rest isn't shown (over 64 KiB)".into());
    }
    f
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
    fn a_patch_splits_into_files() {
        let patch = "\
diff --git a/src/x.rs b/src/x.rs
index 1..2 100644
--- a/src/x.rs
+++ b/src/x.rs
@@ -1,3 +1,3 @@ fn main() {
 keep

-old
+new
diff --git a/gone.txt b/gone.txt
deleted file mode 100644
--- a/gone.txt
+++ /dev/null
@@ -1 +0,0 @@
-bye
\\ No newline at end of file
diff --git a/old name.rs b/new name.rs
similarity index 90%
rename from old name.rs
rename to new name.rs
diff --git a/logo.png b/logo.png
new file mode 100644
Binary files /dev/null and b/logo.png differ
";
        let f = parse_patch(patch);
        assert_eq!(f.len(), 4);
        assert_eq!((f[0].path.as_str(), f[0].status), ("src/x.rs", 'M'));
        assert_eq!((f[0].added, f[0].removed), (1, 1));
        assert_eq!(f[0].lines.len(), 5, "the stripped empty context line stays");
        assert_eq!((f[1].path.as_str(), f[1].status), ("gone.txt", 'D'));
        assert_eq!(f[1].lines.last().unwrap(), "\\ No newline at end of file");
        assert_eq!(f[2].status, 'R');
        assert_eq!(
            (f[2].path.as_str(), f[2].from.as_deref()),
            ("new name.rs", Some("old name.rs"))
        );
        assert_eq!(
            (f[3].status, f[3].lines[0].as_str()),
            ('A', "\\ binary file")
        );
    }

    #[test]
    fn quoted_paths_are_unescaped() {
        assert_eq!(unquote(r#""a\"b\\c""#), r#"a"b\c"#);
        assert_eq!(unquote("plain"), "plain");
        assert_eq!(path_from_header("a/x y b/x y"), "x y");
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
        let files = diff_files(&dir);
        assert_eq!(files.len(), 2, "{files:?}");
        assert_eq!((files[0].path.as_str(), files[0].status), ("a.txt", 'M'));
        assert!(files[0].lines.iter().any(|l| l == "+two"));
        assert_eq!((files[1].path.as_str(), files[1].status), ("b.txt", '?'));
        assert_eq!(files[1].lines, ["@@ -0,0 +1,1 @@", "+new"]);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(status(Path::new("/")).is_none(), "not a repo");
    }
}
