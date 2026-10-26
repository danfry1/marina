//! Non-interactive CLI — the resolution engine exposed for scripts and agents.
//!
//!   marina ls [sel]… [--json]   list running dev servers (optionally filtered)
//!   marina kill <sel>… [--json] SIGTERM → verified SIGKILL matching servers
//!   marina restart <sel>…       restart matching servers (output captured)
//!   marina url [sel]… [--json]  print matching servers' URLs
//!   marina open [sel]… [--mine] open matching servers in the browser
//!   marina who <port>… [--json] what is holding a port (dev server or not)
//!   marina free <port>… [--json] stop whatever dev server holds a port
//!   marina port [name] [--json]  a stable free port for this project/worktree
//!   marina port --list | --release [name]
//!   marina agents [sel]… [--json] running coding-agent sessions + their shells
//!   marina demo                 the TUI on made-up servers (handled in main)
//!   marina version              print the version
//!
//! A <selector> is a port (`3000` or `:3000`, ports only), or a name: an
//! exact project name, else an exact command label, else a substring of
//! either (case-insensitive). Killing a project name takes down every server
//! under it — the grouping primitive, via the CLI. A name that reaches
//! servers in more than one project without naming one is ambiguous: `kill`,
//! `restart` and `open` refuse it (exit 2) unless `--all` is given.
//! Docker targets are stopped/restarted via `docker stop`/`docker restart`.
//!
//! `--mine` narrows ls/kill/restart/url/open to servers started by the agent
//! session running this command, in the git worktree it's run from (so an
//! agent can clean up after itself, and subagents — which share their
//! parent's session — in other worktrees are left alone); `--session` drops
//! the worktree limit; `--ended` narrows to servers whose launching agent
//! session has ended.
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
    marina ls [sel]... [--json]  list running dev servers
    marina kill <sel>... [--json] [--grace <secs>]
                                 stop matching servers (SIGTERM, then SIGKILL
                                 after the grace period, default 5s)
    marina restart <sel>...      restart matching servers (output captured)
    marina url [sel]... [--json] print matching servers' URLs
                                 (no selector: the ones running in this dir)
    marina open [sel]... [--mine]  open matching servers in the browser
                                 (no selector: the ones running in this dir)
    marina who <port>... [--json]  what is holding a port
    marina free <port>... [--json] stop the dev server holding a port
    marina port [name] [--json]  stable free port for this project/worktree
                                 (pnpm dev --port $(marina port))
    marina port --list           show assigned ports
    marina port --release [name] give this project's port back
    marina agents [sel]... [--json]  coding-agent sessions, their shells + servers
    marina demo                  the TUI on a made-up machine — try the keys
    marina version               print the version
    marina <command> --help      this help

SELECTOR:
    a port (3000 or :3000) matches that port only. A name matches an exact
    project name, else an exact command, else part of either. kill, restart
    and open refuse a name that reaches more than one project (--all to go
    ahead). `who` and `free` take ports only.

