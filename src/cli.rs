//! Non-interactive CLI — the resolution engine exposed for scripts and agents.
//!
//!   marina ls [sel]… [--json]   list running dev targets (optionally filtered)
//!   marina kill <sel>… [--json] SIGTERM → verified SIGKILL matching targets
//!   marina restart <sel>…       restart matching targets (output captured)
//!   marina url <sel>… [--json]  print matching targets' URLs
//!   marina open [sel]… [--mine] open matching servers in the browser
//!   marina who <port>… [--json] what is holding a port (dev target or not)
//!   marina free <port>… [--json] stop whatever dev target holds a port
//!   marina port [name] [--json]  a stable free port for this project/worktree
//!   marina port --list | --release [name]
//!   marina agents [sel]… [--json] running coding-agent sessions + their shells
//!   marina version              print the version
//!
//! A <selector> matches by project name (exact or substring, case-insensitive),
//! by port (`3000` or `:3000`), or by command label. Killing a project name
//! takes down every target under it — the grouping primitive, via the CLI.
//! Docker targets are stopped/restarted via `docker stop`/`docker restart`.
//!
//! `--mine` narrows ls/kill/restart/url/open to targets started by the agent
//! session running this command (so an agent can clean up after itself);
//! `--orphaned` to targets whose launching agent session has ended.
//!
//! `agents` is read-only by design: agent sessions are never targets, so no
//! selector, scope or verb here can stop one.
//!
//! Unknown commands and flags are errors (exit 2) — a typo must never fall
//! through and launch the TUI inside a script.

use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::launcher;
use crate::model::{Agent, Snapshot, Target, TargetKind};
use crate::sampler::Sampler;
use crate::sources::{Netstat2Ports, PortSource};
use crate::ui::{fmt_uptime, tildify};
use crate::verbs;

pub const USAGE: &str = "\
marina — developer-process cockpit

USAGE:
    marina                       launch the TUI
    marina ls [sel]... [--json]  list running dev targets
    marina kill <sel>... [--json] stop matching targets (SIGTERM -> SIGKILL)
    marina restart <sel>...      restart matching targets (output captured)
    marina url <sel>... [--json] print matching targets' URLs
    marina open [sel]... [--mine]  open matching servers in the browser
                                 (no selector: the ones running in this dir)
    marina who <port>... [--json]  what is holding a port
    marina free <port>... [--json] stop the dev target holding a port
    marina port [name] [--json]  stable free port for this project/worktree
                                 (pnpm dev --port $(marina port))
    marina port --list           show assigned ports
    marina port --release [name] give this project's port back
    marina agents [sel]... [--json]  coding-agent sessions, their shells + servers
    marina version               print the version

SELECTOR:
    a project name (exact or substring), a port (3000 or :3000), or a command.
    `who` and `free` take ports only.

SCOPE (ls / kill / restart / url / open; usable instead of, or with, selectors):
    --mine       only targets started by the agent session running marina
    --orphaned   only targets whose launching agent session has ended
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
    if let Some(bad) = flags.iter().find(|f| {
        !matches!(
            **f,
            "--json" | "--mine" | "--orphaned" | "--list" | "--release"
        )
    }) {
        eprintln!("marina: unknown flag {bad:?}\n");
        eprint!("{USAGE}");
        return Some(2);
    }
    let json = flags.contains(&"--json");
    let mut scope = Scope {
        mine: None,
        orphaned: flags.contains(&"--orphaned"),
    };
    if flags.contains(&"--mine") {
        match launcher::own_agent() {
            Some(own) => scope.mine = Some(own),
            None => {
                eprintln!("marina: --mine: not running inside an agent session");
                return Some(2);
            }
        }
    }
    if scope.active() && matches!(cmd, "who" | "free" | "port" | "agents") {
        eprintln!("marina: {cmd} doesn't take --mine / --orphaned");
        return Some(2);
    }
    if json && cmd == "open" {
        eprintln!("marina: open doesn't take --json (marina url --json prints the URLs)");
        return Some(2);
    }
    let (list, release) = (flags.contains(&"--list"), flags.contains(&"--release"));
    if (list || release) && cmd != "port" {
        eprintln!("marina: --list / --release only apply to `marina port`");
        return Some(2);
    }
    let code = match cmd {
        "ls" => ls(json, &selectors, &scope),
        "kill" => kill(&selectors, json, &scope),
        "restart" => restart(&selectors, &scope),
        "url" => url(&selectors, json, &scope),
        "open" => open(&selectors, &scope),
        "who" => who(&selectors, json),
        "free" => free(&selectors, json),
        "port" => port(&selectors, json, list, release),
        "agents" => agents(&selectors, json),
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
    s.eager_git();
    let snap = s.build();
    if with_cpu {
        thread::sleep(Duration::from_millis(500));
        s.build()
    } else {
        snap
    }
}

