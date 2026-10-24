//! `marina doctor`: what marina can see and do on this machine, and how to
//! get the rest — hooks, notices, the watcher, where sessions run.

use crate::hooks::Hooked;

fn line(ok: Option<bool>, what: &str, detail: &str) {
    let mark = match ok {
        Some(true) => "✓",
        Some(false) => "✗",
        None => "·",
    };
    if detail.is_empty() {
        println!("  {mark} {what}");
    } else {
        println!("  {mark} {what} — {detail}");
    }
}

pub fn run() -> i32 {
    let cfg = crate::config::load();
    let which = crate::verbs::which;

    println!("agents");
    let snap = {
        let mut s = crate::sampler::Sampler::new();
        s.build()
    };
    let n = snap.agents.len();
    let reachable = snap
        .agents
        .iter()
        .filter(|a| a.host.as_ref().is_some_and(|h| h.kind.interactive()))
        .count();
    if n == 0 {
        line(None, "no agent sessions running now", "");
    } else {
        line(
            (reachable == n).then_some(true),
            &format!("{n} session{} running", if n == 1 { "" } else { "s" }),
            &if reachable == n {
                "all in tmux or herdr: reply, approve and end them from marina".to_string()
            } else {
                format!(
                    "{reachable} in tmux or herdr; for the other {}, marina can only jump to their window (start sessions in tmux/herdr, or with N, to answer from marina)",
                    n - reachable
                )
            },
        );
    }
    for (name, found) in [("tmux", which("tmux")), ("herdr", which("herdr"))] {
        line(
            Some(found.is_some()),
            name,
            if found.is_some() {
                "found"
            } else {
                "not installed"
            },
        );
    }
    line(
        Some(cfg.agents.transcripts),
        "transcripts",
        if cfg.agents.transcripts {
            "read for what each session asked and did"
        } else {
            "off ([agents] transcripts = false): no asks, recaps or replies"
        },
    );

    println!("\nhooks — agents report their own state instead of marina inferring it");
    let mut missing = false;
    for (name, state, path) in crate::hooks::installed() {
        let path = path.map(|p| crate::ui::tildify(&p.display().to_string()));
        match state {
            Hooked::Yes => line(Some(true), name, &path.unwrap_or_default()),
            Hooked::Broken(m) => {
                missing = true;
                line(
                    Some(false),
                    name,
                    &format!(
                        "runs {}, which is gone — marina hooks --install",
                        m.display()
                    ),
                );
            }
            Hooked::No => {
                missing = true;
                line(Some(false), name, "not hooked — marina hooks --install");
            }
            Hooked::Absent => line(None, name, "not installed"),
        }
    }

    println!("\nnotices");
    line(
        Some(cfg.notify.desktop && cfg.notify.agents),
        "agent notices",
        if !cfg.notify.desktop {
            "off ([notify] desktop = false)"
        } else if !cfg.notify.agents {
            "off ([notify] agents = false)"
        } else {
            "on: when a session needs you or finishes"
        },
    );
    if cfg!(target_os = "macos") {
        let tn = which("terminal-notifier").is_some();
        line(
            Some(tn),
            "clickable notices",
            if tn {
                "terminal-notifier: a click jumps to the session"
            } else {
                "brew install terminal-notifier — without it a click does nothing"
            },
        );
    } else {
        let ns = which("notify-send").is_some();
        line(
            Some(ns),
            "notify-send",
            if ns {
                "found"
            } else {
                "not installed: no desktop notices"
            },
        );
    }
    let (installed, running) = (crate::watch::service_installed(), crate::watch::running());
    line(
        Some(running),
        "marina watch",
        match (installed, running) {
            (_, true) => "running: notices arrive with the TUI closed",
            (true, false) => "installed but not running — marina watch --status",
            (false, false) => {
                "not running: notices only while the TUI is open — marina watch --install"
            }
        },
    );
    if cfg!(target_os = "macos") {
        line(
            None,
            "jumping to Ghostty, iTerm2 or Terminal windows",
            "macOS asks once to let marina control them (System Settings › Privacy › Automation)",
        );
    }
    if missing {
        println!("\nmost useful next: marina hooks --install");
    }
    0
}
