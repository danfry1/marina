//! First-class verbs on the selected target. See DESIGN.md "Verbs".
//!
//! kill (SIGTERM now, SIGKILL escalation after a grace period), restart
//! (capture argv+cwd, kill subtree, re-exec off-thread), copy, open are wired.
//! Inline log tailing is still a follow-up.

pub use crate::msg::Verb;

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

/// Send a signal to every pid in the subtree (never the out-of-boundary parent).
pub fn signal_tree(pids: &[u32], signal: &str) -> std::io::Result<()> {
    if pids.is_empty() {
        return Ok(());
    }
    let mut cmd = Command::new("kill");
    cmd.arg(format!("-{signal}"));
    for p in pids {
        cmd.arg(p.to_string());
    }
    // Suppress "No such process" noise — a process exiting before escalation is
    // the normal, successful case.
    cmd.stderr(Stdio::null());
    cmd.status()?;
    Ok(())
}

/// SIGTERM now; escalate to SIGKILL after a grace period unless the returned
/// cancel flag is set first (the `u` undo). The escalation only signals pids
/// whose start_time is unchanged — so a recycled pid (a *different* process that
/// reused the number) is never hit. No-op if the process already exited.
pub fn kill(pids: Vec<u32>) -> std::io::Result<Arc<AtomicBool>> {
    signal_tree(&pids, "TERM")?;
    let fingerprint = start_times(&pids);
    let cancel = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&cancel);
    thread::spawn(move || {
        thread::sleep(Duration::from_secs(4));
        if !flag.load(Ordering::SeqCst) {
            escalate(&pids, &fingerprint);
        }
    });
    Ok(cancel)
}

/// Synchronous SIGTERM → wait → guarded SIGKILL, for the one-shot CLI.
pub fn kill_blocking(pids: &[u32], grace: Duration) {
    let _ = signal_tree(pids, "TERM");
    let fingerprint = start_times(pids);
    thread::sleep(grace);
    escalate(pids, &fingerprint);
}

/// SIGKILL only the pids still alive *and* still the same process as when we
/// captured `fingerprint` (start_time match). Skips exited and recycled pids.
fn escalate(pids: &[u32], fingerprint: &HashMap<u32, u64>) {
    let now = start_times(pids);
    let _ = signal_tree(&survivors(pids, fingerprint, &now), "KILL");
}

/// Pids worth SIGKILLing: still running *and* the same process (start_time
/// unchanged since `captured`). Excludes exited pids and recycled ones.
fn survivors(pids: &[u32], captured: &HashMap<u32, u64>, now: &HashMap<u32, u64>) -> Vec<u32> {
    pids.iter()
        .copied()
        .filter(|p| matches!((captured.get(p), now.get(p)), (Some(a), Some(b)) if a == b))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn survivors_excludes_exited_and_recycled_pids() {
        let pids = [10, 20, 30];
        let captured = HashMap::from([(10, 100u64), (20, 200), (30, 300)]);
        // 10 still same; 20 exited (absent); 30's number recycled (new start_time)
        let now = HashMap::from([(10, 100u64), (30, 999)]);
        assert_eq!(survivors(&pids, &captured, &now), vec![10]);
    }
}

/// Current start_time per pid (the identity fingerprint). Missing = not running.
fn start_times(pids: &[u32]) -> HashMap<u32, u64> {
    let wanted: Vec<Pid> = pids.iter().map(|&p| Pid::from_u32(p)).collect();
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&wanted),
        true,
        ProcessRefreshKind::everything(),
    );
    pids.iter()
        .filter_map(|&p| sys.process(Pid::from_u32(p)).map(|pr| (p, pr.start_time())))
        .collect()
}

/// Kill the subtree, wait for the port to free, then re-exec the captured
/// command in its cwd. Runs off the UI thread.
pub fn restart(argv: Vec<String>, cwd: PathBuf, pids: Vec<u32>) {
    if argv.is_empty() {
        return;
    }
    thread::spawn(move || {
        let _ = signal_tree(&pids, "TERM");
        thread::sleep(Duration::from_millis(1200));
        let _ = respawn(&argv, &cwd);
    });
}

/// Spawn a captured command, detached, in its cwd.
pub fn respawn(argv: &[String], cwd: &Path) -> std::io::Result<()> {
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    cmd.spawn()?;
    Ok(())
}

pub fn copy_url(url: &str) -> std::io::Result<()> {
    let mut child = Command::new("pbcopy").stdin(Stdio::piped()).spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(url.as_bytes())?;
    }
    child.wait()?;
    Ok(())
}

pub fn open_url(url: &str) -> std::io::Result<()> {
    Command::new("open").arg(url).spawn()?;
    Ok(())
}
