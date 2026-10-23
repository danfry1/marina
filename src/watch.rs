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

/// Is a watcher running (other than this process)?
pub fn running() -> bool {
    let Some(pid) = pid_file()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| s.trim().parse::<u32>().ok())
    else {
        return false;
    };
    pid != std::process::id() && alive_marina(pid)
}

/// `pid` is a live process named marina (not a recycled pid).
fn alive_marina(pid: u32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    if unsafe { libc::kill(pid as libc::pid_t, 0) } != 0 {
        return false;
    }
    let name = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "comm="])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    name.trim().rsplit('/').next() == Some("marina")
}

/// Run the watcher in the foreground until killed.
pub fn run() -> i32 {
    if running() {
        eprintln!("marina watch: already running");
        return 1;
    }
    if let Some(p) = pid_file() {
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(&p, std::process::id().to_string());
    }
    let cfg = crate::config::load();
    let mut sampler = crate::sampler::Sampler::new();
    let mut app = crate::ui::App::new();
    crate::ui::load_seen(&mut app);
    app.notify_agents = cfg.notify.agents;
    eprintln!("marina watch: watching (pid {})", std::process::id());
    loop {
        app.apply(Arc::new(sampler.build()));
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

/// The launchd agent: run `marina watch` at login and keep it running, with
/// the installing shell's PATH (herdr, tmux, git, terminal-notifier live
/// there, not in launchd's default).
pub fn plist(exe: &str, path_env: &str, log: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array><string>{}</string><string>watch</string></array>
  <key>EnvironmentVariables</key>
  <dict><key>PATH</key><string>{}</string></dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ProcessType</key><string>Background</string>
  <key>StandardErrorPath</key><string>{}</string>
</dict>
</plist>
"#,
        xml(exe),
        xml(path_env),
        xml(log)
    )
}

pub fn unit(exe: &str, path_env: &str) -> String {
    format!(
        "[Unit]\nDescription=marina watch: agent session notices\n\n\
         [Service]\nExecStart={exe} watch\nEnvironment=PATH={path_env}\nRestart=always\n\n\
         [Install]\nWantedBy=default.target\n"
    )
}

/// Install and start the background watcher.
pub fn install() -> i32 {
    let Some(exe) = crate::verbs::stable_exe() else {
        eprintln!("marina watch: can't find marina's own path");
        return 1;
    };
    let exe = exe.display().to_string();
    let path_env = std::env::var("PATH").unwrap_or_default();
    if cfg!(target_os = "macos") {
        let (Some(plist_at), Some(state)) = (plist_path(), crate::logs::state_dir()) else {
            return 1;
        };
        let log = state.join("watch.log").display().to_string();
        let _ = std::fs::create_dir_all(&state);
        if let Some(dir) = plist_at.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(e) = std::fs::write(&plist_at, plist(&exe, &path_env, &log)) {
            eprintln!("marina watch: writing {}: {e}", plist_at.display());
            return 1;
        }
        // SAFETY: getuid has no preconditions.
        let domain = format!("gui/{}", unsafe { libc::getuid() });
        // replace a previous install
        let _ = Command::new("launchctl")
            .args(["bootout", &format!("{domain}/{LABEL}")])
            .output();
        let ok = Command::new("launchctl")
            .args(["bootstrap", &domain])
            .arg(&plist_at)
            .status()
            .is_ok_and(|s| s.success());
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
        if let Err(e) = std::fs::write(&unit_at, unit(&exe, &path_env)) {
            eprintln!("marina watch: writing {}: {e}", unit_at.display());
            return 1;
        }
        let ok = Command::new("systemctl")
            .args(["--user", "enable", "--now", "marina-watch.service"])
            .status()
            .is_ok_and(|s| s.success());
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
pub fn status() -> i32 {
    let installed = if cfg!(target_os = "macos") {
        plist_path().is_some_and(|p| p.is_file())
    } else {
        unit_path().is_some_and(|p| p.is_file())
    };
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
        let p = plist("/opt/homebrew/bin/marina", "/a:/b&c", "/x/watch.log");
        assert!(p.contains("<string>/opt/homebrew/bin/marina</string><string>watch</string>"));
        assert!(p.contains("<string>/a:/b&amp;c</string>"), "{p}");
        assert!(p.contains("<key>KeepAlive</key><true/>"));
        let u = unit("/usr/bin/marina", "/usr/bin");
        assert!(u.contains("ExecStart=/usr/bin/marina watch"));
        assert!(u.contains("Environment=PATH=/usr/bin"));
    }

    #[test]
    fn a_dead_or_foreign_pid_is_not_a_watcher() {
        assert!(!alive_marina(u32::MAX - 7));
        assert!(!alive_marina(1), "launchd/init isn't marina");
    }
}
