//! First-class verbs on the selected target. See DESIGN.md "Verbs".
//!
//! kill: fingerprint-verified SIGTERM now, SIGKILL escalation after a grace
//! period (`u` cancels the escalation — the SIGTERM itself is already gone,
//! and the UI wording says so honestly). Signals go through `libc::kill`
//! directly, so EPERM ("not your process") is reported instead of vanishing.
//! restart: verified kill → wait for the port to actually free → re-exec with
//! stdout/stderr captured to a marina log file, and an early-exit watchdog.
//! Docker targets: `docker stop` / `docker restart` (see `docker.rs`).

use std::collections::HashMap;
use std::io::Write;
use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

use crate::msg::{SamplerCtl, UiEvent};

/// `(pid, start_time)` — the identity fingerprint carried on every Target.
pub type PidStart = (u32, u64);

/// How long SIGTERM gets before the SIGKILL escalation.
pub const GRACE: Duration = Duration::from_secs(4);

// --- signalling -------------------------------------------------------------

/// Outcome of signalling a set of pids. ESRCH (already exited) is the normal,
/// successful case and is not counted anywhere.
#[derive(Default)]
pub struct SignalOutcome {
    pub sent: usize,
    /// Pids we lacked permission to signal (EPERM) — surfaced, never swallowed.
    pub denied: Vec<u32>,
}

fn send_signal(pid: u32, sig: i32) -> Result<(), i32> {
    let r = unsafe { libc::kill(pid as libc::pid_t, sig) };
    if r == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
    }
}

/// Signal every pid in the subtree (never the out-of-boundary parent).
pub fn signal_tree(pids: &[u32], sig: i32) -> SignalOutcome {
    let mut out = SignalOutcome::default();
    for &p in pids {
        match send_signal(p, sig) {
            Ok(()) => out.sent += 1,
            Err(libc::EPERM) => out.denied.push(p),
            Err(_) => {} // ESRCH: already gone
        }
    }
    out
}

// --- fingerprint verification -----------------------------------------------

/// Pids that are still running *and* still the same process (start_time
/// unchanged since capture). Excludes exited pids and recycled pid numbers —
/// checked before EVERY signal, the first SIGTERM included, because the
/// snapshot the pids came from can be several seconds old.
fn survivors(pid_starts: &[PidStart], now: &HashMap<u32, u64>) -> Vec<u32> {
    pid_starts
        .iter()
        .filter(|(p, s)| now.get(p) == Some(s))
        .map(|&(p, _)| p)
        .collect()
}

fn verified(pid_starts: &[PidStart]) -> Vec<u32> {
    let pids: Vec<u32> = pid_starts.iter().map(|&(p, _)| p).collect();
    survivors(pid_starts, &start_times(&pids))
}

/// Current start_time per pid. Missing = not running.
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

// --- kill -------------------------------------------------------------------

/// Verified SIGTERM now; SIGKILL escalation after [`GRACE`] unless the returned
/// cancel flag is set first (`u`). Runs entirely off the UI thread except the
/// initial signal burst. Errs when every pid already exited or was recycled.
pub fn kill(
    pid_starts: Vec<PidStart>,
    events: Sender<UiEvent>,
    ctl: Sender<SamplerCtl>,
) -> Result<Arc<AtomicBool>, String> {
    let fresh = verified(&pid_starts);
    if fresh.is_empty() {
        return Err("already exited".into());
    }
    report_denied(&signal_tree(&fresh, libc::SIGTERM), &events);
    let cancel = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&cancel);
    thread::spawn(move || {
        thread::sleep(GRACE);
        if !flag.load(Ordering::SeqCst) {
            let stragglers = verified(&pid_starts);
            if !stragglers.is_empty() {
                signal_tree(&stragglers, libc::SIGKILL);
                let n = stragglers.len();
                let _ = events.send(UiEvent::Status(format!(
                    "force-killed {n} straggler{}",
                    if n == 1 { "" } else { "s" }
                )));
            }
        }
        let _ = ctl.send(SamplerCtl::Refresh);
    });
    Ok(cancel)
}

/// Synchronous verified SIGTERM → wait → verified SIGKILL, for the one-shot
/// CLI. Returns (terminated, force-killed) counts.
pub fn kill_blocking(pid_starts: &[PidStart], grace: Duration) -> (usize, usize) {
    let fresh = verified(pid_starts);
    signal_tree(&fresh, libc::SIGTERM);
    thread::sleep(grace);
    let stragglers = verified(pid_starts);
    signal_tree(&stragglers, libc::SIGKILL);
    (fresh.len(), stragglers.len())
}

fn report_denied(out: &SignalOutcome, events: &Sender<UiEvent>) {
    if !out.denied.is_empty() {
        let _ = events.send(UiEvent::Status(format!(
            "permission denied signalling pid {:?} — not your process?",
            out.denied
        )));
    }
}

// --- restart ----------------------------------------------------------------

pub struct RestartPlan {
    pub project: String,
    pub argv: Vec<String>,
    pub cwd: std::path::PathBuf,
    pub ports: Vec<u16>,
    pub pid_starts: Vec<PidStart>,
}

