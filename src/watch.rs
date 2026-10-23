//! `marina watch`: the notices without the TUI. It samples like the TUI and
//! sends the same desktop notifications — an agent session needs you, has
//! finished, or ended leaving servers running — so nothing is missed while
//! marina isn't open. `--install` keeps it running in the background (a
//! launchd agent on macOS, a systemd user service on Linux).
//!
//! While it runs it holds `<state dir>/watch.pid`; an open TUI sees that and
//! leaves desktop notifications to it, so nothing is sent twice.

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

const LABEL: &str = "dev.marina.watch";
const EVERY: Duration = Duration::from_secs(2);

fn pid_file() -> Option<PathBuf> {
    Some(crate::logs::state_dir()?.join("watch.pid"))
}

/// Is a watcher running? It holds an exclusive lock on `watch.pid` for its
/// whole life, so this asks the lock — not the pid inside, which can be
/// stale or reused (the TUI is named marina too).
pub fn running() -> bool {
    let Some(f) = pid_file().and_then(|p| std::fs::File::open(p).ok()) else {
        return false;
    };
    !try_lock(&f)
}

/// Take the exclusive lock without waiting; `true` if we got it. (Dropping
/// the file releases it.)
fn try_lock(f: &std::fs::File) -> bool {
    use std::os::fd::AsRawFd;
    // SAFETY: flock on a file descriptor we own.
    unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
}

/// Keep `watch.log` from growing forever: start over past 1MB.
fn cap_log() {
    if let Some(log) = crate::logs::state_dir().map(|d| d.join("watch.log")) {
        if std::fs::metadata(&log).is_ok_and(|m| m.len() > 1 << 20) {
            let _ = std::fs::write(&log, "");
        }
    }
}

/// Run the watcher in the foreground until killed.
pub fn run() -> i32 {
    let Some(p) = pid_file() else {
        eprintln!("marina watch: no state directory");
        return 1;
    };
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&p);
    let Ok(mut lock) = lock else {
        eprintln!("marina watch: can't open {}", p.display());
        return 1;
    };
    if !try_lock(&lock) {
        // another watcher holds it; exit quietly (launchd retries later)
        eprintln!("marina watch: already running");
        return 0;
    }
    {
        use std::io::Write;
        let _ = lock.set_len(0);
        let _ = write!(lock, "{}", std::process::id());
    }
    cap_log();
    let cfg = crate::config::load();
    let mut sampler = crate::sampler::Sampler::new();
    let mut app = crate::ui::App::new();
    crate::ui::load_seen(&mut app);
    app.notify_agents = cfg.notify.agents;
    eprintln!("marina watch: watching (pid {})", std::process::id());
    loop {
        app.apply(Arc::new(sampler.build()));
        app.tick(); // prunes per-row bookkeeping, as the TUI loop does
        for n in app.take_notifications() {
            eprintln!("{}", n.text);
            if cfg.notify.desktop {
                crate::verbs::notify("marina", &n.text, n.jump.as_deref());
            }
        }
        std::thread::sleep(EVERY);
    }
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn plist_path() -> Option<PathBuf> {
    Some(
        home()?
            .join("Library/LaunchAgents")
            .join(format!("{LABEL}.plist")),
    )
}

fn unit_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| Some(home()?.join(".config")))?;
    Some(base.join("systemd/user/marina-watch.service"))
}

/// XML-escape for the plist.
fn xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// The installing shell's environment the watcher needs: PATH (herdr,
/// tmux, git, terminal-notifier live there, not in launchd's default) and
/// the XDG directories, so it finds the same config and state as the TUI.
pub fn service_env() -> Vec<(String, String)> {
    ["PATH", "XDG_CONFIG_HOME", "XDG_STATE_HOME"]
        .iter()
        .filter_map(|k| Some((k.to_string(), std::env::var(k).ok()?)))
        .collect()
}

/// The launchd agent: run `marina watch` at login and keep it running.
pub fn plist(exe: &str, env: &[(String, String)], log: &str) -> String {
    let env: String = env
        .iter()
        .map(|(k, v)| format!("<key>{}</key><string>{}</string>", xml(k), xml(v)))
        .collect();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array><string>{}</string><string>watch</string></array>
  <key>EnvironmentVariables</key>
  <dict>{}</dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ProcessType</key><string>Background</string>
  <key>StandardErrorPath</key><string>{}</string>
</dict>
</plist>
"#,
        xml(exe),
        env,
        xml(log)
    )
}

/// A systemd value: quoted, with `%` specifiers and `$` expansion escaped.
fn sd(s: &str) -> String {
    format!(
        "\"{}\"",
        s.replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('%', "%%")
            .replace('$', "$$")
    )
}

pub fn unit(exe: &str, env: &[(String, String)]) -> String {
    let env: String = env
        .iter()
        .map(|(k, v)| format!("Environment={}\n", sd(&format!("{k}={v}"))))
        .collect();
    format!(
        "[Unit]\nDescription=marina watch: agent session notices\n\n\
         [Service]\nExecStart={} watch\n{env}Restart=always\n\n\
         [Install]\nWantedBy=default.target\n",
        sd(exe)
    )
}