/// `--mine` / `--orphaned`: narrow by who launched a target.
struct Scope {
    /// The agent session running this command (pid, session id).
    mine: Option<(Option<u32>, Option<String>)>,
    orphaned: bool,
}

impl Scope {
    fn active(&self) -> bool {
        self.mine.is_some() || self.orphaned
    }

    fn keep(&self, t: &Target) -> bool {
        let l = t.launcher.as_ref();
        if let Some(own) = &self.mine {
            if !l.is_some_and(|l| launcher::same_session(l, own)) {
                return false;
            }
        }
        !self.orphaned || l.is_some_and(|l| l.is_orphaned())
    }
}

/// Selector + scope matching. No selectors means "everything in scope" when a
/// scope flag is given (`kill --orphaned`), or for `ls`; otherwise nothing.
fn pick<'a>(
    snap: &'a Snapshot,
    selectors: &[&str],
    scope: &Scope,
    all_if_empty: bool,
) -> Vec<&'a Target> {
    if selectors.is_empty() && !all_if_empty && !scope.active() {
        return Vec::new();
    }
    snap.targets
        .iter()
        .filter(|t| selectors.is_empty() || selectors.iter().any(|s| matches(t, s)))
        .filter(|t| scope.keep(t))
        .collect()
}

#[cfg(test)]
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

fn ls(json: bool, selectors: &[&str], scope: &Scope) -> i32 {
    let snap = snapshot(true);
    let targets = pick(&snap, selectors, scope, true);
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
        return if selectors.is_empty() && !scope.active() {
            0
        } else {
            1
        };
    }
    println!(
        "{:<20} {:<16} {:<8} {:>6} {:>8} {:<14} {:<14}",
        "PROJECT", "COMMAND", "PORT", "CPU", "MEM", "VIA", "URL"
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
        let via = t
            .launcher
            .as_ref()
            .map(|l| l.short())
            .unwrap_or_else(|| "—".into());
        println!(
            "{:<20} {:<16} {:<8} {:>6} {:>8} {:<14} {:<14}",
            t.project, t.command_label, port, cpu, mem, via, url
        );
    }
    0
}

/// Does agent `a` match a selector: its name, project, cwd, or a session-id
/// prefix?
fn agent_matches(a: &Agent, sel: &str) -> bool {
    let q = sel.to_lowercase();
    a.name == q
        || a.project.to_lowercase().contains(&q)
        || a.cwd.to_string_lossy().to_lowercase().contains(&q)
        || a.session.as_deref().is_some_and(|s| s.starts_with(&q))
}

/// The servers agent `a` launched: targets whose live launcher is its pid.
fn servers_of<'a>(snap: &'a Snapshot, a: &Agent) -> Vec<&'a Target> {
    snap.targets
        .iter()
        .filter(|t| {
            t.launcher
                .as_ref()
                .is_some_and(|l| l.alive && l.name == a.name && l.pid == Some(a.pid))
        })
        .collect()
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn agents(selectors: &[&str], json: bool) -> i32 {
    let snap = snapshot(true);
    let picked: Vec<&Agent> = snap
        .agents
        .iter()
        .filter(|a| selectors.is_empty() || selectors.iter().any(|s| agent_matches(a, s)))
        .collect();
    if json {
        let view: Vec<AgentJson> = picked
            .iter()
            .map(|a| AgentJson::new(a, &servers_of(&snap, a)))
            .collect();
        match serde_json::to_string_pretty(&view) {
            Ok(s) => println!("{s}"),
            Err(e) => eprintln!("marina: json error: {e}"),
        }
        return if picked.is_empty() && !selectors.is_empty() {
            1
        } else {
            0
        };
    }
    if picked.is_empty() {
        println!("no agent sessions running");
        return if selectors.is_empty() { 0 } else { 1 };
    }
    let pw = picked
        .iter()
        .map(|a| a.project.chars().count())
        .max()
        .unwrap_or(0)
        .clamp(7, 32);
    println!(
        "{:<9} {:<7} {:<pw$} {:>6} {:>8} {:<5} {:<10} {:<8} DOING",
        "AGENT", "PID", "PROJECT", "CPU", "MEM", "UP", "SESSION", "GIT"
    );
    let repeated = crate::model::repeated_helpers(picked.iter().copied());
    for a in picked {
        // `$ cargo test` per live shell, `:3000` per server it started
        let mut doing: Vec<String> = a.shells().map(|c| format!("$ {}", c.label)).collect();
        doing.extend(
            servers_of(&snap, a)
                .iter()
                .flat_map(|t| t.ports.iter().map(|p| format!(":{p}"))),
        );
        let now = now_secs();
        let servers = servers_of(&snap, a).len();
        let doing = if doing.is_empty() {
            match (a.looks_finished(now, servers), a.is_stale(now)) {
                (true, _) => format!("{} (stale, looks finished)", a.activity(now)),
                (false, true) => format!("{} (stale)", a.activity(now)),
                _ => a.activity(now),
            }
        } else {
            doing.join(" · ")
        };
        let doing = match a.title() {
            Some(t) => format!("{doing} — {t}"),
            None => doing,
        };
        println!(
            "{:<9} {:<7} {:<pw$} {:>6} {:>8} {:<5} {:<10} {:<8} {doing}",
            a.name,
            a.pid,
            a.project,
            format!("{:.1}%", a.cpu_pct),
            format!("{}MB", a.mem_bytes / (1024 * 1024)),
            fmt_uptime(a.start_time),
            a.session
                .as_deref()
                .map(|s| crate::launcher::short_id(s, 8))
                .unwrap_or("—"),
            a.git.map(|g| g.short()).unwrap_or_else(|| "—".into()),
        );
    }
    if !repeated.is_empty() {
        println!();
        println!(
            "helpers each session runs its own copy of: {}",
            repeated_text(&repeated, usize::MAX)
        );
    }
    0
}

