//! Non-interactive CLI — the resolution engine exposed for scripts and agents.
//!
//!   port-manager ls [--json]        list targets (table, or JSON for agents)
//!   port-manager kill <selector>…   SIGTERM then SIGKILL matching targets
//!   port-manager restart <selector> restart matching targets (re-exec in cwd)
//!   port-manager url <selector>…    print matching targets' URLs
//!
//! A <selector> matches by project name (exact or substring, case-insensitive),
//! by port (`3000` or `:3000`), or by command label. Killing a project name
//! takes down every target under it — the grouping primitive, via the CLI.

use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::model::{Snapshot, Target, TargetKind};
use crate::sampler::Sampler;
use crate::verbs;

pub const USAGE: &str = "\
marina — developer-process cockpit

USAGE:
    marina                 launch the TUI
    marina ls [--json]     list running dev targets
    marina kill <sel>...   stop matching targets (SIGTERM -> SIGKILL)
    marina restart <sel>   restart matching targets
    marina url <sel>...    print matching targets' URLs

SELECTOR:
    a project name (exact or substring), a port (3000 or :3000), or a command.
";

/// Dispatch a CLI subcommand. Returns `Some(exit_code)` if it handled a
/// subcommand, `None` if there was none (caller should launch the TUI).
/// Exit codes: 0 ok, 1 no match, 2 usage error — so scripts/agents can branch.
pub fn dispatch(args: &[String]) -> Option<i32> {
    let cmd = args.first()?.as_str();
    let rest = &args[1..];
    let selectors: Vec<&str> = rest
        .iter()
        .map(|s| s.as_str())
        .filter(|s| !s.starts_with('-'))
        .collect();
    let code = match cmd {
        "ls" => {
            ls(rest.iter().any(|a| a == "--json"));
            0
        }
        "kill" => kill(&selectors),
        "restart" => restart(&selectors),
        "url" => url(&selectors),
        "help" | "--help" | "-h" => {
            print!("{USAGE}");
            0
        }
        _ => return None, // not a subcommand (e.g. --dump, or TUI)
    };
    Some(code)
}

// --- snapshots --------------------------------------------------------------

/// One snapshot. `with_cpu` builds twice so CPU deltas are meaningful.
fn snapshot(with_cpu: bool) -> Snapshot {
    let mut s = Sampler::new();
    let snap = s.build();
    if with_cpu {
        thread::sleep(Duration::from_millis(500));
        s.build()
    } else {
        snap
    }
}

fn select<'a>(snap: &'a Snapshot, selectors: &[&str]) -> Vec<&'a Target> {
    snap.targets
        .iter()
        .filter(|t| selectors.iter().any(|s| matches(t, s)))
        .collect()
}

fn matches(t: &Target, sel: &str) -> bool {
    let s = sel.trim_start_matches(':');
    if let Ok(port) = s.parse::<u16>() {
        if t.ports.contains(&port) {
            return true;
        }
    }
    let sel = sel.to_lowercase();
    t.project.to_lowercase().contains(&sel) || t.command_label.to_lowercase().contains(&sel)
}

// --- handlers ---------------------------------------------------------------

fn ls(json: bool) {
    let snap = snapshot(true);
    if json {
        let view: Vec<TargetJson> = snap.targets.iter().map(TargetJson::from).collect();
        match serde_json::to_string_pretty(&view) {
            Ok(s) => println!("{s}"),
            Err(e) => eprintln!("marina: json error: {e}"),
        }
        return;
    }
    if snap.targets.is_empty() {
        println!("no dev targets running");
        return;
    }
    println!(
        "{:<20} {:<16} {:<7} {:>6} {:>8} {:<14}",
        "PROJECT", "COMMAND", "PORT", "CPU", "MEM", "URL"
    );
    for t in &snap.targets {
        let port = t
            .ports
            .first()
            .map(|p| format!(":{p}"))
            .unwrap_or_else(|| "—".into());
        let (cpu, mem) = if t.pids.is_empty() {
            ("—".into(), "—".into())
        } else {
            (
                format!("{:.1}%", t.cpu_pct),
                format!("{}MB", t.mem_bytes / (1024 * 1024)),
            )
        };
        let url = t.url.as_ref().map(|u| u.value.as_str()).unwrap_or("");
        println!(
            "{:<20} {:<16} {:<7} {:>6} {:>8} {:<14}",
            t.project, t.command_label, port, cpu, mem, url
        );
    }
}

fn kill(selectors: &[&str]) -> i32 {
    if selectors.is_empty() {
        eprintln!("kill: need a selector (project, port, or command)");
        return 2;
    }
    let snap = snapshot(false);
    let targets = select(&snap, selectors);
    if targets.is_empty() {
        eprintln!("no targets match {selectors:?}");
        return 1;
    }
    let mut pids: Vec<u32> = Vec::new();
    for t in &targets {
        let port = t.ports.first().map(|p| format!(":{p}")).unwrap_or_default();
        println!("killing {} {} ({} pids)", t.project, port, t.pids.len());
        pids.extend(&t.pids);
    }
    verbs::kill_blocking(&pids, Duration::from_millis(1500)); // SIGTERM -> guarded SIGKILL
    println!("done.");
    0
}