SCOPE (ls / kill / restart / url / open; usable instead of, or with, selectors):
    --mine       only servers started by the agent session running marina,
                 in this git worktree (subagents share their parent's session)
    --session    like --mine, in every worktree
    --ended      only servers whose launching agent session has ended
                 (`detached` servers, whose launcher is unknown, aren't included)

EXIT CODES:
    0 ok · 1 a selector matched nothing, or a server didn't stop
    2 usage error, or an ambiguous selector · 3 the port scan failed
";

/// Dispatch a CLI subcommand. Returns `Some(exit_code)` if it handled the
/// invocation, `None` when there were no args (caller launches the TUI).
/// Exit codes: 0 ok, 1 no match (or a partial failure), 2 usage error or an
/// ambiguous selector, 3 the port scan failed — so scripts/agents can branch.
pub fn dispatch(args: &[String]) -> Option<i32> {
    let cmd = args.first()?.as_str();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        print!("{USAGE}");
        return Some(0);
    }
    // `--grace <secs>` is the one flag with a value
    let mut rest: Vec<&str> = Vec::new();
    let mut grace = None;
    let mut it = args[1..].iter().map(String::as_str);
    while let Some(a) = it.next() {
        let value = match a.strip_prefix("--grace") {
            Some("") => it.next(),
            Some(v) if v.starts_with('=') => Some(&v[1..]),
            _ => {
                rest.push(a);
                continue;
            }
        };
        match value
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|g| *g >= 0.0)
        {
            Some(g) => grace = Some(Duration::from_secs_f64(g)),
            None => {
                eprintln!("marina: --grace takes a number of seconds (--grace 10)");
                return Some(2);
            }
        }
    }
    let flags: Vec<&str> = rest
        .iter()
        .copied()
        .filter(|s| s.starts_with('-'))
        .collect();
    let selectors: Vec<&str> = rest
        .iter()
        .copied()
        .filter(|s| !s.starts_with('-'))
        .collect();
    if let Some(bad) = flags.iter().find(|f| {
        !matches!(
            **f,
            // --orphaned: the old name for --ended
            "--json"
                | "--mine"
                | "--session"
                | "--ended"
                | "--orphaned"
                | "--all"
                | "--list"
                | "--release"
        )
    }) {
        eprintln!("marina: unknown flag {bad:?}\n");
        eprint!("{USAGE}");
        return Some(2);
    }
    let json = flags.contains(&"--json");
    let all = flags.contains(&"--all");
    let mut scope = Scope {
        mine: None,
        tree: None,
        ended: flags.contains(&"--ended") || flags.contains(&"--orphaned"),
    };
    let session = flags.contains(&"--session");
    if session || flags.contains(&"--mine") {
        let flag = if session { "--session" } else { "--mine" };
        match launcher::own_agent() {
            Some(own) => scope.mine = Some(own),
            None => {
                eprintln!("marina: {flag}: not running inside an agent session");
                return Some(2);
            }
        }
        // --session widens --mine, so both together mean --session. Without
        // a worktree --mine can't tell this agent from its subagents, so it
        // refuses rather than quietly reaching the whole session.
        if !session {
            let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
            scope.tree = std::env::current_dir()
                .ok()
                .and_then(|d| crate::resolve::worktree_root(&d, home.as_deref()));
            if scope.tree.is_none() {
                eprintln!("marina: --mine: not inside a git worktree (--session for every server this session started)");
                return Some(2);
            }
        }
    }
    if scope.active() && matches!(cmd, "who" | "free" | "port" | "agents") {
        eprintln!("marina: {cmd} doesn't take --mine / --session / --ended");
        return Some(2);
    }
    if json && cmd == "open" {
        eprintln!("marina: open doesn't take --json (marina url --json prints the URLs)");
        return Some(2);
    }
    if json && matches!(cmd, "restart" | "version") {
        eprintln!("marina: {cmd} doesn't take --json");
        return Some(2);
    }
    if all && !matches!(cmd, "kill" | "restart" | "open") {
        eprintln!("marina: --all only applies to kill, restart and open");
        return Some(2);
    }
    if grace.is_some() && cmd != "kill" {
        eprintln!("marina: --grace only applies to `marina kill`");
        return Some(2);
    }
    let (list, release) = (flags.contains(&"--list"), flags.contains(&"--release"));
    if (list || release) && cmd != "port" {
        eprintln!("marina: --list / --release only apply to `marina port`");
        return Some(2);
    }
    let pick = Pick {
        selectors: &selectors,
        scope: &scope,
        all,
    };
    let code = match cmd {
        "ls" => ls(json, &pick),
        "kill" => kill(&pick, json, grace.unwrap_or(Duration::from_secs(5))),
        "restart" => restart(&pick),
        "url" => url(&pick, json),
        "open" => open(&pick),
        "who" => who(&selectors, json),
        "free" => free(&selectors, json),
        "port" => port(&selectors, json, list, release),
        "agents" => agents(&selectors, json),
        "version" | "--version" | "-V" => {
            println!("marina {}", env!("CARGO_PKG_VERSION"));
            0
        }
        "help" => {
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

/// A snapshot to act on, or exit 3 when the port scan failed — "nothing is
/// running" must never be reported on the strength of a scan that didn't
/// happen.
fn scanned(cmd: &str, with_cpu: bool) -> Result<Snapshot, i32> {
    let snap = snapshot(with_cpu);
    match &snap.error {
        Some(e) => {
            eprintln!("marina {cmd}: couldn't scan ports ({e}) — not reporting a partial list");
            Err(3)
        }
        None => Ok(snap),
    }
}

/// `--mine` / `--session` / `--ended`: narrow by who launched a target.
struct Scope {
    /// The agent session running this command (pid, session id).
    mine: Option<(Option<u32>, Option<String>)>,
    /// `--mine`: the caller's git worktree. Subagents share their parent's
    /// session, so this is what keeps one worktree's agent off another's
    /// servers. `None` for `--session` (`--mine` refuses to run without one).
    tree: Option<std::path::PathBuf>,
    ended: bool,
}

impl Scope {
    fn active(&self) -> bool {
        self.mine.is_some() || self.ended
    }

    fn keep(&self, t: &Target) -> bool {
        let l = t.launcher.as_ref();
        if let Some(own) = &self.mine {
            if !l.is_some_and(|l| launcher::same_session(l, own)) {
                return false;
            }
            if let Some(tree) = &self.tree {
                let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
                if crate::resolve::worktree_root(&t.cwd, home.as_deref()).as_ref() != Some(tree) {
                    return false;
                }
            }
        }
        !self.ended || l.is_some_and(|l| l.is_orphaned())
    }

    /// `started by this session`, for "nothing to kill (no servers …)".
    fn describe(&self) -> &'static str {
        match (self.mine.is_some(), self.ended) {
            (true, true) => "in that scope",
            (true, false) if self.tree.is_some() => "started by this session in this worktree",
            (true, false) => "started by this session",
            _ => "left running by ended agent sessions",
        }
    }
}

/// A command's selectors and scope.
struct Pick<'a> {
    selectors: &'a [&'a str],
    scope: &'a Scope,
    /// `--all`: an ambiguous name acts on everything it matches.
    all: bool,
}