/// `chrome-devtools-mcp ×19 785MB, mobile-mcp ×2 90MB` — the first `n`.
pub fn repeated_text(r: &[(String, usize, u64)], n: usize) -> String {
    r.iter()
        .take(n)
        .map(|(l, k, m)| format!("{l} ×{k} {}MB", m / (1024 * 1024)))
        .collect::<Vec<_>>()
        .join(", ")
}

fn kill(selectors: &[&str], json: bool, scope: &Scope) -> i32 {
    if selectors.is_empty() && !scope.active() {
        eprintln!("kill: need a selector (project, port, or command) or --mine / --orphaned");
        return 2;
    }
    let snap = snapshot(false);
    let targets = pick(&snap, selectors, scope, false);
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

fn restart(selectors: &[&str], scope: &Scope) -> i32 {
    if selectors.is_empty() && !scope.active() {
        eprintln!("restart: need a selector or --mine / --orphaned");
        return 2;
    }
    let snap = snapshot(false);
    let targets = pick(&snap, selectors, scope, false);
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

fn url(selectors: &[&str], json: bool, scope: &Scope) -> i32 {
    let snap = snapshot(false);
    let targets = pick(&snap, selectors, scope, false);
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

/// `marina open [sel]`: open matching servers' web URLs in the browser. With
/// no selector or scope, the servers running in (or under) the current
/// directory — what you mean from a project's terminal.
fn open(selectors: &[&str], scope: &Scope) -> i32 {
    let snap = snapshot(false);
    let bare = selectors.is_empty() && !scope.active();
    let mut targets = pick(&snap, selectors, scope, bare);
    if bare {
        // running here: in this directory or under it, or in a project this
        // directory is inside (`proj/src` finds proj's server)
        let here = std::env::current_dir().ok();
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
        if here
            .as_ref()
            .is_some_and(|h| h.parent().is_none() || Some(h) == home.as_ref())
        {
            eprintln!("marina open: from here that's everything — name one (marina open api, marina open 3000)");
            return 2;
        }
        targets.retain(|t| {
            here.as_ref().is_some_and(|h| {
                !t.cwd.as_os_str().is_empty() && (t.cwd.starts_with(h) || h.starts_with(&t.cwd))
            })
        });
    }
    let web: Vec<&Target> = targets
        .iter()
        .copied()
        .filter(|t| t.url.as_ref().is_some_and(|u| u.scheme.is_web()))
        .collect();
    // from ~ or / a bare `open` would match everything: list, don't open
    if bare && web.len() > 3 {
        eprintln!("{} web servers run under here — name one:", web.len());
        for t in &web {
            if let Some(u) = &t.url {
                eprintln!(
                    "  marina open {}\t{}",
                    t.ports.first().copied().unwrap_or_default(),
                    u.value
                );
            }
        }
        return 1;
    }
    if web.is_empty() {
        eprintln!(
            "{}",
            match (selectors.is_empty(), scope.active()) {
                (true, false) => "no web server running here — name one (marina open api, marina open 3000) or use --mine",
                _ => "no web server matches",
            }
        );
        return 1;
    }
    let mut failed = false;
    for t in web {
        let Some(u) = &t.url else { continue };
        match verbs::open_url(&u.value) {
            Ok(()) => println!("opening {}\t{}", t.project, u.value),
            Err(e) => {
                eprintln!("couldn't open {}: {e}", u.value);
                failed = true;
            }
        }
    }
    i32::from(failed)
}

// --- ports: who / free ------------------------------------------------------

/// `who`/`free` take ports only — a project substring like `api` silently
/// matching several targets is fine for `ls`, not for "what's on this port".
fn parse_ports(cmd: &str, selectors: &[&str]) -> Result<Vec<u16>, i32> {
    if selectors.is_empty() {
        eprintln!("{cmd}: need a port (3000 or :3000)");
        return Err(2);
    }
    let mut ports = Vec::new();
    for sel in selectors {
        match sel.trim_start_matches(':').parse::<u16>() {
            Ok(p) if p != 0 => {
                if !ports.contains(&p) {
                    ports.push(p);
                }
            }
            _ => {
                eprintln!("{cmd}: {sel:?} is not a port (use `marina ls {sel}` to match by name)");
                return Err(2);
            }
        }
    }
    Ok(ports)
}

fn target_on(snap: &Snapshot, port: u16) -> Option<&Target> {
    snap.targets.iter().find(|t| t.ports.contains(&port))
}

/// A process holding a port that isn't a dev target — curated out (outside
/// $HOME, a system daemon), `[[ignore]]`d, or an unmapped docker proxy.
/// Only the process name is exposed, never argv.
#[derive(Serialize)]
struct Holder {
    pid: u32,
    name: String,
}

/// Every listener on `port`, or `None` when the port scan itself failed.
fn listener_pids(port: u16) -> Option<Vec<u32>> {
    let mut pids: Vec<u32> = Netstat2Ports
        .listeners()
        .ok()?
        .into_iter()
        .filter(|l| l.port == port)
        .map(|l| l.pid)
        .collect();
    pids.sort_unstable();
    pids.dedup();
    Some(pids)
}

fn holders(port: u16) -> Vec<Holder> {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
    let pids = listener_pids(port).unwrap_or_default();
    let wanted: Vec<Pid> = pids.iter().map(|&p| Pid::from_u32(p)).collect();
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&wanted),
        true,
        ProcessRefreshKind::nothing(),
    );
    pids.into_iter()
        .map(|pid| Holder {
            pid,
            name: sys
                .process(Pid::from_u32(pid))
                .map(|p| p.name().to_string_lossy().into_owned())
                .unwrap_or_else(|| "?".into()),
        })
        .collect()
}

/// Busy if anything is listening. Falls back to a bind probe when the port
/// scan fails, so a broken scan never reports a held port as free.
fn port_busy(port: u16) -> bool {
    match listener_pids(port) {
        Some(pids) => !pids.is_empty(),
        None => std::net::TcpListener::bind(("127.0.0.1", port)).is_err(),
    }
}

/// `client-portal · next dev · pid 4121 · up 3d · feat/x · ~/dev/client-portal`
fn describe(t: &Target, port: u16) -> String {
    let mut parts = vec![t.project.clone(), t.command_label.clone()];
    if let Some(c) = &t.container {
        parts.push(format!("container {c}"));
    } else {
        if t.anchor.pid != 0 {
            parts.push(format!("pid {}", t.anchor.pid));
        }
        if t.anchor.start_time != 0 {
            parts.push(format!("up {}", fmt_uptime(t.anchor.start_time)));
        }
        if let Some(b) = &t.git_branch {
            parts.push(b.clone());
        }
        parts.push(tildify(&t.cwd.display().to_string()));
    }
    let others: Vec<String> = t
        .ports
        .iter()
        .filter(|&&p| p != port)
        .map(|p| format!(":{p}"))
        .collect();
    if !others.is_empty() {
        parts.push(format!("also {}", others.join(" ")));
    }
    if t.exposed {
        parts.push("LAN-exposed".into());
    }
    if let Some(l) = &t.launcher {
        parts.push(format!("via {}", l.describe()));
    }
    parts.join(" · ")
}

/// What `free` stopped: just the name, plus any other ports that went with it
/// (the whole target is stopped, so its sibling ports are released too).
fn stopped(t: &Target, port: u16) -> String {
    let others: Vec<String> = t
        .ports
        .iter()
        .filter(|&&p| p != port)
        .map(|p| format!(":{p}"))
        .collect();
    let mut s = format!("{} · {}", t.project, t.command_label);
    if !others.is_empty() {
        s.push_str(&format!(" (also released {})", others.join(" ")));
    }
    s
}

fn describe_holders(hs: &[Holder]) -> String {
    hs.iter()
        .map(|h| format!("{} (pid {})", h.name, h.pid))
        .collect::<Vec<_>>()
        .join(", ")
}

#[derive(Serialize)]
struct PortJson {
    port: u16,
    /// who: `target` | `other` | `free`.
    /// free: `freed` | `already_free` | `refused` | `still_busy`.
    status: &'static str,
    target: Option<TargetJson>,
    /// Non-target processes holding the port (name + pid only).
    processes: Vec<Holder>,
}

/// Exit 0 when every port is held, 1 when any is free — so
/// `marina who 3000 || pnpm dev` reads naturally.
fn who(selectors: &[&str], json: bool) -> i32 {
    let ports = match parse_ports("who", selectors) {
        Ok(p) => p,
        Err(code) => return code,
    };
    let snap = snapshot(false);
    let mut code = 0;
    let mut view = Vec::new();
    for port in ports {
        let (status, target, processes) = match target_on(&snap, port) {
            Some(t) => ("target", Some(t), Vec::new()),
            None => {
                let hs = holders(port);
                // held with no visible owner (another user's) still counts as held
                if hs.is_empty() && !port_busy(port) {
                    ("free", None, hs)
                } else {
                    ("other", None, hs)
                }
            }
        };
        if status == "free" {
            code = 1;
        }
        if !json {
            let line = match (status, target) {
                (_, Some(t)) => describe(t, port),
                ("free", _) => "free".into(),
                _ if processes.is_empty() => {
                    "in use, but the owner isn't visible (another user's process?)".into()
                }
                _ => format!("{} — not a dev target", describe_holders(&processes)),
            };
            println!(":{port:<6}{line}");
        }
        view.push(PortJson {
            port,
            status,
            target: target.map(TargetJson::from),
            processes,
        });
    }
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&view).unwrap_or_default()
        );
    }
    code
}

