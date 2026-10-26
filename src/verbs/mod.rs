//! First-class verbs on the selected target. See DESIGN.md "Verbs".
//!
//! kill: fingerprint-verified SIGTERM. In the TUI escalation is explicit —
//! a server still running after [`GRACE`] is reported, and `X` again sends
//! SIGKILL — so a slow, clean shutdown (postgres waiting on clients) is never
//! cut short. The one-shot CLI escalates by itself after `--grace`. Signals
//! go through `libc::kill` directly, so EPERM ("not your process") is
//! reported instead of vanishing.
//! restart: verified kill → wait for the port to actually free → re-exec with
//! stdout/stderr captured to a marina log file, and an early-exit watchdog.
//! Docker targets: `docker stop` / `docker restart` (see `docker.rs`).

use std::collections::HashMap;
use std::io::Write;
use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::Sender;
use std::thread;
use std::time::{Duration, Instant};

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

use crate::msg::{SamplerCtl, UiEvent};

/// `(pid, start_time)` — the identity fingerprint carried on every Target.
pub type PidStart = (u32, u64);

/// How long SIGTERM gets before the TUI says a server is still running.
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

/// Verified SIGTERM now. If anything is still running after [`GRACE`], the
/// status line says so and names the key that force-kills it; nothing is
/// escalated on its own. Errs when every pid already exited or was recycled.
pub fn kill(
    pid_starts: Vec<PidStart>,
    label: String,
    events: Sender<UiEvent>,
    ctl: Sender<SamplerCtl>,
) -> Result<(), String> {
    let fresh = verified(&pid_starts);
    if fresh.is_empty() {
        return Err("already exited".into());
    }
    report_denied(&signal_tree(&fresh, libc::SIGTERM), &events);
    thread::spawn(move || {
        thread::sleep(GRACE);
        if !verified(&pid_starts).is_empty() {
            let _ = events.send(UiEvent::Status(format!(
                "X again to force-kill {label} (still running after {}s)",
                GRACE.as_secs()
            )));
        }
        let _ = ctl.send(SamplerCtl::Refresh);
    });
    Ok(())
}

/// `X` on a server that's still stopping: verified SIGKILL now.
pub fn force_kill(
    pid_starts: Vec<PidStart>,
    label: String,
    events: Sender<UiEvent>,
    ctl: Sender<SamplerCtl>,
) {
    thread::spawn(move || {
        let out = signal_tree(&verified(&pid_starts), libc::SIGKILL);
        report_denied(&out, &events);
        if out.denied.is_empty() {
            let _ = events.send(UiEvent::Status(format!("force-killed {label}")));
        }
        let _ = ctl.send(SamplerCtl::Refresh);
    });
}

/// What [`kill_blocking`] did, per pid.
#[derive(Default, Debug)]
pub struct KillReport {
    /// Not ours to signal (EPERM).
    pub denied: Vec<u32>,
    /// Still running after the grace period, so sent SIGKILL.
    pub forced: Vec<u32>,
    /// Still running even after SIGKILL.
    pub survivors: Vec<u32>,
}

/// Synchronous verified SIGTERM → wait → verified SIGKILL, for the one-shot
/// CLI. Returns as soon as everything has exited; only processes still alive
/// after `grace` are force-killed.
pub fn kill_blocking(pid_starts: &[PidStart], grace: Duration) -> KillReport {
    let fresh = verified(pid_starts);
    let mut denied = signal_tree(&fresh, libc::SIGTERM).denied;
    let deadline = Instant::now() + grace;
    let mut stragglers = verified(pid_starts);
    while !stragglers.is_empty() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
        stragglers = verified(pid_starts);
    }
    stragglers.retain(|p| !denied.contains(p));
    let out = signal_tree(&stragglers, libc::SIGKILL);
    denied.extend(out.denied);
    // SIGKILL can't be ignored, but it isn't instant
    let deadline = Instant::now() + Duration::from_millis(500);
    let mut survivors = verified(pid_starts);
    while !survivors.is_empty() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
        survivors = verified(pid_starts);
    }
    survivors.retain(|p| !denied.contains(p));
    KillReport {
        denied,
        forced: stragglers,
        survivors,
    }
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
    /// Say, once, that the new process doesn't get the old one's environment.
    pub note_env: bool,
}

/// Kill the subtree (verified), wait until the ports actually free (escalating
/// to SIGKILL if SIGTERM is ignored), then re-exec the captured command in its
/// cwd with stdout/stderr captured to marina's log file — so `t` always works
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
                let _ = events.send(UiEvent::Status(if plan.note_env {
                    format!(
                        "t shows its output · restarted {} without its original environment",
                        plan.project
                    )
                } else {
                    format!("t shows its output · restarted {}", plan.project)
                }));
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
            "t for its log · restart {project} crashed on startup ({what})"
        )));
    }
}

/// Spawn a captured command, detached, in its cwd. With a `log` path, stdout
/// and stderr are appended there (the marina-capture file `t` discovers).
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

/// Fire-and-forget desktop notification: `osascript` on macOS, `notify-send`
/// on Linux. Runs on its own thread (and reaps the child); failures — no
/// notification daemon, headless box — are silently ignored.
pub fn notify(title: &str, body: &str) {
    let mut cmd = if cfg!(target_os = "macos") {
        let mut c = Command::new("osascript");
        c.arg("-e").arg(format!(
            "display notification {} with title {}",
            applescript_str(body),
            applescript_str(title)
        ));
        c
    } else {
        let mut c = Command::new("notify-send");
        c.arg(title).arg(body);
        c
    };
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    thread::spawn(move || {
        let _ = cmd.status();
    });
}

/// An AppleScript string literal. Text comes from process/project names, so
/// quotes and backslashes are escaped — it must never break out of the literal.
fn applescript_str(s: &str) -> String {
    let escaped: String = s
        .chars()
        .filter(|c| !c.is_control())
        .flat_map(|c| match c {
            '"' | '\\' => vec!['\\', c],
            _ => vec![c],
        })
        .collect();
    format!("\"{escaped}\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applescript_strings_cannot_break_out() {
        assert_eq!(applescript_str("plain"), "\"plain\"");
        assert_eq!(
            applescript_str("a\" & do shell script \"x"),
            "\"a\\\" & do shell script \\\"x\""
        );
        assert_eq!(applescript_str("back\\slash"), "\"back\\\\slash\"");
        assert_eq!(applescript_str("new\nline"), "\"newline\"");
    }

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