/// Install and start the background watcher.
pub fn install() -> i32 {
    let Some(exe) = crate::verbs::stable_exe() else {
        eprintln!("marina watch: can't find marina's own path");
        return 1;
    };
    let exe = exe.display().to_string();
    let env = service_env();
    if cfg!(target_os = "macos") {
        let (Some(plist_at), Some(state)) = (plist_path(), crate::logs::state_dir()) else {
            return 1;
        };
        let log = state.join("watch.log").display().to_string();
        let _ = std::fs::create_dir_all(&state);
        if let Some(dir) = plist_at.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(e) = std::fs::write(&plist_at, plist(&exe, &env, &log)) {
            eprintln!("marina watch: writing {}: {e}", plist_at.display());
            return 1;
        }
        // SAFETY: getuid has no preconditions.
        let domain = format!("gui/{}", unsafe { libc::getuid() });
        // replace a previous install
        let _ = Command::new("launchctl")
            .args(["bootout", &format!("{domain}/{LABEL}")])
            .output();
        // bootout returns before the old job is gone; bootstrap can fail
        // until it is
        let ok = (0..10).any(|i| {
            if i > 0 {
                std::thread::sleep(std::time::Duration::from_millis(300));
            }
            Command::new("launchctl")
                .args(["bootstrap", &domain])
                .arg(&plist_at)
                .output()
                .is_ok_and(|o| o.status.success())
        });
        if !ok {
            eprintln!(
                "marina watch: launchctl couldn't start {}",
                plist_at.display()
            );
            return 1;
        }
        println!("marina watch: installed — runs at login, notices go to Notification Center");
        println!("  agent: {}", plist_at.display());
        println!("  log:   {log}");
        if crate::verbs::which("terminal-notifier").is_none() {
            println!("  tip: `brew install terminal-notifier` makes notifications clickable (jump to the session)");
        }
    } else {
        let Some(unit_at) = unit_path() else {
            return 1;
        };
        if let Some(dir) = unit_at.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(e) = std::fs::write(&unit_at, unit(&exe, &env)) {
            eprintln!("marina watch: writing {}: {e}", unit_at.display());
            return 1;
        }
        // pick up a rewritten unit, and restart one already running
        let sc = |args: &[&str]| {
            Command::new("systemctl")
                .arg("--user")
                .args(args)
                .status()
                .is_ok_and(|s| s.success())
        };
        let ok = sc(&["daemon-reload"])
            && sc(&["enable", "marina-watch.service"])
            && sc(&["restart", "marina-watch.service"]);
        if !ok {
            eprintln!(
                "marina watch: systemctl couldn't start {}",
                unit_at.display()
            );
            return 1;
        }
        println!("marina watch: installed as {}", unit_at.display());
    }
    0
}

/// Stop and remove the background watcher.
pub fn uninstall() -> i32 {
    if cfg!(target_os = "macos") {
        // SAFETY: getuid has no preconditions.
        let target = format!("gui/{}/{LABEL}", unsafe { libc::getuid() });
        let _ = Command::new("launchctl")
            .args(["bootout", &target])
            .output();
        if let Some(p) = plist_path() {
            let _ = std::fs::remove_file(p);
        }
    } else {
        let _ = Command::new("systemctl")
            .args(["--user", "disable", "--now", "marina-watch.service"])
            .status();
        if let Some(p) = unit_path() {
            let _ = std::fs::remove_file(p);
        }
    }
    println!("marina watch: uninstalled");
    0
}

/// `--status`: is it installed, and running?
/// Is the login service (launchd agent / systemd unit) installed?
pub fn service_installed() -> bool {
    if cfg!(target_os = "macos") {
        plist_path().is_some_and(|p| p.is_file())
    } else {
        unit_path().is_some_and(|p| p.is_file())
    }
}

pub fn status() -> i32 {
    let installed = service_installed();
    let run = running();
    println!(
        "marina watch: {} · {}",
        if installed {
            "installed"
        } else {
            "not installed"
        },
        if run { "running" } else { "not running" }
    );
    if run {
        0
    } else {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_files_carry_the_path_and_escape_it() {
        let env = vec![
            ("PATH".to_string(), "/a:/b&c".to_string()),
            ("XDG_STATE_HOME".to_string(), "/s".to_string()),
        ];
        let p = plist("/opt/homebrew/bin/marina", &env, "/x/watch.log");
        assert!(p.contains("<string>/opt/homebrew/bin/marina</string><string>watch</string>"));
        assert!(
            p.contains("<key>PATH</key><string>/a:/b&amp;c</string>"),
            "{p}"
        );
        assert!(p.contains("<key>XDG_STATE_HOME</key><string>/s</string>"));
        assert!(p.contains("<key>KeepAlive</key><true/>"));
        let u = unit(
            "/home/me/my bin/marina",
            &[("PATH".into(), "/usr/bin:$X%h".into())],
        );
        assert!(
            u.contains("ExecStart=\"/home/me/my bin/marina\" watch"),
            "{u}"
        );
        assert!(u.contains("Environment=\"PATH=/usr/bin:$$X%%h\""), "{u}");
    }

    #[test]
    fn the_lock_not_the_pid_says_who_is_watching() {
        let p = std::env::temp_dir().join(format!("marina-lock-{}", std::process::id()));
        std::fs::write(&p, "1").unwrap(); // a stale pid file, unlocked
        let held = std::fs::File::open(&p).unwrap();
        assert!(try_lock(&held), "nobody holds it");
        let other = std::fs::File::open(&p).unwrap();
        assert!(!try_lock(&other), "taken while the first is open");
        drop(held);
        // a child forked by a parallel test can hold a copy of the fd until
        // it execs; give it a moment
        let freed = (0..50).any(|_| {
            std::thread::sleep(std::time::Duration::from_millis(10));
            try_lock(&other)
        });
        assert!(freed, "released when the holder goes");
        let _ = std::fs::remove_file(&p);
    }
}