/// What a set of selectors picked.
#[derive(Default)]
struct Picked<'a> {
    targets: Vec<&'a Target>,
    /// Selectors that matched nothing in scope.
    unmatched: Vec<String>,
    /// Names that reach servers in several projects without naming one.
    ambiguous: Vec<(String, Vec<&'a Target>)>,
}

impl Picked<'_> {
    /// Name each selector that matched nothing; 1 if any did.
    fn report_unmatched(&self, cmd: &str) -> i32 {
        for sel in &self.unmatched {
            eprintln!("marina {cmd}: nothing matches {sel:?}");
        }
        i32::from(!self.unmatched.is_empty())
    }

    /// Refuse an ambiguous selector, listing what it reaches (exit 2).
    fn refuse_ambiguous(&self, cmd: &str) -> Option<i32> {
        if self.ambiguous.is_empty() {
            return None;
        }
        for (sel, ts) in &self.ambiguous {
            let mut projects: Vec<&str> = ts.iter().map(|t| t.project.as_str()).collect();
            projects.dedup();
            eprintln!(
                "marina {cmd}: {sel:?} matches servers in {} projects — name one, or add --all:",
                projects.len()
            );
            for t in ts {
                let port = t.ports.first().map(|p| format!(":{p}")).unwrap_or_default();
                eprintln!("  {:<20} {:<16} {port}", t.project, t.command_label);
            }
        }
        Some(2)
    }
}

impl Pick<'_> {
    /// Selector + scope matching. No selectors means "everything in scope"
    /// when a scope flag is given (`kill --ended`), or with `all_if_empty`
    /// (`ls`); otherwise nothing.
    fn on<'a>(&self, snap: &'a Snapshot, all_if_empty: bool) -> Picked<'a> {
        if self.selectors.is_empty() {
            let targets = if all_if_empty || self.scope.active() {
                snap.targets.iter().filter(|t| self.scope.keep(t)).collect()
            } else {
                Vec::new()
            };
            return Picked {
                targets,
                ..Picked::default()
            };
        }
        let mut out = Picked::default();
        let mut keys = Vec::new();
        for &sel in self.selectors {
            let hits: Vec<&Target> = matching(snap, sel)
                .into_iter()
                .filter(|t| self.scope.keep(t))
                .collect();
            if hits.is_empty() {
                out.unmatched.push(sel.to_string());
                continue;
            }
            keys.extend(hits.iter().map(|t| &t.key));
            if !self.all && is_ambiguous(sel, &hits) {
                out.ambiguous.push((sel.to_string(), hits));
            }
        }
        // snapshot order, each server once
        out.targets = snap
            .targets
            .iter()
            .filter(|t| keys.contains(&&t.key))
            .collect();
        out
    }
}

