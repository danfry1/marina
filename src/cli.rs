//! Non-interactive CLI — the resolution engine exposed for scripts and agents.
//!
//!   marina ls [sel]… [--json]   list running dev targets (optionally filtered)
//!   marina kill <sel>… [--json] SIGTERM → verified SIGKILL matching targets
//!   marina restart <sel>…       restart matching targets (output captured)
//!   marina url <sel>… [--json]  print matching targets' URLs
//!   marina version              print the version
//!
//! A <selector> matches by project name (exact or substring, case-insensitive),
//! by port (`3000` or `:3000`), or by command label. Killing a project name
//! takes down every target under it — the grouping primitive, via the CLI.
//! Docker targets are stopped/restarted via `docker stop`/`docker restart`.
//!
//! Unknown commands and flags are errors (exit 2) — a typo must never fall
//! through and launch the TUI inside a script.

use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::model::{Snapshot, Target, TargetKind};
use crate::sampler::Sampler;
use crate::verbs;

pub const USAGE: &str = "\
marina — developer-process cockpit

USAGE:
    marina                       launch the TUI
    marina ls [sel]... [--json]  list running dev targets
    marina kill <sel>... [--json] stop matching targets (SIGTERM -> SIGKILL)
    marina restart <sel>...      restart matching targets (output captured)
    marina url <sel>... [--json] print matching targets' URLs
    marina version               print the version

SELECTOR:
    a project name (exact or substring), a port (3000 or :3000), or a command.
";

/// Dispatch a CLI subcommand. Returns `Some(exit_code)` if it handled the
/// invocation, `None` when there were no args (caller launches the TUI).
/// Exit codes: 0 ok, 1 no match, 2 usage error — so scripts/agents can branch.
pub fn dispatch(args: &[String]) -> Option<i32> {
    let cmd = args.first()?.as_str();
    let rest = &args[1..];
    let flags: Vec<&str> = rest
        .iter()
        .map(String::as_str)
        .filter(|s| s.starts_with('-'))
        .collect();
    let selectors: Vec<&str> = rest
        .iter()
        .map(String::as_str)
        .filter(|s| !s.starts_with('-'))
        .collect();
    if let Some(bad) = flags.iter().find(|f| **f != "--json") {
        eprintln!("marina: unknown flag {bad:?}\n");
        eprint!("{USAGE}");
        return Some(2);
    }
    let json = flags.contains(&"--json");
    let code = match cmd {
        "ls" => ls(json, &selectors),
        "kill" => kill(&selectors, json),
        "restart" => restart(&selectors),
        "url" => url(&selectors, json),
        "version" | "--version" | "-V" => {
            println!("marina {}", env!("CARGO_PKG_VERSION"));
            0
        }
        "help" | "--help" | "-h" => {
            print!("{USAGE}");
            0
        }
        other => {
            eprintln!("marina: unknown command {other:?}\n");
            eprint!("{USAGE}");
            2
        }
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

fn ls(json: bool, selectors: &[&str]) -> i32 {
    let snap = snapshot(true);
    let targets: Vec<&Target> = if selectors.is_empty() {
        snap.targets.iter().collect()
    } else {
        select(&snap, selectors)
    };
    if json {
        let view: Vec<TargetJson> = targets.iter().copied().map(TargetJson::from).collect();
        match serde_json::to_string_pretty(&view) {
            Ok(s) => println!("{s}"),
            Err(e) => eprintln!("marina: json error: {e}"),
        }
        return 0;
    }
    if targets.is_empty() {
        println!("no dev targets running");
        return if selectors.is_empty() { 0 } else { 1 };
    }
    println!(
        "{:<20} {:<16} {:<8} {:>6} {:>8} {:<14}",
        "PROJECT", "COMMAND", "PORT", "CPU", "MEM", "URL"
    );
    for t in &targets {
        let port = match t.ports.first() {
            // `!` marks a LAN-exposed bind (0.0.0.0 / ::)
            Some(p) if t.exposed => format!(":{p}!"),
            Some(p) => format!(":{p}"),
            None => "—".into(),
        };
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
            "{:<20} {:<16} {:<8} {:>6} {:>8} {:<14}",
            t.project, t.command_label, port, cpu, mem, url
        );
    }
    0
}

fn kill(selectors: &[&str], json: bool) -> i32 {
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
    let mut pid_starts: Vec<verbs::PidStart> = Vec::new();
    let mut killed: Vec<&Target> = Vec::new();
    for t in &targets {
        killed.push(t);
        if let Some(c) = &t.container {
            if !json {
                println!("stopping container {c}");
            }
            let _ = std::process::Command::new("docker")
                .args(["stop", c])
                .status();
            continue;
        }
        let port = t.ports.first().map(|p| format!(":{p}")).unwrap_or_default();
        if !json {
            println!("killing {} {} ({} pids)", t.project, port, t.pids.len());
        }
        pid_starts.extend(&t.pid_starts);
    }
    if !pid_starts.is_empty() {
        // verified SIGTERM -> wait -> verified SIGKILL (recycled pids skipped)
        verbs::kill_blocking(&pid_starts, Duration::from_millis(1500));
    }
    if json {
        let view: Vec<TargetJson> = killed.into_iter().map(TargetJson::from).collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&view).unwrap_or_default()
        );
    } else {
        println!("done.");
    }
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
    let mut pid_starts: Vec<verbs::PidStart> = Vec::new();
    let mut code = 0;
    for t in &targets {
        if let Some(c) = &t.container {
            let ok = std::process::Command::new("docker")
                .args(["restart", c])
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if ok {
                println!("restarted container {c}");
            } else {
                eprintln!("docker restart {c} failed");
                code = 1;
            }
            continue;
        }
        pid_starts.extend(&t.pid_starts);
        if t.anchor_argv.is_empty() {
            eprintln!("skipping {}: command not captured", t.project);
            continue;
        }
        plans.push((t.project.clone(), t.anchor_argv.clone(), t.cwd.clone()));
    }
    if plans.is_empty() {
        if targets.iter().any(|t| t.container.is_some()) {
            return code; // docker-only selection — outcome already reported
        }
        eprintln!("nothing restartable (command not captured)");
        return 1;
    }
    verbs::kill_blocking(&pid_starts, Duration::from_millis(1500));
    for (project, argv, cwd) in plans {
        let log = crate::logs::state_log_path(&project);
        match verbs::respawn(&argv, &cwd, log.as_deref()) {
            Ok(_child) => match &log {
                Some(p) => println!("restarted {project} (output -> {})", p.display()),
                None => println!("restarted {project}"),
            },
            Err(e) => {
                eprintln!("restart {project} failed: {e}");
                code = 1;
            }
        }
    }
    code
}