/// Stop the dev target holding each port, then wait until the port is really
/// released. Idempotent: an already-free port exits 0, so
/// `marina free 3000 && pnpm dev` is safe to re-run. Refuses (exit 1) to touch
/// a holder that isn't a dev target — that's outside marina's remit.
fn free(selectors: &[&str], json: bool) -> i32 {
    const RELEASE_WAIT: Duration = Duration::from_secs(3);

    let ports = match parse_ports("free", selectors) {
        Ok(p) => p,
        Err(code) => return code,
    };
    let snap = snapshot(false);

    // Classify first, then stop each target once (two ports can share one).
    let mut plan: Vec<(u16, Option<&Target>, Vec<Holder>)> = Vec::new();
    let mut to_stop: Vec<&Target> = Vec::new();
    for &port in &ports {
        match target_on(&snap, port) {
            Some(t) => {
                if !to_stop.iter().any(|s| s.key == t.key) {
                    to_stop.push(t);
                }
                plan.push((port, Some(t), Vec::new()));
            }
            None => plan.push((port, None, holders(port))),
        }
    }

    let mut pid_starts: Vec<verbs::PidStart> = Vec::new();
    for t in &to_stop {
        match &t.container {
            Some(c) => {
                let _ = std::process::Command::new("docker")
                    .args(["stop", c])
                    .stdout(std::process::Stdio::null())
                    .status();
            }
            None => pid_starts.extend(&t.pid_starts),
        }
    }
    if !pid_starts.is_empty() {
        verbs::kill_blocking(&pid_starts, Duration::from_millis(1500));
    }
    if !to_stop.is_empty() {
        let deadline = std::time::Instant::now() + RELEASE_WAIT;
        while plan.iter().any(|(p, t, _)| t.is_some() && port_busy(*p))
            && std::time::Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(150));
        }
    }

    let mut code = 0;
    let mut view = Vec::new();
    for (port, target, processes) in plan {
        let status = match target {
            Some(_) if port_busy(port) => "still_busy",
            Some(_) => "freed",
            None if !processes.is_empty() || port_busy(port) => "refused",
            None => "already_free",
        };
        if matches!(status, "still_busy" | "refused") {
            code = 1;
        }
        if !json {
            let line = match (status, target) {
                ("freed", Some(t)) => format!("freed — stopped {}", stopped(t, port)),
                ("still_busy", Some(t)) => {
                    format!("still in use after stopping {}", stopped(t, port))
                }
                ("already_free", _) => "already free".into(),
                _ if processes.is_empty() => {
                    "in use by a process marina can't see (another user's?) — not touched".into()
                }
                _ => format!(
                    "held by {} — not a dev target, not touched",
                    describe_holders(&processes)
                ),
            };
            println!(":{port:<6}{line}");
        }
        view.push(PortJson {
            port,
            status,
            target: target.map(TargetJson::from),
            processes,
        });
    }
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&view).unwrap_or_default()
        );
    }
    code
}