/// A selector that's a port: `3000` or `:3000`.
fn as_port(sel: &str) -> Option<u16> {
    sel.strip_prefix(':').unwrap_or(sel).parse().ok()
}

/// The servers one selector names: a port's server; else the projects of
/// that exact name; else servers running that exact command; else any whose
/// project or command contains it.
fn matching<'a>(snap: &'a Snapshot, sel: &str) -> Vec<&'a Target> {
    if let Some(port) = as_port(sel) {
        return snap
            .targets
            .iter()
            .filter(|t| t.ports.contains(&port))
            .collect();
    }
    let q = sel.to_lowercase();
    let by = |f: &dyn Fn(&Target) -> bool| -> Vec<&'a Target> {
        snap.targets.iter().filter(|t| f(t)).collect()
    };
    let project = by(&|t| t.project.to_lowercase() == q);
    if !project.is_empty() {
        return project;
    }
    let command = by(&|t| t.command_label.to_lowercase() == q);
    if !command.is_empty() {
        return command;
    }
    by(&|t| t.project.to_lowercase().contains(&q) || t.command_label.to_lowercase().contains(&q))
}

/// A name whose servers span several projects, none of them named by it
/// exactly (`api` → `api-gateway`, `rapid-ui`; `vite` → three projects).
fn is_ambiguous(sel: &str, hits: &[&Target]) -> bool {
    if as_port(sel).is_some() || hits.iter().any(|t| t.project.eq_ignore_ascii_case(sel)) {
        return false;
    }
    let first = &hits[0].project;
    hits.iter().any(|t| &t.project != first)
}

#[cfg(test)]
fn select<'a>(snap: &'a Snapshot, selectors: &[&str]) -> Vec<&'a Target> {
    let scope = Scope {
        mine: None,
        tree: None,
        ended: false,
    };
    Pick {
        selectors,
        scope: &scope,
        all: true,
    }
    .on(snap, false)
    .targets
}

// --- handlers ---------------------------------------------------------------