fn url(selectors: &[&str], json: bool) -> i32 {
    let snap = snapshot(false);
    let targets = select(&snap, selectors);
    if targets.is_empty() {
        eprintln!("no targets match {selectors:?}");
        return 1;
    }
    if json {
        #[derive(Serialize)]
        struct UrlJson<'a> {
            project: &'a str,
            url: Option<&'a str>,
        }
        let view: Vec<UrlJson> = targets
            .iter()
            .map(|t| UrlJson {
                project: &t.project,
                url: t.url.as_ref().map(|u| u.value.as_str()),
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&view).unwrap_or_default()
        );
        return 0;
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
    /// Listening on 0.0.0.0 / :: — reachable from the LAN.
    exposed: bool,
    /// Docker container name, when the target is a published container port.
    container: Option<String>,
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
            exposed: t.exposed,
            container: t.container.clone(),
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
    fn unknown_commands_and_flags_error_instead_of_launching_the_tui() {
        // a typo'd subcommand must not fall through to the TUI
        assert_eq!(dispatch(&["lss".into()]), Some(2));
        assert_eq!(dispatch(&["--jsonx".into()]), Some(2));
        // no args -> None -> caller launches the TUI
        assert_eq!(dispatch(&[]), None);
    }

    #[test]
    fn version_prints_and_exits_zero() {
        assert_eq!(dispatch(&["version".into()]), Some(0));
        assert_eq!(dispatch(&["--version".into()]), Some(0));
        assert_eq!(dispatch(&["-V".into()]), Some(0));
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
            pid_starts: vec![],
            pids: vec![],
            project: "db".into(),
            command_label: "postgres".into(),
            cwd: "/x".into(),
            git_branch: None,
            cpu_pct: 0.0,
            mem_bytes: 0,
            url: None,
            exposed: true,
            container: Some("myapp-db-1".into()),
        };
        let j = TargetJson::from(&t);
        assert_eq!(j.kind, "listener");
        assert!(j.cpu_pct.is_none() && j.mem_bytes.is_none() && j.uptime_secs.is_none());
        assert_eq!(j.ports, vec![5432]);
        assert!(j.exposed);
        assert_eq!(j.container.as_deref(), Some("myapp-db-1"));
    }

    #[test]
    fn json_shape_is_stable() {
        // Agents depend on `ls --json` — pin the field names.
        let snap = Snapshot::sample();
        let j = serde_json::to_value(TargetJson::from(&snap.targets[0])).unwrap();
        let obj = j.as_object().unwrap();
        for field in [
            "project",
            "command",
            "kind",
            "ports",
            "url",
            "cpu_pct",
            "mem_bytes",
            "uptime_secs",
            "pids",
            "anchor_pid",
            "cwd",
            "branch",
            "exposed",
            "container",
        ] {
            assert!(obj.contains_key(field), "missing JSON field {field}");
        }
        assert_eq!(obj.len(), 14, "unexpected extra/removed JSON fields");
    }
}