// --- port -------------------------------------------------------------------

/// The project root `marina port` keys on: the nearest project marker above
/// the (canonical) cwd — each worktree, and each package in a monorepo, is
/// its own root.
fn current_root() -> Option<std::path::PathBuf> {
    let cwd = std::env::current_dir().ok()?.canonicalize().ok()?;
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    crate::resolve::project_root(&cwd, home.as_deref())
}

/// `marina port [name]`: print a stable port for this project (stdout is just
/// the number, for `$(marina port)`); `--list` shows every assignment;
/// `--release [name]` gives one back. Exit 1 when the range is exhausted or
/// there is nothing to release.
fn port(selectors: &[&str], json: bool, list: bool, release: bool) -> i32 {
    use crate::ports::{allocate, AllocError, Outcome, Store};

    if list && release {
        eprintln!("port: use either --list or --release");
        return 2;
    }
    if selectors.len() > 1 {
        eprintln!("port: takes at most one service name");
        return 2;
    }
    let name = selectors.first().copied();
    if let Some(n) = name.filter(|n| !crate::ports::valid_name(n)) {
        eprintln!("port: {n:?} is not a valid service name (letters, digits, - _ .)");
        return 2;
    }
    let store = match Store::open() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("port: can't open the port registry: {e}");
            return 1;
        }
    };
    let mut leases = store.load();
    if list {
        return port_list(&leases, json);
    }
    let Some(root) = current_root() else {
        eprintln!("port: not inside a project — run it from a project or worktree directory");
        return 2;
    };

    if release {
        let freed = crate::ports::release(&mut leases, &root, name);
        if let Err(e) = store.save(&leases) {
            eprintln!("port: can't save the port registry: {e}");
            return 1;
        }
        return match freed {
            Some(p) => {
                println!("released :{p}");
                0
            }
            None => {
                eprintln!("no port assigned here");
                1
            }
        };
    }

    // Busy ports are fine only when it's this project's own server on them.
    let listening: std::collections::HashSet<u16> = Netstat2Ports
        .listeners()
        .map(|ls| ls.into_iter().map(|l| l.port).collect())
        .unwrap_or_default();
    let snap = std::cell::OnceCell::new();
    let foreign_busy = |p: u16| {
        let busy = listening.contains(&p) || std::net::TcpListener::bind(("127.0.0.1", p)).is_err();
        busy && !snapshot_on(&snap)
            .targets
            .iter()
            .any(|t| t.ports.contains(&p) && t.cwd.starts_with(&root))
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let range = crate::config::load().ports.range();
    let result = allocate(
        &mut leases,
        &root,
        name,
        range.clone(),
        now,
        foreign_busy,
        |r| r.exists(),
    );
    let (p, outcome) = match result {
        Ok(r) => r,
        Err(AllocError::Exhausted) => {
            eprintln!(
                "port: every port in {}-{} is assigned or busy (widen [ports] range, or --release unused ones)",
                range.start(),
                range.end()
            );
            return 1;
        }
    };
    if let Err(e) = store.save(&leases) {
        eprintln!("port: can't save the port registry: {e}");
        return 1;
    }
    if json {
        #[derive(Serialize)]
        struct PortOut<'a> {
            port: u16,
            root: String,
            name: Option<&'a str>,
            /// `existing` | `new` | `moved`
            status: &'static str,
            moved_from: Option<u16>,
        }
        let (status, moved_from) = match outcome {
            Outcome::Existing => ("existing", None),
            Outcome::New => ("new", None),
            Outcome::Moved { from } => ("moved", Some(from)),
        };
        let out = PortOut {
            port: p,
            root: root.display().to_string(),
            name,
            status,
            moved_from,
        };
        println!("{}", serde_json::to_string_pretty(&out).unwrap_or_default());
    } else {
        if let Outcome::Moved { from } = outcome {
            eprintln!(
                "marina: :{from} is now used by another process — moved this project to :{p}"
            );
        }
        println!("{p}");
    }
    0
}