fn ls(json: bool, pick: &Pick) -> i32 {
    let snap = match scanned("ls", true) {
        Ok(s) => s,
        Err(code) => return code,
    };
    let picked = pick.on(&snap, true);
    let code = picked.report_unmatched("ls");
    let targets = &picked.targets;
    if json {
        let view: Vec<TargetJson> = targets.iter().copied().map(TargetJson::from).collect();
        match serde_json::to_string_pretty(&view) {
            Ok(s) => println!("{s}"),
            Err(e) => eprintln!("marina: json error: {e}"),
        }
        return code;
    }
    if targets.is_empty() {
        if pick.selectors.is_empty() {
            match pick.scope.active() {
                true => println!("no dev servers {}", pick.scope.describe()),
                false => println!("no dev servers running"),
            }
        }
        return code;
    }
    println!(
        "{:<20} {:<16} {:<8} {:>6} {:>8} {:<14} {:<14}",
        "PROJECT", "COMMAND", "PORT", "CPU", "MEM", "VIA", "URL"
    );
    for t in targets {
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
    code
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
    let snap = match scanned("agents", true) {
        Ok(s) => s,
        Err(code) => return code,
    };
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

/// What `kill` did to one server, for `kill --json`'s `result`.
fn kill_result(t: &Target, report: &verbs::KillReport, docker_ok: Option<bool>) -> &'static str {
    if let Some(ok) = docker_ok {
        return if ok { "terminated" } else { "still_running" };
    }
    let has = |pids: &[u32]| t.pid_starts.iter().any(|(p, _)| pids.contains(p));
    if has(&report.denied) {
        "denied"
    } else if has(&report.survivors) {
        "still_running"
    } else if has(&report.forced) {
        "force_killed"
    } else {
        "terminated"
    }
}

#[derive(Serialize)]
struct KillJson {
    #[serde(flatten)]
    target: TargetJson,
    /// `terminated` | `force_killed` | `still_running` | `denied`.
    result: &'static str,
}

fn kill(pick: &Pick, json: bool, grace: Duration) -> i32 {
    if pick.selectors.is_empty() && !pick.scope.active() {
        eprintln!("marina kill: need a selector (project, port, or command) or --mine / --ended");
        return 2;
    }
    let snap = match scanned("kill", false) {
        Ok(s) => s,
        Err(code) => return code,
    };
    let picked = pick.on(&snap, false);
    if let Some(code) = picked.refuse_ambiguous("kill") {
        return code;
    }
    let mut code = picked.report_unmatched("kill");
    let targets = &picked.targets;
    if targets.is_empty() {
        if pick.selectors.is_empty() {
            // nothing in scope is already the outcome asked for
            eprintln!("nothing to kill (no servers {})", pick.scope.describe());
        }
        if json {
            println!("[]");
        }
        return code;
    }
    let mut pid_starts: Vec<verbs::PidStart> = Vec::new();
    let mut docker_ok = std::collections::HashMap::new();
    for t in targets {
        let port = t
            .ports
            .first()
            .map(|p| format!(" :{p}"))
            .unwrap_or_default();
        if !json {
            println!("stopping {} · {}{port}", t.project, t.command_label);
        }
        match &t.container {
            Some(c) => {
                let ok = std::process::Command::new("docker")
                    .args(["stop", c])
                    .stdout(std::process::Stdio::null())
                    .status()
                    .is_ok_and(|s| s.success());
                docker_ok.insert(&t.key, ok);
            }
            None => pid_starts.extend(&t.pid_starts),
        }
    }
    // verified SIGTERM -> wait -> verified SIGKILL (recycled pids skipped)
    let report = if pid_starts.is_empty() {
        verbs::KillReport::default()
    } else {
        verbs::kill_blocking(&pid_starts, grace)
    };
    let results: Vec<&'static str> = targets
        .iter()
        .map(|t| kill_result(t, &report, docker_ok.get(&t.key).copied()))
        .collect();
    let stopped = results
        .iter()
        .filter(|r| matches!(**r, "terminated" | "force_killed"))
        .count();
    if stopped < targets.len() {
        code = 1;
    }
    if json {
        let view: Vec<KillJson> = targets
            .iter()
            .zip(&results)
            .map(|(t, &result)| KillJson {
                target: TargetJson::from(*t),
                result,
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&view).unwrap_or_default()
        );
        return code;
    }
    for (t, r) in targets.iter().zip(&results) {
        let why = match *r {
            "force_killed" => format!(
                "force-killed (still running after {}s)",
                grace.as_secs_f64()
            ),
            "still_running" => "still running".into(),
            "denied" => "permission denied — not your process?".into(),
            _ => continue,
        };
        let port = t
            .ports
            .first()
            .map(|p| format!(" :{p}"))
            .unwrap_or_default();
        eprintln!("{} · {}{port}: {why}", t.project, t.command_label);
    }
    match stopped == targets.len() {
        true => println!("stopped {}", plural(stopped, "server")),
        false => println!("stopped {stopped} of {}", plural(targets.len(), "server")),
    }
    code
}

/// `3 servers`, `1 server`.
fn plural(n: usize, what: &str) -> String {
    format!("{n} {what}{}", if n == 1 { "" } else { "s" })
}

fn restart(pick: &Pick) -> i32 {
    if pick.selectors.is_empty() && !pick.scope.active() {
        eprintln!("marina restart: need a selector or --mine / --ended");
        return 2;
    }
    let snap = match scanned("restart", false) {
        Ok(s) => s,
        Err(code) => return code,
    };
    let picked = pick.on(&snap, false);
    if let Some(code) = picked.refuse_ambiguous("restart") {
        return code;
    }
    let mut code = picked.report_unmatched("restart");
    let targets = &picked.targets;
    if targets.is_empty() {
        if pick.selectors.is_empty() {
            eprintln!("nothing to restart (no servers {})", pick.scope.describe());
        }
        return code;
    }
    // Capture commands, terminate everything, then re-exec.
    let mut plans: Vec<(String, Vec<String>, std::path::PathBuf)> = Vec::new();
    let mut pid_starts: Vec<verbs::PidStart> = Vec::new();
    for t in targets {
        if let Some(c) = &t.container {
            let ok = std::process::Command::new("docker")
                .args(["restart", c])
                .stdout(std::process::Stdio::null())
                .status()
                .is_ok_and(|s| s.success());
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
            eprintln!("skipping {}: its command wasn't captured", t.project);
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
    let mut restarted = false;
    for (project, argv, cwd) in plans {
        let log = crate::logs::state_log_path(&project);
        match verbs::respawn(&argv, &cwd, log.as_deref()) {
            Ok(_child) => {
                restarted = true;
                match &log {
                    Some(p) => println!("restarted {project} (output -> {})", p.display()),
                    None => println!("restarted {project}"),
                }
            }
            Err(e) => {
                eprintln!("restart {project} failed: {e}");
                code = 1;
            }
        }
    }
    if restarted {
        eprintln!(
            "marina: restarted without the original environment — a server that needs env vars set by its old shell may fail; check its log"
        );
    }
    code
}

/// The servers running in (or under) the current directory, or in a
/// project this directory is inside (`proj/src` finds proj's server) —
/// what a bare `url` / `open` means from a project's terminal. Errs (exit 2)
/// from `~` or `/`, where that's everything.
fn running_here<'a>(cmd: &str, targets: Vec<&'a Target>) -> Result<Vec<&'a Target>, i32> {
    let here = std::env::current_dir().ok();
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    if here
        .as_ref()
        .is_some_and(|h| h.parent().is_none() || Some(h) == home.as_ref())
    {
        eprintln!(
            "marina {cmd}: from here that's everything — name one (marina {cmd} api, marina {cmd} 3000)"
        );
        return Err(2);
    }
    Ok(targets
        .into_iter()
        .filter(|t| {
            here.as_ref().is_some_and(|h| {
                !t.cwd.as_os_str().is_empty() && (t.cwd.starts_with(h) || h.starts_with(&t.cwd))
            })
        })
        .collect())
}

fn url(pick: &Pick, json: bool) -> i32 {
    let snap = match scanned("url", false) {
        Ok(s) => s,
        Err(code) => return code,
    };
    let bare = pick.selectors.is_empty() && !pick.scope.active();
    let picked = pick.on(&snap, bare);
    let code = picked.report_unmatched("url");
    let targets = if bare {
        match running_here("url", picked.targets) {
            Ok(t) => t,
            Err(code) => return code,
        }
    } else {
        picked.targets
    };
    if targets.is_empty() {
        if bare {
            eprintln!(
                "marina url: no server running here — name one (marina url api, marina url 3000)"
            );
            return 1;
        }
        if pick.selectors.is_empty() {
            eprintln!("no servers {}", pick.scope.describe());
        }
        if json {
            println!("[]");
        }
        return code;
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
        return code;
    }
    for t in targets {
        match &t.url {
            Some(u) => println!("{}\t{}", t.project, u.value),
            None => println!("{}\t—", t.project),
        }
    }
    code
}

/// `marina open [sel]`: open matching servers' web URLs in the browser. With
/// no selector or scope, the servers running in (or under) the current
/// directory — what you mean from a project's terminal. More than three
/// are listed instead of opened, unless `--all`.
fn open(pick: &Pick) -> i32 {
    let snap = match scanned("open", false) {
        Ok(s) => s,
        Err(code) => return code,
    };
    let bare = pick.selectors.is_empty() && !pick.scope.active();
    let picked = pick.on(&snap, bare);
    if let Some(code) = picked.refuse_ambiguous("open") {
        return code;
    }
    let code = picked.report_unmatched("open");
    let targets = if bare {
        match running_here("open", picked.targets) {
            Ok(t) => t,
            Err(code) => return code,
        }
    } else {
        picked.targets
    };
    let web: Vec<&Target> = targets
        .iter()
        .copied()
        .filter(|t| t.url.as_ref().is_some_and(|u| u.scheme.is_web()))
        .collect();
    // a tab per match gets out of hand: list them, don't open them
    if web.len() > 3 && !pick.all {
        eprintln!(
            "{} web servers {} — name one, or add --all:",
            web.len(),
            if bare { "run under here" } else { "match" }
        );
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
        if !targets.is_empty() || bare || pick.selectors.is_empty() {
            eprintln!(
                "{}",
                match (pick.selectors.is_empty(), pick.scope.active()) {
                    (true, false) => "no web server running here — name one (marina open api, marina open 3000) or use --mine",
                    _ => "no web server matches",
                }
            );
        }
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
    code.max(i32::from(failed))
}

// --- ports: who / free ------------------------------------------------------

/// `who`/`free` take ports only — a project substring like `api` silently
/// matching several servers is fine for `ls`, not for "what's on this port".
fn parse_ports(cmd: &str, selectors: &[&str]) -> Result<Vec<u16>, i32> {
    if selectors.is_empty() {
        eprintln!("marina {cmd}: need a port (3000 or :3000)");
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
                eprintln!(
                    "marina {cmd}: {sel:?} is not a port (use `marina ls {sel}` to match by name)"
                );
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
    let snap = match scanned("who", false) {
        Ok(s) => s,
        Err(code) => return code,
    };
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
                _ => format!("{} — not a dev server", describe_holders(&processes)),
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
    let snap = match scanned("free", false) {
        Ok(s) => s,
        Err(code) => return code,
    };

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
                    "held by {} — not a dev server, not touched",
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
    /// Started by an agent session that has since ended (`--ended`).
    ended: bool,
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
            ended: l.is_orphaned(),
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
    fn mine_is_this_worktree_and_session_is_every_worktree() {
        // a repo with a subagent worktree nested inside it, as agent tools
        // lay them out (.claude/worktrees/<name>)
        let base = std::env::temp_dir().join(format!("marina-mine-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let repo = base.join("repo");
        let wt = repo.join(".claude/worktrees/a");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(wt.join("web")).unwrap();
        std::fs::write(wt.join(".git"), "gitdir: elsewhere\n").unwrap();

        let agent = |session: &str| launcher::Launcher {
            kind: launcher::LauncherKind::Agent,
            name: "claude".into(),
            pid: Some(20),
            alive: true,
            session: Some(session.into()),
            cwd: None,
            start_time: 0,
        };
        let t = Snapshot::sample().targets[0].clone();
        let at = |cwd: &std::path::Path, l| Target {
            cwd: cwd.to_path_buf(),
            launcher: Some(l),
            ..t.clone()
        };
        let parent = at(&repo, agent("s1"));
        let sub = at(&wt.join("web"), agent("s1")); // subagent: same session
        let other = at(&repo, agent("s2"));

        let own = Some((None, Some("s1".to_string())));
        let mine_in = |tree: &std::path::Path| Scope {
            mine: own.clone(),
            tree: crate::resolve::worktree_root(tree, None),
            ended: false,
        };
        let keeps = |s: &Scope| [&parent, &sub, &other].map(|t| s.keep(t));
        assert_eq!(keeps(&mine_in(&repo)), [true, false, false]);
        assert_eq!(keeps(&mine_in(&wt)), [false, true, false]);
        let session = Scope {
            mine: own.clone(),
            tree: None,
            ended: false,
        };
        assert_eq!(keeps(&session), [true, true, false]);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The sample plus `api` (:4000), `api-gateway` (:4001) and a project
    /// named `app-3000` (:4002).
    fn named_snapshot() -> Snapshot {
        let mut snap = Snapshot::sample();
        for (name, port) in [("api", 4000), ("api-gateway", 4001), ("app-3000", 4002)] {
            let mut t = snap.targets[0].clone();
            t.key = crate::model::TargetKey::Port(port);
            t.ports = vec![port];
            t.project = name.into();
            t.command_label = "vite".into();
            snap.targets.push(t);
        }
        snap
    }

    fn pick_with<'a>(snap: &'a Snapshot, selectors: &'a [&'a str]) -> Picked<'a> {
        let scope = Scope {
            mine: None,
            tree: None,
            ended: false,
        };
        Pick {
            selectors,
            scope: &scope,
            all: false,
        }
        .on(snap, false)
    }

    #[test]
    fn an_exact_name_wins_and_a_port_matches_only_ports() {
        let snap = named_snapshot();
        let p = pick_with(&snap, &["api"]);
        assert_eq!(p.targets.len(), 1, "api, not api-gateway");
        assert!(p.ambiguous.is_empty());
        // nothing on :3001, and a project named app-3000 isn't a port match
        let p = pick_with(&snap, &["3001"]);
        assert!(p.targets.is_empty());
        assert_eq!(p.unmatched, vec!["3001".to_string()]);
        assert_eq!(pick_with(&snap, &["3000"]).targets[0].ports, vec![3000]);
    }

    #[test]
    fn a_name_reaching_several_projects_is_ambiguous() {
        let snap = named_snapshot();
        // a substring: api-gateway, and the `api` it doesn't name exactly
        let p = pick_with(&snap, &["gate"]);
        assert!(p.ambiguous.is_empty(), "one project is fine");
        let p = pick_with(&snap, &["ap"]);
        assert_eq!(p.ambiguous.len(), 1);
        assert_eq!(p.refuse_ambiguous("kill"), Some(2));
        // an exact command across projects is ambiguous too
        assert_eq!(pick_with(&snap, &["vite"]).ambiguous.len(), 1);
        // --all goes ahead
        assert_eq!(select(&snap, &["vite"]).len(), 3);
    }

    #[test]
    fn help_anywhere_prints_usage_and_flags_are_checked_per_command() {
        let d = |a: &[&str]| dispatch(&a.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(d(&["ls", "--help"]), Some(0));
        assert_eq!(d(&["kill", "api", "-h"]), Some(0));
        assert_eq!(
            d(&["ls", "--all"]),
            Some(2),
            "--all is for kill/restart/open"
        );
        assert_eq!(d(&["ls", "--grace", "3"]), Some(2), "--grace is for kill");
        assert_eq!(
            d(&["kill", "x", "--grace"]),
            Some(2),
            "--grace needs seconds"
        );
        assert_eq!(d(&["kill", "x", "--grace=soon"]), Some(2));
        assert_eq!(d(&["open", "x", "--json"]), Some(2), "open has no JSON");
        assert_eq!(d(&["restart", "x", "--json"]), Some(2), "nor restart");
        assert_eq!(d(&["version", "--json"]), Some(2), "nor version");
        assert_eq!(
            d(&["ls", "--dump"]),
            Some(2),
            "debug flags are first-position only"
        );
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
        assert_eq!(d(&["port", "--session"]), Some(2));
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
        assert_eq!(dispatch(&["agents".into(), "--ended".into()]), Some(2));
        assert_eq!(dispatch(&["agents".into(), "--orphaned".into()]), Some(2));
    }

    #[test]
    fn kill_results_name_what_happened_to_each_server() {
        let snap = Snapshot::sample();
        let mut t = snap.targets[0].clone();
        t.pid_starts = vec![(10, 1), (11, 1)];
        let mut r = verbs::KillReport::default();
        assert_eq!(kill_result(&t, &r, None), "terminated");
        r.forced = vec![11];
        assert_eq!(kill_result(&t, &r, None), "force_killed");
        r.survivors = vec![11];
        assert_eq!(kill_result(&t, &r, None), "still_running");
        r.denied = vec![10];
        assert_eq!(kill_result(&t, &r, None), "denied");
        assert_eq!(kill_result(&t, &r, Some(true)), "terminated", "docker stop");
        let j = serde_json::to_value(KillJson {
            target: TargetJson::from(&t),
            result: "terminated",
        })
        .unwrap();
        assert_eq!(j["result"], "terminated");
        assert_eq!(
            j["project"], "client-portal",
            "the target's fields, flattened"
        );
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
        let l = launcher::Launcher {
            kind: launcher::LauncherKind::Agent,
            name: "claude".into(),
            pid: Some(1),
            alive: false,
            session: None,
            cwd: None,
            start_time: 0,
        };
        let j = serde_json::to_value(LauncherJson::from(&l)).unwrap();
        assert_eq!(j["ended"], true, "named like the TUI's claude·ended");
    }
}
