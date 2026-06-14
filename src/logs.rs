//! Best-effort log discovery + tailing for the `T` verb. See DESIGN.md.
//!
//! macOS gives no general way to attach to a running process's stdout, so we
//! look at its open file descriptors (via `lsof`) for a `.log`-ish regular file
//! and tail that. Many dev servers log to stdout instead — then discovery
//! returns `None` and the UI says so honestly.

use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

/// Find a log file for a target, in order of confidence:
/// 1. a `.log` held open by a pid in the subtree (`lsof`),
/// 2. a `*.log` in the project dir or its `logs/` subdir,
/// 3. a pm2 log matching the project name.
pub fn discover(pids: &[u32], cwd: &Path, project: &str) -> Option<PathBuf> {
    from_fds(pids)
        .or_else(|| in_dir(cwd))
        .or_else(|| pm2_log(project))
}

fn from_fds(pids: &[u32]) -> Option<PathBuf> {
    if pids.is_empty() {
        return None;
    }
    let list = pids
        .iter()
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let out = Command::new("lsof")
        .args(["-p", &list, "-Fn"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut candidates: Vec<PathBuf> = text
        .lines()
        .filter_map(|l| l.strip_prefix('n'))
        .map(PathBuf::from)
        .filter(|p| is_logish(p))
        .collect();
    candidates.sort_by_key(|p| !has_log_ext(p)); // real `.log` files first
    candidates.into_iter().next()
}

/// Newest `*.log` directly in `cwd` or `cwd/logs/`.
fn in_dir(cwd: &Path) -> Option<PathBuf> {
    let mut logs: Vec<(std::time::SystemTime, PathBuf)> = [cwd.to_path_buf(), cwd.join("logs")]
        .iter()
        .filter_map(|d| std::fs::read_dir(d).ok())
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| has_log_ext(p))
        .filter_map(|p| {
            let mtime = std::fs::metadata(&p).and_then(|m| m.modified()).ok()?;
            Some((mtime, p))
        })
        .collect();
    logs.sort_by_key(|b| std::cmp::Reverse(b.0)); // newest first
    logs.into_iter().next().map(|(_, p)| p)
}

/// pm2 keeps logs at `~/.pm2/logs/<name>-out.log`; match loosely on project.
fn pm2_log(project: &str) -> Option<PathBuf> {
    if project.is_empty() {
        return None;
    }
    let dir = PathBuf::from(std::env::var_os("HOME")?).join(".pm2/logs");
    let entries = std::fs::read_dir(dir).ok()?;
    entries.flatten().map(|e| e.path()).find(|p| {
        let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
        name.contains(project) && name.contains("out")
    })
}

fn has_log_ext(p: &Path) -> bool {
    p.extension().is_some_and(|e| e == "log")
}

fn is_logish(p: &Path) -> bool {
    let s = p.to_string_lossy();
    // A real path (absolute) that looks like a log, never a socket/pipe entry.
    s.starts_with('/') && (has_log_ext(p) || s.contains("/log/") || s.contains("/logs/"))
}

/// Tail a file: emit a tail of existing content, then follow appended lines.
/// The thread exits when `stop` is set (pane closed) or the receiver is dropped.
pub fn tail(path: PathBuf, stop: Arc<AtomicBool>) -> Receiver<String> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let Ok(file) = File::open(&path) else {
            let _ = tx.send(format!("(cannot open {})", path.display()));
            return;
        };
        let len = file.metadata().map(|m| m.len()).unwrap_or(0);
        let start = len.saturating_sub(16 * 1024); // last ~16KB for context
        let mut reader = BufReader::new(file);
        if start > 0 {
            let _ = reader.seek(SeekFrom::Start(start));
            let mut partial = String::new();
            let _ = reader.read_line(&mut partial); // drop the split first line
        }
        loop {
            if stop.load(Ordering::Relaxed) {
                break; // pane closed — don't linger on a quiet log
            }
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => thread::sleep(Duration::from_millis(400)), // EOF: wait for more
                Ok(_) => {
                    if tx.send(line.trim_end().to_string()).is_err() {
                        break; // UI closed the pane
                    }
                }
                Err(_) => break,
            }
        }
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_newest_log_in_dir() {
        let dir = std::env::temp_dir().join(format!("pm-logtest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("logs")).unwrap();
        std::fs::write(dir.join("app.log"), "x").unwrap();
        std::fs::write(dir.join("notes.txt"), "x").unwrap(); // ignored (not .log)
        assert_eq!(in_dir(&dir), Some(dir.join("app.log")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn is_logish_filters_sockets_and_non_logs() {
        assert!(is_logish(Path::new("/var/log/app.log")));
        assert!(is_logish(Path::new("/srv/app/logs/out.log")));
        assert!(!is_logish(Path::new("/x/config.toml")));
        assert!(!is_logish(Path::new("*:4321"))); // socket entry, not a path
    }

    #[test]
    fn from_fds_finds_a_log_we_hold_open() {
        use std::fs::OpenOptions;
        let path = std::env::temp_dir().join(format!("marina-fd-{}.log", std::process::id()));
        let _f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .unwrap(); // keep the fd open for the duration
        let found = from_fds(&[std::process::id()]);
        let _ = std::fs::remove_file(&path);
        let found = found.expect("our open .log should be discovered via lsof");
        assert!(found
            .to_string_lossy()
            .ends_with(&format!("marina-fd-{}.log", std::process::id())));
    }

    #[test]
    fn tail_follows_appended_lines() {
        use std::io::Write;
        let path = std::env::temp_dir().join(format!("marina-tail-{}.log", std::process::id()));
        let mut f = std::fs::File::create(&path).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let rx = tail(path.clone(), Arc::clone(&stop));
        writeln!(f, "hello world").unwrap();
        f.flush().unwrap();
        // poll briefly for the line
        let mut got = None;
        for _ in 0..40 {
            if let Ok(line) = rx.try_recv() {
                got = Some(line);
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        stop.store(true, Ordering::Relaxed);
        let _ = std::fs::remove_file(&path);
        assert_eq!(got.as_deref(), Some("hello world"));
    }
}