/// Build the snapshot at most once, and only if a busy port needs explaining.
fn snapshot_on(cell: &std::cell::OnceCell<Snapshot>) -> &Snapshot {
    cell.get_or_init(|| snapshot(false))
}

fn port_list(leases: &crate::ports::Leases, json: bool) -> i32 {
    let mut rows: Vec<&crate::ports::Lease> = leases.leases.iter().collect();
    rows.sort_by_key(|l| l.port);
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&rows).unwrap_or_default()
        );
        return 0;
    }
    if rows.is_empty() {
        println!("no ports assigned (run `marina port` in a project)");
        return 0;
    }
    let listening: std::collections::HashSet<u16> = Netstat2Ports
        .listeners()
        .map(|ls| ls.into_iter().map(|l| l.port).collect())
        .unwrap_or_default();
    println!("{:<7} {:<10} {:<9} PROJECT ROOT", "PORT", "NAME", "STATE");
    for l in rows {
        let state = if !l.root.exists() {
            "gone"
        } else if listening.contains(&l.port) {
            "listening"
        } else {
            "idle"
        };
        println!(
            ":{:<6} {:<10} {:<9} {}",
            l.port,
            l.name.as_deref().unwrap_or("—"),
            state,
            tildify(&l.root.display().to_string())
        );
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
    /// Who started it (agent session / editor / terminal / detached), or null.
    launcher: Option<LauncherJson>,
}

#[derive(Serialize)]
struct LauncherJson {
    /// `agent` | `editor` | `terminal` | `detached`.
    kind: &'static str,
    name: String,
    pid: Option<u32>,
    /// The launching process is still running.
    alive: bool,
    /// Started by an agent session that has since ended.
    orphaned: bool,
    /// Agent session id (Claude Code: `claude --resume <session>`).
    session: Option<String>,
    cwd: Option<String>,
}

