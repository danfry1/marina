//! Launcher attribution: *who started this server?* — a coding agent session,
//! an editor, a terminal, or nobody any more (detached).
//!
//! Two signals, in order of trust:
//!
//! 1. **The live parent chain.** Walking up from the anchor, the nearest agent
//!    wins outright (an agent inside a terminal inside an editor is still the
//!    agent's server); otherwise the nearest editor, then terminal/multiplexer.
//! 2. **Agent marker env vars**, for when the chain is gone. Agents background
//!    servers with `&`/`nohup`; once that shell exits the server is reparented
//!    to pid 1 and the chain says nothing. Claude Code exports `CLAUDE_PID` and
//!    `CLAUDE_CODE_SESSION_ID` (and the cross-agent `AI_AGENT`) to everything it
//!    spawns, and env survives reparenting — so a detached server can still be
//!    traced to its session, and flagged as **orphaned** once that session ends.
//!
//! Only an allowlist of non-secret keys is ever read from a process's env
//! (see [`AgentEnv::from_environ`]); nothing else is retained.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;

use crate::sources::ProcInfo;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LauncherKind {
    Agent,
    Editor,
    Terminal,
    /// Reparented to pid 1 with no agent marker — nothing owns it any more
    /// (or it daemonized itself).
    Detached,
}