fn restart(selectors: &[&str]) -> i32 {
    if selectors.is_empty() {
        eprintln!("restart: need a selector");
        return 2;
    }
    let snap = snapshot(false);
    let targets = select(&snap, selectors);
    if targets.is_empty() {
        eprintln!("no targets match {selectors:?}");
        return 1;
    }
    // Capture commands, terminate everything, then re-exec.
    let mut plans: Vec<(String, Vec<String>, std::path::PathBuf)> = Vec::new();
    let mut pids: Vec<u32> = Vec::new();
    for t in &targets {
        pids.extend(&t.pids);
        if t.anchor_argv.is_empty() {
            eprintln!("skipping {}: command not captured", t.project);
            continue;
        }
        plans.push((t.project.clone(), t.anchor_argv.clone(), t.cwd.clone()));
    }
    if plans.is_empty() {
        eprintln!("nothing restartable (command not captured)");
        return 1;
    }
    let _ = verbs::signal_tree(&pids, "TERM");
    thread::sleep(Duration::from_millis(1500));
    let mut code = 0;
    for (project, argv, cwd) in plans {
        match verbs::respawn(&argv, &cwd) {
            Ok(()) => println!("restarted {project}"),
            Err(e) => {
                eprintln!("restart {project} failed: {e}");
                code = 1;
            }
        }
    }
    code
}

fn url(selectors: &[&str]) -> i32 {
    let snap = snapshot(false);
    let targets = select(&snap, selectors);
    if targets.is_empty() {
        eprintln!("no targets match {selectors:?}");
        return 1;
    }
    for t in targets {
        match &t.url {
            Some(u) => println!("{}\t{}", t.project, u.value),
            None => println!("{}\t—", t.project),
        }
    }
    0
}

// --- JSON view --------------------------------------------------------------

#[derive(Serialize)]
struct TargetJson {
    project: String,
    command: String,
    kind: &'static str,
    ports: Vec<u16>,
    url: Option<String>,
    cpu_pct: Option<f32>,
    mem_bytes: Option<u64>,
    uptime_secs: Option<u64>,
    pids: Vec<u32>,
    anchor_pid: u32,
    cwd: String,
    branch: Option<String>,
}

impl From<&Target> for TargetJson {
    fn from(t: &Target) -> Self {
        let measured = !t.pids.is_empty();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        TargetJson {
            project: t.project.clone(),
            command: t.command_label.clone(),
            kind: match t.kind {
                TargetKind::Listener => "listener",
                TargetKind::Watched => "watched",
            },
            ports: t.ports.clone(),
            url: t.url.as_ref().map(|u| u.value.clone()),
            cpu_pct: measured.then_some(t.cpu_pct),
            mem_bytes: measured.then_some(t.mem_bytes),
            uptime_secs: (t.anchor.start_time != 0)
                .then(|| now.saturating_sub(t.anchor.start_time)),
            pids: t.pids.clone(),
            anchor_pid: t.anchor.pid,
            cwd: t.cwd.display().to_string(),
            branch: t.git_branch.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Snapshot;

    #[test]
    fn selectors_match_by_port_project_and_command() {
        let snap = Snapshot::sample(); // client-portal has 2 targets (next dev + postgres)
        assert_eq!(select(&snap, &["3000"]).len(), 1); // by port
        assert_eq!(select(&snap, &[":8000"]).len(), 1); // by :port
        assert_eq!(select(&snap, &["client-portal"]).len(), 2); // by project -> the whole group
        assert!(select(&snap, &["postgres"])
            .iter()
            .any(|t| t.command_label == "postgres")); // by command label
        assert!(select(&snap, &["nope-xyz"]).is_empty()); // no match
    }

    #[test]
    fn json_view_nulls_unmeasurable_fields() {
        use crate::model::{Anchor, Target, TargetKey, TargetKind};
        // a docker-style target: no pids, no start_time
        let t = Target {
            key: TargetKey::Port(5432),
            kind: TargetKind::Listener,
            ports: vec![5432],
            anchor: Anchor {
                pid: 0,
                start_time: 0,
            },
            anchor_argv: vec![],
            pids: vec![],
            project: "db".into(),
            command_label: "postgres".into(),
            cwd: "/x".into(),
            git_branch: None,
            cpu_pct: 0.0,
            mem_bytes: 0,
            url: None,
        };
        let j = TargetJson::from(&t);
        assert_eq!(j.kind, "listener");
        assert!(j.cpu_pct.is_none() && j.mem_bytes.is_none() && j.uptime_secs.is_none());
        assert_eq!(j.ports, vec![5432]);
    }
}