impl From<&launcher::Launcher> for LauncherJson {
    fn from(l: &launcher::Launcher) -> Self {
        LauncherJson {
            kind: l.kind.as_str(),
            name: l.name.clone(),
            pid: l.pid,
            alive: l.alive,
            orphaned: l.is_orphaned(),
            session: l.session.clone(),
            cwd: l.cwd.as_ref().map(|c| c.display().to_string()),
        }
    }
}

/// `marina agents --json`: one live agent session.
#[derive(Serialize)]
struct AgentJson {
    name: String,
    pid: u32,
    /// Resumable session id, when known.
    session: Option<String>,
    /// How to get back into it (`claude --resume <id>`, …).
    resume: Option<String>,
    /// `working` (a shell running, or recent terminal output), `blocked`
    /// (waiting on a question, host-reported), `idle`, or `stale` (idle, no
    /// terminal input for over a day).
    state: &'static str,
    cwd: String,
    project: String,
    branch: Option<String>,
    uptime_secs: Option<u64>,
    cpu_pct: f32,
    mem_bytes: u64,
    /// Agent + its shells and helpers (not its servers' pids).
    pids: Vec<u32>,
    /// Shells running a command right now.
    shells: Vec<AgentChildJson>,
    /// Long-lived helpers: MCP servers, daemons, …
    helpers: Vec<AgentChildJson>,
    /// Dev servers this session started (still running).
    servers: Vec<AgentServerJson>,
    /// The session's task title, when its terminal host reports one.
    title: Option<String>,
    /// The multiplexer pane it runs in: `{kind: "herdr"|"tmux", target}`.
    host: Option<HostJson>,
    /// Its working tree: uncommitted, untracked and unpushed work.
    git: Option<GitJson>,
    /// Stale, no servers, and a clean, pushed tree.
    looks_finished: bool,
}

#[derive(Serialize)]
struct HostJson {
    kind: &'static str,
    target: String,
    /// For herdr/tmux, the terminal app its client runs in.
    outer: Option<&'static str>,
}

#[derive(Serialize)]
struct GitJson {
    changed: u32,
    untracked: u32,
    insertions: u32,
    deletions: u32,
    ahead: Option<u32>,
    behind: Option<u32>,
}

#[derive(Serialize)]
struct AgentChildJson {
    pid: u32,
    command: String,
    uptime_secs: Option<u64>,
    cpu_pct: f32,
    mem_bytes: u64,
}

#[derive(Serialize)]
struct AgentServerJson {
    project: String,
    command: String,
    ports: Vec<u16>,
}

impl AgentJson {
    fn new(a: &Agent, servers: &[&Target]) -> Self {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let up = |start: u64| (start != 0).then(|| now.saturating_sub(start));
        let child = |c: &crate::model::AgentChild| AgentChildJson {
            pid: c.pid,
            command: c.label.clone(),
            uptime_secs: up(c.start_time),
            cpu_pct: c.cpu_pct,
            mem_bytes: c.mem_bytes,
        };
        AgentJson {
            name: a.name.clone(),
            pid: a.pid,
            session: a.session.clone(),
            resume: a.as_launcher().resume_hint(),
            state: if a.is_working(now) {
                "working"
            } else if a.is_blocked() {
                "blocked"
            } else if a.is_stale(now) {
                "stale"
            } else {
                "idle"
            },
            cwd: a.cwd.display().to_string(),
            project: a.project.clone(),
            branch: a.git_branch.clone(),
            uptime_secs: up(a.start_time),
            cpu_pct: a.cpu_pct,
            mem_bytes: a.mem_bytes,
            pids: a.pids.clone(),
            shells: a.children.iter().filter(|c| c.shell).map(child).collect(),
            helpers: a.children.iter().filter(|c| !c.shell).map(child).collect(),
            servers: servers
                .iter()
                .map(|t| AgentServerJson {
                    project: t.project.clone(),
                    command: t.command_label.clone(),
                    ports: t.ports.clone(),
                })
                .collect(),
            title: a.title().map(String::from),
            host: a.host.as_ref().map(|h| HostJson {
                kind: h.kind.as_str(),
                target: h.target.clone(),
                outer: h.outer.map(|o| o.as_str()),
            }),
            git: a.git.map(|g| GitJson {
                changed: g.changed,
                untracked: g.untracked,
                insertions: g.insertions,
                deletions: g.deletions,
                ahead: g.ahead,
                behind: g.behind,
            }),
            looks_finished: a.looks_finished(now, servers.len()),
        }
    }
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
            launcher: t.launcher.as_ref().map(LauncherJson::from),
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
    fn who_and_free_take_ports_only() {
        assert_eq!(
            parse_ports("who", &["3000", ":5432", "3000"]),
            Ok(vec![3000, 5432])
        );
        assert_eq!(parse_ports("who", &[]), Err(2));
        assert_eq!(parse_ports("free", &["client-portal"]), Err(2));
        assert_eq!(parse_ports("free", &["0"]), Err(2));
        assert_eq!(parse_ports("free", &["70000"]), Err(2));
        // a non-port selector never reaches the kill path
        assert_eq!(dispatch(&["free".into(), "api".into()]), Some(2));
    }