impl LauncherKind {
    pub fn as_str(self) -> &'static str {
        match self {
            LauncherKind::Agent => "agent",
            LauncherKind::Editor => "editor",
            LauncherKind::Terminal => "terminal",
            LauncherKind::Detached => "detached",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Launcher {
    pub kind: LauncherKind,
    /// `claude`, `codex`, `cursor`, `tmux`, … (`detached` for Detached).
    pub name: String,
    /// The launching process (agent / editor / terminal), if known.
    pub pid: Option<u32>,
    /// The launching process is still running. `false` for an agent means the
    /// session ended and left this server behind — an orphan.
    pub alive: bool,
    /// Agent session id (Claude Code: resumable with `claude --resume <id>`).
    pub session: Option<String>,
    /// Where the launching process runs — tells agent sessions apart.
    pub cwd: Option<PathBuf>,
    pub start_time: u64,
}

impl Launcher {
    pub fn is_orphaned(&self) -> bool {
        self.kind == LauncherKind::Agent && !self.alive
    }

    /// Column-width form: `claude`, `tmux`, `detached`, `claude·ended`.
    pub fn short(&self) -> String {
        if self.is_orphaned() {
            format!("{}·ended", self.name)
        } else {
            self.name.clone()
        }
    }

    /// One-line detail: `claude (pid 35115, ~/dev/app, up 2d, session 219af9f5)`.
    pub fn describe(&self) -> String {
        if self.kind == LauncherKind::Detached {
            return "detached (its launching shell exited)".into();
        }
        let short_session = self.session.as_deref().map(|s| &s[..s.len().min(8)]);
        if self.is_orphaned() {
            return match short_session {
                Some(s) => format!("{} (session {s} ended)", self.name),
                None => format!("{} (session ended)", self.name),
            };
        }
        let mut parts = Vec::new();
        if let Some(pid) = self.pid {
            parts.push(format!("pid {pid}"));
        }
        if let Some(cwd) = &self.cwd {
            parts.push(crate::ui::tildify(&cwd.display().to_string()));
        }
        if self.start_time != 0 {
            parts.push(format!("up {}", crate::ui::fmt_uptime(self.start_time)));
        }
        if let Some(s) = short_session {
            parts.push(format!("session {s}"));
        }
        if parts.is_empty() {
            self.name.clone()
        } else {
            format!("{} ({})", self.name, parts.join(", "))
        }
    }

    /// How to get back into the session that started this, when we know.
    pub fn resume_hint(&self) -> Option<String> {
        match (&self.session, self.name.as_str()) {
            (Some(s), "claude") => Some(format!("claude --resume {s}")),
            (Some(s), "codex") => Some(format!("codex resume {s}")),
            (Some(s), "opencode") => Some(format!("opencode --session {s}")),
            _ => None,
        }
    }
}

/// The allowlisted agent markers from one process's environment.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AgentEnv {
    pub agent: Option<String>,
    pub pid: Option<u32>,
    pub session: Option<String>,
}

/// What a marker variable tells us. Only `Pid` and `Session` values are ever
/// kept (parsed / validated); `Flag` values are never read.
#[derive(Clone, Copy, PartialEq)]
enum Role {
    Flag,
    Session,
    Pid,
}

/// Agent marker variables, as exported by each agent to the commands it runs
/// (verified against the agents' source / binaries, and the cross-agent table
/// maintained by `is-ai-agent`). An agent's position in this table is its
/// precedence when markers from several agents are present (nested agents —
/// env can't say which is innermost, so the order is fixed).
const MARKERS: &[(&str, &str, Role)] = &[
    ("CLAUDE_PID", "claude", Role::Pid),
    ("CLAUDE_CODE_SESSION_ID", "claude", Role::Session),
    ("CLAUDECODE", "claude", Role::Flag),
    ("CODEX_THREAD_ID", "codex", Role::Session),
    ("CODEX_SANDBOX", "codex", Role::Flag),
    ("OPENCODE_SESSION_ID", "opencode", Role::Session),
    ("OPENCODE", "opencode", Role::Flag),
    ("CURSOR_SANDBOX", "cursor-agent", Role::Flag),
    ("CURSOR_AGENT", "cursor", Role::Flag),
    ("COPILOT_AGENT_SESSION_ID", "copilot", Role::Session),
    ("COPILOT_AGENT", "copilot", Role::Flag),
    ("COPILOT_CLI", "copilot", Role::Flag),
    ("GEMINI_CLI", "gemini", Role::Flag),
    ("AMP_CURRENT_THREAD_ID", "amp", Role::Session),
    ("GOOSE_TERMINAL", "goose", Role::Flag),
    ("CLINE_TASK_ID", "cline", Role::Session),
    ("CLINE_ACTIVE", "cline", Role::Flag),
    ("ROO_CODE_TASK_ID", "roo", Role::Session),
    ("AUGMENT_AGENT", "augment", Role::Flag),
    ("QWEN_CODE_SESSION_ID", "qwen", Role::Session),
    ("QWEN_CODE", "qwen", Role::Flag),
    ("PI_SESSION_ID", "pi", Role::Session),
    ("PI_CODING_AGENT", "pi", Role::Flag),
    ("CRUSH", "crush", Role::Flag),
    ("KIRO_AGENT_PATH", "kiro", Role::Flag),
];

impl AgentEnv {
    /// Pick the allowlisted markers out of a raw environ. Every other variable
    /// — including tokens agents also export — is skipped without being
    /// stored; of the markers, only pids and validated session ids are kept.
    pub fn from_environ(environ: &[OsString]) -> Option<AgentEnv> {
        // agent -> (pid, session), for every agent with at least one marker
        let mut hits: Vec<(&'static str, Option<u32>, Option<String>)> = Vec::new();
        let mut generic: Option<String> = None;
        for kv in environ {
            let Some(kv) = kv.to_str() else { continue };
            let Some((k, v)) = kv.split_once('=') else {
                continue;
            };
            if matches!(k, "AI_AGENT" | "AGENT") {
                // cross-agent conventions: `AI_AGENT=claude-code_2-1-294_agent`,
                // `AGENT=amp`; AI_AGENT wins over the bare `AGENT=1`.
                if let Some(name) = agent_from_marker(v) {
                    if k == "AI_AGENT" || generic.is_none() {
                        generic = Some(name);
                    }
                }
                continue;
            }
            let Some(&(_, agent, role)) = MARKERS.iter().find(|(key, _, _)| *key == k) else {
                continue;
            };
            if v.is_empty() {
                continue;
            }
            let i = match hits.iter().position(|h| h.0 == agent) {
                Some(i) => i,
                None => {
                    hits.push((agent, None, None));
                    hits.len() - 1
                }
            };
            match role {
                Role::Pid => hits[i].1 = v.parse().ok(),
                Role::Session if is_session_id(v) => hits[i].2 = Some(v.into()),
                _ => {}
            }
        }
        let rank = |a: &str| MARKERS.iter().position(|(_, m, _)| *m == a);
        if let Some((agent, pid, session)) = hits.into_iter().min_by_key(|h| rank(h.0)) {
            return Some(AgentEnv {
                agent: Some(agent.into()),
                pid,
                session,
            });
        }
        generic.map(|name| AgentEnv {
            agent: Some(name),
            pid: None,
            session: None,
        })
    }
}

/// Session/thread ids: `219af9f5-…` (Claude, Codex), `ses_3b0f…` (opencode),
/// `T-…` (Amp). Anything else is dropped rather than displayed.
fn is_session_id(v: &str) -> bool {
    !v.is_empty()
        && v.len() <= 80
        && v.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// A generic marker value -> agent name: `claude-code_2-1-294_agent` ->
/// `claude`, `github_copilot_vscode_agent` -> `copilot`, `opencode`, `amp`;
/// a bare `1`/`true` -> `agent`. Only short `[a-z0-9-]` names survive.
fn agent_from_marker(v: &str) -> Option<String> {
    let v = v.trim().to_lowercase();
    if v.is_empty() || v == "0" || v == "false" {
        return None;
    }
    if v == "1" || v == "true" {
        return Some("agent".into());
    }
    for (needle, name) in [
        ("claude", "claude"),
        ("github_copilot", "copilot"),
        ("copilot", "copilot"),
        ("codex", "codex"),
        ("opencode", "opencode"),
        ("cursor", "cursor"),
        ("gemini", "gemini"),
    ] {
        if v.starts_with(needle) {
            return Some(name.into());
        }
    }
    let head = v.split('_').next()?;
    (!head.is_empty()
        && head.len() <= 24
        && head
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'))
    .then(|| head.to_string())
}

/// Known launchers by process name (or a node/bun script's basename).
/// Matched case-insensitively as a prefix, because macOS truncates names and
/// editors run as `Code Helper (Plugin)` / `Cursor Helper (Plugin)`. Order
/// matters: `cursor-agent` must be tried before `cursor`.
const KNOWN: &[(&str, LauncherKind, &str)] = &[
    // agents
    ("claude", LauncherKind::Agent, "claude"),
    ("codex", LauncherKind::Agent, "codex"),
    ("opencode", LauncherKind::Agent, "opencode"),
    ("gemini", LauncherKind::Agent, "gemini"),
    ("cursor-agent", LauncherKind::Agent, "cursor-agent"),
    ("aider", LauncherKind::Agent, "aider"),
    ("goose", LauncherKind::Agent, "goose"),
    ("amp", LauncherKind::Agent, "amp"),
    ("crush", LauncherKind::Agent, "crush"),
    ("droid", LauncherKind::Agent, "droid"),
    ("copilot", LauncherKind::Agent, "copilot"),
    ("qwen", LauncherKind::Agent, "qwen"),
    ("auggie", LauncherKind::Agent, "augment"),
    ("kiro-cli", LauncherKind::Agent, "kiro"),
    ("cline", LauncherKind::Agent, "cline"),
    // editors / IDEs
    ("cursor", LauncherKind::Editor, "cursor"),
    ("code helper", LauncherKind::Editor, "vscode"),
    ("code", LauncherKind::Editor, "vscode"),
    ("windsurf", LauncherKind::Editor, "windsurf"),
    ("zed", LauncherKind::Editor, "zed"),
    ("idea", LauncherKind::Editor, "intellij"),
    ("webstorm", LauncherKind::Editor, "webstorm"),
    ("pycharm", LauncherKind::Editor, "pycharm"),
    ("goland", LauncherKind::Editor, "goland"),
    ("rustrover", LauncherKind::Editor, "rustrover"),
    ("rubymine", LauncherKind::Editor, "rubymine"),
    ("phpstorm", LauncherKind::Editor, "phpstorm"),
    ("clion", LauncherKind::Editor, "clion"),
    ("rider", LauncherKind::Editor, "rider"),
    ("xcode", LauncherKind::Editor, "xcode"),
    ("nvim", LauncherKind::Editor, "nvim"),
    ("vim", LauncherKind::Editor, "vim"),
    ("emacs", LauncherKind::Editor, "emacs"),
    ("helix", LauncherKind::Editor, "helix"),
    // terminals / multiplexers
    ("tmux", LauncherKind::Terminal, "tmux"),
    ("zellij", LauncherKind::Terminal, "zellij"),
    ("screen", LauncherKind::Terminal, "screen"),
    ("ghostty", LauncherKind::Terminal, "ghostty"),
    ("iterm", LauncherKind::Terminal, "iterm"),
    ("terminal", LauncherKind::Terminal, "terminal"),
    ("wezterm", LauncherKind::Terminal, "wezterm"),
    ("kitty", LauncherKind::Terminal, "kitty"),
    ("alacritty", LauncherKind::Terminal, "alacritty"),
    ("warp", LauncherKind::Terminal, "warp"),
    ("stable", LauncherKind::Terminal, "warp"), // Warp's process name
    ("hyper", LauncherKind::Terminal, "hyper"),
    ("gnome-terminal", LauncherKind::Terminal, "gnome-terminal"),
    ("konsole", LauncherKind::Terminal, "konsole"),
    ("foot", LauncherKind::Terminal, "foot"),
    ("rio", LauncherKind::Terminal, "rio"),
];

/// Names that must match exactly — as prefixes they'd swallow unrelated
/// programs (`code` vs `codesign`, `amp` vs `ampd`, `rio` vs `rioja`, …).
const EXACT: &[&str] = &[
    "code", "amp", "zed", "idea", "vim", "rio", "foot", "stable", "hyper", "screen", "goose",
    "crush", "droid", "terminal", "warp", "cline", "kitty", "rider", "clion",
];

/// Interpreters whose first argument is the real program (`node …/codex.js`).
const SCRIPT_HOSTS: &[&str] = &["node", "bun", "deno", "python", "python3"];

fn basename(s: &str) -> &str {
    s.rsplit('/').next().unwrap_or(s)
}

fn lookup(name: &str) -> Option<(LauncherKind, &'static str)> {
    let n = name.trim_start_matches('-').to_lowercase();
    let n = n.strip_suffix(".js").unwrap_or(&n);
    KNOWN.iter().find_map(|&(needle, kind, display)| {
        let hit = if EXACT.contains(&needle) {
            n == needle
        } else {
            n.starts_with(needle)
        };
        hit.then_some((kind, display))
    })
}

/// Classify one process as a known launcher, if it is one.
pub fn classify(p: &ProcInfo) -> Option<(LauncherKind, &'static str)> {
    if let Some(hit) = lookup(&p.name) {
        return Some(hit);
    }
    // node/bun-hosted CLIs: look at the script (`node /…/bin/codex`).
    let prog = p.argv.first().map(|a| basename(a)).unwrap_or("");
    if SCRIPT_HOSTS.contains(&prog) {
        if let Some(script) = p.argv.get(1).filter(|a| !a.starts_with('-')) {
            if let Some(hit @ (LauncherKind::Agent, _)) = lookup(basename(script)) {
                return Some(hit);
            }
        }
    }
    None
}

/// Attribute the target anchored at `anchor` to whoever launched it.
pub fn find(anchor: u32, procs: &HashMap<u32, ProcInfo>) -> Option<Launcher> {
    let mut nearest_other: Option<(LauncherKind, &'static str, &ProcInfo)> = None;
    let mut reached_init = false;
    // An unrecognized non-shell ancestor (a supervisor, some app) means the
    // server *is* owned — just by something we can't name. Don't call that
    // detached, and don't guess.
    let mut unknown_owner = false;
    let mut seen = std::collections::HashSet::new();
    let mut cur = procs.get(&anchor).and_then(|p| p.ppid);
    while let Some(pid) = cur {
        if pid <= 1 {
            reached_init = true;
            break;
        }
        if !seen.insert(pid) {
            break; // cycle guard
        }
        let Some(p) = procs.get(&pid) else { break };
        match classify(p) {
            Some((LauncherKind::Agent, name)) => {
                // The same agent's markers name its session (Claude, Codex,
                // opencode, …) — unless they carry a pid that isn't this one.
                let env = procs
                    .get(&anchor)
                    .and_then(|a| a.agent.as_ref())
                    .filter(|e| e.agent.as_deref() == Some(name))
                    .filter(|e| e.pid.is_none_or(|p| p == pid));
                return Some(Launcher {
                    kind: LauncherKind::Agent,
                    name: name.into(),
                    pid: Some(pid),
                    alive: true,
                    session: env.and_then(|e| e.session.clone()),
                    cwd: p.cwd.clone(),
                    start_time: p.start_time,
                });
            }
            Some((kind, name)) if nearest_other.is_none() => {
                nearest_other = Some((kind, name, p));
            }
            Some(_) => {}
            None if !crate::resolve::is_shell(&p.name) => unknown_owner = true,
            None => {}
        }
        cur = p.ppid;
    }
    let env = procs.get(&anchor).and_then(|a| a.agent.as_ref());
    if let Some((kind, name, p)) = nearest_other {
        // An editor's own agent (Cursor agent, Copilot agent mode) runs its
        // commands inside the editor — the markers say it was the agent, not
        // the human. Terminals don't get this upgrade: a tmux server started
        // from an agent would leak its markers onto every later pane.
        if let (LauncherKind::Editor, Some(env)) = (kind, env) {
            let mut l = from_env(env, procs);
            l.cwd = l.cwd.or_else(|| p.cwd.clone());
            return Some(l);
        }
        return Some(Launcher {
            kind,
            name: name.into(),
            pid: Some(p.pid),
            alive: true,
            session: None,
            cwd: p.cwd.clone(),
            start_time: p.start_time,
        });
    }
    // No live launcher in the chain. Env markers survive reparenting, so a
    // backgrounded agent server can still be traced — and its session checked.
    if let Some(env) = env {
        return Some(from_env(env, procs));
    }
    (reached_init && !unknown_owner).then(|| Launcher {
        kind: LauncherKind::Detached,
        name: "detached".into(),
        pid: None,
        alive: false,
        session: None,
        cwd: None,
        start_time: 0,
    })
}

/// A launcher from env markers alone (the launching process isn't in the
/// chain). Orphaned only on proof: the marker names a pid and that pid is no
/// longer an agent. Without a pid (most agents export only a session id),
/// liveness is unknown — never raise a false "session ended".
fn from_env(env: &AgentEnv, procs: &HashMap<u32, ProcInfo>) -> Launcher {
    let live = env
        .pid
        .and_then(|pid| procs.get(&pid))
        .filter(|p| matches!(classify(p), Some((LauncherKind::Agent, _))));
    Launcher {
        kind: LauncherKind::Agent,
        name: env.agent.clone().unwrap_or_else(|| "agent".into()),
        pid: env.pid,
        alive: env.pid.is_none() || live.is_some(),
        session: env.session.clone(),
        cwd: live.and_then(|p| p.cwd.clone()),
        start_time: live.map(|p| p.start_time).unwrap_or(0),
    }
}

/// The agent session this process runs inside, if any — what `--mine`
/// compares against. Prefers our own env markers, then the parent chain.
pub fn own_agent() -> Option<(Option<u32>, Option<String>)> {
    let env = std::env::vars_os().map(|(k, v)| {
        let mut kv = k;
        kv.push("=");
        kv.push(v);
        kv
    });
    if let Some(e) = AgentEnv::from_environ(&env.collect::<Vec<_>>()) {
        if e.pid.is_some() || e.session.is_some() {
            return Some((e.pid, e.session));
        }
    }
    use crate::sources::{ProcSource, SysinfoProcs};
    let src = SysinfoProcs::new();
    let procs = src.procs();
    let me = std::process::id();
    let mut cur = procs.get(&me).and_then(|p| p.ppid);
    let mut seen = std::collections::HashSet::new();
    while let Some(pid) = cur.filter(|&p| p > 1 && seen.insert(p)) {
        let p = procs.get(&pid)?;
        if matches!(classify(p), Some((LauncherKind::Agent, _))) {
            return Some((Some(pid), None));
        }
        cur = p.ppid;
    }
    None
}

/// Does `l` belong to the agent session identified by `own`?
pub fn same_session(l: &Launcher, own: &(Option<u32>, Option<String>)) -> bool {
    if l.kind != LauncherKind::Agent {
        return false;
    }
    match (&l.session, &own.1) {
        (Some(a), Some(b)) => a == b,
        _ => l.pid.is_some() && l.pid == own.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(pid: u32, ppid: u32, name: &str, argv: &[&str]) -> ProcInfo {
        ProcInfo {
            pid,
            ppid: Some(ppid),
            name: name.into(),
            argv: argv.iter().map(|s| s.to_string()).collect(),
            cwd: Some(PathBuf::from("/Users/dev/app")),
            cpu_pct: 0.0,
            mem_bytes: 0,
            start_time: 100,
            agent: None,
        }
    }
    fn map(ps: Vec<ProcInfo>) -> HashMap<u32, ProcInfo> {
        ps.into_iter().map(|p| (p.pid, p)).collect()
    }
    fn env(kvs: &[&str]) -> Vec<OsString> {
        kvs.iter().map(OsString::from).collect()
    }

    #[test]
    fn env_allowlist_reads_markers_and_nothing_else() {
        let e = AgentEnv::from_environ(&env(&[
            "AI_AGENT=claude-code_2-1-294_agent",
            "CLAUDE_PID=35115",
            "CLAUDE_CODE_SESSION_ID=219af9f5-0000-4000-8000-000000000000",
            "CLAUDE_CODE_MESSAGING_TOKEN=secret",
            "GITHUB_TOKEN=secret",
        ]))
        .unwrap();
        assert_eq!(e.agent.as_deref(), Some("claude"));
        assert_eq!(e.pid, Some(35115));
        assert_eq!(
            e.session.as_deref(),
            Some("219af9f5-0000-4000-8000-000000000000")
        );
        assert!(format!("{e:?}").find("secret").is_none());
        // no markers -> no agent
        assert!(AgentEnv::from_environ(&env(&["PATH=/bin", "HOME=/x"])).is_none());
        // a malformed session id is dropped rather than shown (the marker
        // still says which agent it was)
        let e = AgentEnv::from_environ(&env(&["CLAUDE_CODE_SESSION_ID=$(rm -rf)"])).unwrap();
        assert_eq!((e.agent.as_deref(), e.session), (Some("claude"), None));
    }

    fn agent_of(kvs: &[&str]) -> Option<(String, Option<u32>, Option<String>)> {
        AgentEnv::from_environ(&env(kvs)).map(|e| (e.agent.unwrap(), e.pid, e.session))
    }

    #[test]
    fn codex_opencode_and_copilot_markers_are_recognized() {
        // Codex: thread id is the resumable session
        assert_eq!(
            agent_of(&[
                "CODEX_SANDBOX=seatbelt",
                "CODEX_THREAD_ID=0199a213-81c0-7800-8aa1-bbab2a035a53",
            ]),
            Some((
                "codex".into(),
                None,
                Some("0199a213-81c0-7800-8aa1-bbab2a035a53".into())
            ))
        );
        // opencode's bash tool: AGENT=1, OPENCODE=1, AI_AGENT, OPENCODE_SESSION_ID
        assert_eq!(
            agent_of(&[
                "AGENT=1",
                "OPENCODE=1",
                "AI_AGENT=opencode",
                "OPENCODE_SESSION_ID=ses_3b0fcef09ffe1mWuat9ccxYaKv",
            ]),
            Some((
                "opencode".into(),
                None,
                Some("ses_3b0fcef09ffe1mWuat9ccxYaKv".into())
            ))
        );
        // Copilot agent mode in VS Code
        assert_eq!(
            agent_of(&["AI_AGENT=github_copilot_vscode_agent", "COPILOT_AGENT=1"]).map(|a| a.0),
            Some("copilot".into())
        );
        assert_eq!(
            agent_of(&["GEMINI_CLI=1"]).map(|a| a.0),
            Some("gemini".into())
        );
        assert_eq!(
            agent_of(&["AMP_CURRENT_THREAD_ID=T-5a7c"]).map(|a| (a.0, a.2)),
            Some(("amp".into(), Some("T-5a7c".into())))
        );
    }

    #[test]
    fn generic_markers_name_an_agent_only_when_nothing_specific_does() {
        assert_eq!(
            agent_of(&["AGENT=goose"]).map(|a| a.0),
            Some("goose".into())
        );
        assert_eq!(agent_of(&["AGENT=1"]).map(|a| a.0), Some("agent".into()));
        assert_eq!(agent_of(&["AGENT=0"]), None);
        // AI_AGENT beats a bare AGENT, specific markers beat both
        assert_eq!(
            agent_of(&["AGENT=1", "AI_AGENT=amp"]).map(|a| a.0),
            Some("amp".into())
        );
        assert_eq!(
            agent_of(&["AI_AGENT=opencode", "CODEX_THREAD_ID=abc"]).map(|a| a.0),
            Some("codex".into())
        );
        // junk values never become a displayed name
        assert_eq!(agent_of(&["AGENT=$(curl x)"]), None);
    }

    #[test]
    fn nested_agents_resolve_by_fixed_precedence() {
        // codex launched from a Claude Code shell: both sets of markers
        let a = agent_of(&[
            "CLAUDE_PID=20",
            "CLAUDE_CODE_SESSION_ID=aaaa",
            "CODEX_THREAD_ID=bbbb",
        ])
        .unwrap();
        assert_eq!(
            (a.0.as_str(), a.1, a.2.as_deref()),
            ("claude", Some(20), Some("aaaa"))
        );
    }

    #[test]
    fn flag_values_are_never_kept() {
        let e = AgentEnv::from_environ(&env(&["CURSOR_AGENT=hunter2", "KIRO_AGENT_PATH=/secret"]))
            .unwrap();
        assert_eq!(e.agent.as_deref(), Some("cursor"));
        let dbg = format!("{e:?}");
        assert!(!dbg.contains("hunter2") && !dbg.contains("secret"), "{dbg}");
    }

    #[test]
    fn live_codex_in_chain_gets_its_thread_and_resume_hint() {
        let mut server = p(30, 20, "node", &["node", "vite"]);
        server.agent = Some(AgentEnv {
            agent: Some("codex".into()),
            pid: None,
            session: Some("0199a213".into()),
        });
        let procs = map(vec![p(20, 1, "codex", &["codex"]), server]);
        let l = find(30, &procs).unwrap();
        assert_eq!((l.name.as_str(), l.pid), ("codex", Some(20)));
        assert_eq!(l.resume_hint().as_deref(), Some("codex resume 0199a213"));
        // opencode's hint
        let oc = Launcher {
            name: "opencode".into(),
            session: Some("ses_1".into()),
            ..l
        };
        assert_eq!(
            oc.resume_hint().as_deref(),
            Some("opencode --session ses_1")
        );
    }

    #[test]
    fn editor_agent_markers_upgrade_but_terminal_ones_do_not() {
        let tagged = |pid: u32, ppid: u32| {
            let mut s = p(pid, ppid, "node", &["node", "vite"]);
            s.agent = Some(AgentEnv {
                agent: Some("cursor".into()),
                pid: None,
                session: None,
            });
            s
        };
        // Cursor's agent ran it inside the editor -> the agent, not the editor
        let procs = map(vec![
            p(10, 1, "Cursor Helper (Plugin)", &["Cursor Helper"]),
            p(20, 10, "zsh", &["-zsh"]),
            tagged(30, 20),
        ]);
        let l = find(30, &procs).unwrap();
        assert_eq!((l.kind, l.name.as_str()), (LauncherKind::Agent, "cursor"));
        assert!(!l.is_orphaned());
        // same markers under tmux (maybe leaked from the session that started
        // the tmux server) -> stays the terminal
        let procs = map(vec![
            p(10, 1, "tmux: server", &["tmux"]),
            p(20, 10, "zsh", &["-zsh"]),
            tagged(30, 20),
        ]);
        assert_eq!(find(30, &procs).unwrap().kind, LauncherKind::Terminal);
    }

    #[test]
    fn reparented_opencode_server_is_attributed_but_never_called_ended() {
        let mut server = p(30, 1, "node", &["node", "vite"]);
        server.agent = Some(AgentEnv {
            agent: Some("opencode".into()),
            pid: None,
            session: Some("ses_abc".into()),
        });
        let l = find(30, &map(vec![server])).unwrap();
        assert_eq!((l.kind, l.name.as_str()), (LauncherKind::Agent, "opencode"));
        assert_eq!(l.session.as_deref(), Some("ses_abc"));
        assert!(!l.is_orphaned());
    }

    #[test]
    fn classify_names_without_overmatching() {
        let c = |name: &str| classify(&p(1, 0, name, &[name])).map(|(_, n)| n);
        assert_eq!(c("claude"), Some("claude"));
        assert_eq!(c("Cursor Helper (Plugin)"), Some("cursor"));
        assert_eq!(c("cursor-agent"), Some("cursor-agent"));
        assert_eq!(c("Code Helper (Plugin)"), Some("vscode"));
        assert_eq!(c("tmux: server"), Some("tmux"));
        assert_eq!(c("ghostty"), Some("ghostty"));
        assert_eq!(c("codesign"), None); // not `code`
        assert_eq!(c("ampd"), None); // not `amp`
        assert_eq!(c("zsh"), None);
        // node-hosted agent CLI
        let codex = p(1, 0, "node", &["node", "/usr/local/bin/codex", "--yolo"]);
        assert_eq!(classify(&codex).map(|(_, n)| n), Some("codex"));
    }

    #[test]
    fn nearest_agent_wins_over_terminal_and_editor() {
        // vite(40) <- zsh(30) <- claude(20) <- zsh(15) <- ghostty(10)
        let procs = map(vec![
            p(10, 1, "ghostty", &["ghostty"]),
            p(15, 10, "zsh", &["-zsh"]),
            p(20, 15, "claude", &["claude"]),
            p(30, 20, "zsh", &["/bin/zsh", "-c", "pnpm dev"]),
            p(40, 30, "node", &["node", "vite"]),
        ]);
        let l = find(40, &procs).unwrap();
        assert_eq!(
            (l.kind, l.name.as_str(), l.pid),
            (LauncherKind::Agent, "claude", Some(20))
        );
        assert!(l.alive && !l.is_orphaned());
    }

    #[test]
    fn falls_back_to_nearest_terminal() {
        let procs = map(vec![
            p(10, 1, "tmux: server", &["tmux"]),
            p(20, 10, "zsh", &["-zsh"]),
            p(30, 20, "node", &["node", "vite"]),
        ]);
        let l = find(30, &procs).unwrap();
        assert_eq!((l.kind, l.name.as_str()), (LauncherKind::Terminal, "tmux"));
    }

    #[test]
    fn reparented_server_is_traced_through_env_and_flagged_orphaned() {
        let mut server = p(30, 1, "node", &["node", "vite"]);
        server.agent = Some(AgentEnv {
            agent: Some("claude".into()),
            pid: Some(20),
            session: Some("abc-123".into()),
        });
        // session still running
        let live = map(vec![p(20, 1, "claude", &["claude"]), server.clone()]);
        let l = find(30, &live).unwrap();
        assert_eq!((l.kind, l.alive), (LauncherKind::Agent, true));
        assert_eq!(l.session.as_deref(), Some("abc-123"));
        // session ended (pid gone, or recycled by something that isn't an agent)
        let gone = map(vec![p(20, 1, "postgres", &["postgres"]), server]);
        let l = find(30, &gone).unwrap();
        assert!(l.is_orphaned());
    }

    #[test]
    fn markers_without_a_pid_are_never_called_orphaned() {
        let mut server = p(30, 1, "node", &["node", "vite"]);
        server.agent = Some(AgentEnv {
            agent: Some("claude".into()),
            pid: None,
            session: Some("abc-123".into()),
        });
        let l = find(30, &map(vec![server])).unwrap();
        assert_eq!(l.kind, LauncherKind::Agent);
        assert!(!l.is_orphaned(), "can't prove the session ended");
    }

    #[test]
    fn reparented_without_markers_is_detached() {
        let procs = map(vec![p(30, 1, "node", &["node", "vite"])]);
        assert_eq!(find(30, &procs).unwrap().kind, LauncherKind::Detached);
    }

    #[test]
    fn unknown_supervisor_is_not_guessed() {
        let procs = map(vec![
            p(10, 1, "herdr", &["herdr"]),
            p(20, 10, "node", &["node", "vite"]),
        ]);
        assert!(find(20, &procs).is_none());
    }

    #[test]
    fn same_session_prefers_session_id_then_pid() {
        let l = Launcher {
            kind: LauncherKind::Agent,
            name: "claude".into(),
            pid: Some(20),
            alive: true,
            session: Some("s1".into()),
            cwd: None,
            start_time: 0,
        };
        assert!(same_session(&l, &(Some(99), Some("s1".into()))));
        assert!(!same_session(&l, &(Some(20), Some("s2".into()))));
        assert!(same_session(&l, &(Some(20), None)));
        let term = Launcher {
            kind: LauncherKind::Terminal,
            ..l
        };
        assert!(!same_session(&term, &(Some(20), None)));
    }
}
