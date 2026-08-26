//! Best-effort log discovery + tailing for the `T` verb. See DESIGN.md.
//!
//! macOS gives no general way to attach to a running process's stdout, so we
//! look at its open file descriptors (via `lsof`) for a `.log`-ish regular file
//! and tail that. Many dev servers log to stdout instead — but anything marina
//! itself restarts gets stdout/stderr captured to the state dir (below), so
//! those always tail. Docker targets tail via `docker logs -f` (`tail_cmd`).
//!
//! Discovery shells out to `lsof`, which can stall — callers run it off the
//! UI thread and receive the result as a `UiEvent::LogReady`.

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

/// Find a log file for a target, in order of confidence:
/// 1. a `.log` held open by a pid in the subtree (`lsof`),
/// 2. marina's own capture from a previous restart of this project,
/// 3. a `*.log` in the project dir or its `logs/` subdir,
/// 4. a pm2 log matching the project name.
pub fn discover(pids: &[u32], cwd: &Path, project: &str) -> Option<PathBuf> {
    from_fds(pids)
        .or_else(|| captured(project))
        .or_else(|| in_dir(cwd))
        .or_else(|| pm2_log(project))
}

/// `$XDG_STATE_HOME/marina/logs/<project>.log` (or `~/.local/state/…`) — where
/// marina captures stdout/stderr of processes it restarts. Creates the dir.
pub fn state_log_path(project: &str) -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
    let dir = base.join("marina").join("logs");
    std::fs::create_dir_all(&dir).ok()?;
    let safe: String = project
        .chars()
        .map(|c| {
            if matches!(c, '/' | '\\' | ' ') {
                '-'
            } else {
                c
            }
        })
        .collect();
    Some(dir.join(format!("{safe}.log")))
}

/// A previously-captured marina log for this project, if one exists.
fn captured(project: &str) -> Option<PathBuf> {
    if project.is_empty() {
        return None;
    }
    let p = state_log_path(project)?;
    p.exists().then_some(p)
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
/// Survives rotation/truncation (restarts from the top when the file shrinks).
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
                Ok(0) => {
                    // EOF. If the file shrank below our position it was
                    // rotated/truncated — start over instead of waiting forever.
                    let pos = reader.stream_position().unwrap_or(0);
                    match std::fs::metadata(&path) {
                        Ok(m) if m.len() < pos => {
                            let _ = reader.seek(SeekFrom::Start(0));
                            let _ = tx.send("— log truncated, restarting tail —".into());
                        }
                        _ => thread::sleep(Duration::from_millis(400)),
                    }
                }
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

/// Tail the stdout+stderr of a long-running command (e.g. `docker logs -f`).
/// The child is killed when `stop` is set; both streams merge into one channel.
pub fn tail_cmd(mut cmd: Command, stop: Arc<AtomicBool>) -> Receiver<String> {
    let (tx, rx) = mpsc::channel();
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    thread::spawn(move || {
        let Ok(mut child) = cmd.spawn() else {
            let _ = tx.send("(cannot run command)".into());
            return;
        };
        let mut readers = Vec::new();
        if let Some(out) = child.stdout.take() {
            readers.push(spawn_reader(out, tx.clone()));
        }
        if let Some(err) = child.stderr.take() {
            readers.push(spawn_reader(err, tx.clone()));
        }
        loop {
            if let Ok(Some(_)) = child.try_wait() {
                break; // command ended on its own
            }
            if stop.load(Ordering::Relaxed) {
                let _ = child.kill();
                let _ = child.wait();
                break;
            }
            thread::sleep(Duration::from_millis(300));
        }
        for r in readers {
            let _ = r.join();
        }
    });
    rx
}

fn spawn_reader(stream: impl Read + Send + 'static, tx: Sender<String>) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        for line in BufReader::new(stream).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    })
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
    fn state_log_path_is_stable_and_sanitized() {
        let p = state_log_path("scope/app one").expect("state dir");
        assert!(p.ends_with("marina/logs/scope-app-one.log"));
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
        // Tests run in parallel and another test may also hold a `.log` open, so
        // assert the discovery mechanism (a held-open `.log` is found), not which.
        let found = found.expect("from_fds should discover a held-open .log via lsof");
        assert_eq!(found.extension().and_then(|e| e.to_str()), Some("log"));
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

    #[test]
    fn tail_recovers_from_truncation() {
        use std::io::Write;
        let path = std::env::temp_dir().join(format!("marina-trunc-{}.log", std::process::id()));
        let mut f = std::fs::File::create(&path).unwrap();
        // longer than the replacement content, so the shrink is detectable
        writeln!(f, "old line one is quite long").unwrap();
        writeln!(f, "old line two is quite long").unwrap();
        f.flush().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let rx = tail(path.clone(), Arc::clone(&stop));
        // wait for the pre-existing lines
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        let mut seen = Vec::new();
        while std::time::Instant::now() < deadline
            && !seen.iter().any(|l: &String| l.starts_with("old line two"))
        {
            while let Ok(l) = rx.try_recv() {
                seen.push(l);
            }
            std::thread::sleep(Duration::from_millis(30));
        }
        // rotate: truncate and write fresh content
        let mut f = std::fs::File::create(&path).unwrap(); // truncates
        writeln!(f, "fresh line").unwrap();
        f.flush().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(4);
        while std::time::Instant::now() < deadline
            && !seen.iter().any(|l: &String| l == "fresh line")
        {
            while let Ok(l) = rx.try_recv() {
                seen.push(l);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        stop.store(true, Ordering::Relaxed);
        let _ = std::fs::remove_file(&path);
        assert!(
            seen.iter().any(|l| l == "fresh line"),
            "tail should recover after truncation; saw {seen:?}"
        );
    }
}