    #[test]
    fn target_on_matches_ports_exactly() {
        let snap = Snapshot::sample();
        assert_eq!(target_on(&snap, 3000).unwrap().command_label, "next dev");
        assert!(target_on(&snap, 300).is_none()); // no substring match on ports
    }

    #[test]
    fn describe_reads_like_a_row_and_flags_extra_ports_and_exposure() {
        let mut snap = Snapshot::sample();
        let t = &mut snap.targets[1]; // billing-api · uvicorn · :8000, exposed
        t.ports.push(8001);
        let line = describe(t, 8000);
        assert!(
            line.starts_with("billing-api · uvicorn · main · "),
            "{line}"
        );
        assert!(line.contains("also :8001"), "{line}");
        assert!(line.ends_with("LAN-exposed"), "{line}");
        // pid/uptime are omitted when unknown rather than printed as 0
        assert!(!line.contains("pid 0") && !line.contains("up "), "{line}");
        assert_eq!(
            stopped(t, 8000),
            "billing-api · uvicorn (also released :8001)"
        );
    }

    #[test]
    fn who_on_a_free_port_exits_one() {
        // grab an ephemeral port, then release it so it's (almost surely) free
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        assert_eq!(dispatch(&["who".into(), port.to_string()]), Some(1));
        // free is idempotent: nothing to stop is success
        assert_eq!(dispatch(&["free".into(), port.to_string()]), Some(0));
    }

    #[test]
    fn free_refuses_a_holder_that_is_not_a_dev_target() {
        // our own test process: marina never lists its own session, so this
        // listener is held by a non-target — `free` must not kill the test run
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port().to_string();
        assert_eq!(dispatch(&["who".into(), port.clone()]), Some(0));
        assert_eq!(dispatch(&["free".into(), port]), Some(1));
        drop(l);
    }

    #[test]
    fn port_usage_errors_exit_two_before_touching_the_registry() {
        let d = |a: &[&str]| dispatch(&a.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(d(&["port", "web", "api"]), Some(2)); // one name at most
        assert_eq!(d(&["port", "a/b"]), Some(2)); // invalid name
        assert_eq!(d(&["port", "--list", "--release"]), Some(2));
        assert_eq!(d(&["port", "--mine"]), Some(2));
        assert_eq!(d(&["ls", "--list"]), Some(2)); // port-only flags
        assert_eq!(d(&["kill", "x", "--release"]), Some(2));
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
            launcher: None,
        };
        let j = TargetJson::from(&t);
        assert_eq!(j.kind, "listener");
        assert!(j.cpu_pct.is_none() && j.mem_bytes.is_none() && j.uptime_secs.is_none());
        assert_eq!(j.ports, vec![5432]);
        assert!(j.exposed);
        assert_eq!(j.container.as_deref(), Some("myapp-db-1"));
    }

    #[test]
    fn agents_match_by_name_project_and_session_prefix_and_json_is_pinned() {
        let a = crate::model::Agent::sample(10, "41616c1a-c2d3", "marina");
        assert!(agent_matches(&a, "claude"));
        assert!(agent_matches(&a, "mari"));
        assert!(agent_matches(&a, "41616c"));
        assert!(!agent_matches(&a, "opencode"));

        let mut snap = Snapshot::sample();
        snap.targets[0].launcher = Some(a.as_launcher()); // :3000 is its server
        let j = serde_json::to_value(AgentJson::new(&a, &servers_of(&snap, &a))).unwrap();
        let obj = j.as_object().unwrap();
        for field in [
            "name",
            "pid",
            "session",
            "resume",
            "state",
            "cwd",
            "project",
            "branch",
            "uptime_secs",
            "cpu_pct",
            "mem_bytes",
            "pids",
            "shells",
            "helpers",
            "servers",
            "title",
            "host",
            "git",
            "looks_finished",
        ] {
            assert!(obj.contains_key(field), "missing JSON field {field}");
        }
        assert_eq!(obj.len(), 19);
        assert_eq!(j["resume"], "claude --resume 41616c1a-c2d3");
        assert_eq!(j["shells"][0]["command"], "cargo test");
        assert_eq!(j["servers"][0]["ports"][0], 3000);
    }

    #[test]
    fn agents_take_no_scope_flags() {
        assert_eq!(dispatch(&["agents".into(), "--orphaned".into()]), Some(2));
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
            "launcher",
        ] {
            assert!(obj.contains_key(field), "missing JSON field {field}");
        }
        assert_eq!(obj.len(), 15, "unexpected extra/removed JSON fields");
    }
}