/// Kill the subtree (verified), wait until the ports actually free (escalating
/// to SIGKILL if SIGTERM is ignored), then re-exec the captured command in its
/// cwd with stdout/stderr captured to marina's log file — so `T` always works
/// on anything marina has restarted. An immediate crash is reported instead of
/// vanishing silently. Runs off the UI thread.
pub fn restart(plan: RestartPlan, events: Sender<UiEvent>, ctl: Sender<SamplerCtl>) {
    if plan.argv.is_empty() {
        return;
    }
    thread::spawn(move || {
        report_denied(
            &signal_tree(&verified(&plan.pid_starts), libc::SIGTERM),
            &events,
        );
        if !wait_ports_free(&plan.ports, Duration::from_secs(3)) {
            // SIGTERM ignored — escalate so the respawn doesn't hit EADDRINUSE.
            signal_tree(&verified(&plan.pid_starts), libc::SIGKILL);
            if !wait_ports_free(&plan.ports, Duration::from_secs(2)) {
                let _ = events.send(UiEvent::Status(format!(
                    "restart {}: port still busy — respawning anyway",
                    plan.project
                )));
            }
        }
        let log = crate::logs::state_log_path(&plan.project);
        match respawn(&plan.argv, &plan.cwd, log.as_deref()) {
            Ok(child) => {
                let _ = events.send(UiEvent::Status(format!(
                    "restarted {} — output captured (T to tail)",
                    plan.project
                )));
                let _ = ctl.send(SamplerCtl::Refresh);
                watch_child(child, plan.project, events);
            }
            Err(e) => {
                let _ = events.send(UiEvent::Status(format!(
                    "restart {} failed: {e}",
                    plan.project
                )));
                let _ = ctl.send(SamplerCtl::Refresh);
            }
        }
    });
}

/// True once every port accepts a bind (i.e. the old server released it).
fn wait_ports_free(ports: &[u16], timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        let busy = ports
            .iter()
            .any(|&p| TcpListener::bind(("127.0.0.1", p)).is_err());
        if !busy {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(150));
    }
}

/// Reap the child (no zombie while marina runs) and flag an immediate crash —
/// a respawn that dies within 2s would otherwise just silently not appear.
fn watch_child(mut child: Child, project: String, events: Sender<UiEvent>) {
    let started = Instant::now();
    let status = child.wait();
    if started.elapsed() < Duration::from_secs(2) {
        let what = status
            .map(|s| s.to_string())
            .unwrap_or_else(|_| "unknown".into());
        let _ = events.send(UiEvent::Status(format!(
            "restart {project} crashed on startup ({what}) — T for its log"
        )));
    }
}

/// Spawn a captured command, detached, in its cwd. With a `log` path, stdout
/// and stderr are appended there (the marina-capture file `T` discovers).
pub fn respawn(argv: &[String], cwd: &Path, log: Option<&Path>) -> std::io::Result<Child> {
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]).current_dir(cwd).stdin(Stdio::null());
    match log.map(log_sink) {
        Some(Ok((out, err))) => {
            cmd.stdout(out).stderr(err);
        }
        _ => {
            cmd.stdout(Stdio::null()).stderr(Stdio::null());
        }
    }
    cmd.spawn()
}

fn log_sink(path: &Path) -> std::io::Result<(Stdio, Stdio)> {
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let _ = writeln!(f, "--- marina restart (unix {ts}) ---");
    let f2 = f.try_clone()?;
    Ok((Stdio::from(f), Stdio::from(f2)))
}

// --- docker -----------------------------------------------------------------

/// Run `docker <action> <name>` for each container, off-thread, reporting each
/// outcome. `action` is "stop" or "restart".
pub fn docker(
    action: &'static str,
    names: Vec<String>,
    events: Sender<UiEvent>,
    ctl: Sender<SamplerCtl>,
) {
    thread::spawn(move || {
        for name in &names {
            let ok = Command::new("docker")
                .args([action, name.as_str()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            let _ = events.send(UiEvent::Status(if ok {
                format!("docker {action} {name}: done")
            } else {
                format!("docker {action} {name} failed — daemon running?")
            }));
        }
        let _ = ctl.send(SamplerCtl::Refresh);
    });
}

// --- copy / open ------------------------------------------------------------

/// Copy to the clipboard via the platform's tool: `pbcopy` on macOS, and
/// `wl-copy` (Wayland) or `xclip` (X11) on Linux — whichever is present.
pub fn copy_url(url: &str) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    let candidates: &[&[&str]] = &[&["pbcopy"]];
    #[cfg(not(target_os = "macos"))]
    let candidates: &[&[&str]] = &[&["wl-copy"], &["xclip", "-selection", "clipboard"]];

    for cmd in candidates {
        let (prog, args) = cmd.split_first().expect("clipboard command not empty");
        let mut c = Command::new(prog);
        c.args(args).stdin(Stdio::piped());
        if let Ok(mut child) = c.spawn() {
            if let Some(mut stdin) = child.stdin.take() {
                stdin.write_all(url.as_bytes())?;
            }
            child.wait()?;
            return Ok(());
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "no clipboard tool found (pbcopy / wl-copy / xclip)",
    ))
}

/// Open a URL in the default browser: `open` on macOS, `xdg-open` on Linux.
pub fn open_url(url: &str) -> std::io::Result<()> {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    Command::new(opener).arg(url).spawn()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn survivors_excludes_exited_and_recycled_pids() {
        let captured: Vec<PidStart> = vec![(10, 100), (20, 200), (30, 300)];
        // 10 still same; 20 exited (absent); 30's number recycled (new start_time)
        let now = HashMap::from([(10, 100u64), (30, 999)]);
        assert_eq!(survivors(&captured, &now), vec![10]);
    }

    #[test]
    fn wait_ports_free_returns_fast_when_free_and_detects_busy() {
        // an unbound high port is free immediately
        assert!(wait_ports_free(&[0], Duration::from_millis(100)));
        // a port we hold stays busy until the timeout
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let p = l.local_addr().unwrap().port();
        assert!(!wait_ports_free(&[p], Duration::from_millis(200)));
        drop(l);
    }
}
