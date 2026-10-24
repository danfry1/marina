//! Agents reporting their own state. Without these, whether a session is
//! working, at an approval prompt or done is inferred from its transcript,
//! screen and shells (see `Agent::infer_waiting`); with them, the agent says
//! so as it happens.
//!
//! `marina hooks --install` registers `marina hook <agent>` with each agent it
//! finds: Claude Code's and Codex's hooks (the same JSON shape, the payload on
//! stdin) and an opencode plugin (the state and session as arguments). Each
//! call writes one small file per session under `state_dir()/reports`, which
//! the sampler matches to a live session by process ancestry or session id.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::{json, Value};

use crate::hosts::HostStatus;

/// A report this much older than the transcript's newest record has been
/// overtaken (a prompt typed, a turn interrupted): inference takes over.
pub const REPORT_SLACK: u64 = 5;
/// Report files older than this are deleted.
const REPORT_KEEP: u64 = 7 * 24 * 3600;
/// How many ancestors of the hook process are recorded: the agent is its
/// parent or, through a shell, its grandparent.
const CHAIN: usize = 6;

/// The latest thing one session said about itself.
#[derive(Clone, Debug, PartialEq)]
pub struct Report {
    pub agent: String,
    pub session: Option<String>,
    pub status: HostStatus,
    /// Unix seconds.
    pub at: u64,
    /// The hook process's ancestors, nearest first.
    pub chain: Vec<u32>,
}

fn reports_dir() -> Option<PathBuf> {
    Some(crate::logs::state_dir()?.join("reports"))
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// --- the hook ----------------------------------------------------------------

/// `marina hook <agent> [state session]`: record what the agent reported.
/// Always exits 0 and prints nothing — a hook's output is read by the agent
/// (Claude Code adds a prompt hook's stdout to the conversation), and a
/// failing hook must never get in its way.
pub fn run_hook(args: &[String]) -> i32 {
    let _ = record(args);
    0
}

fn record(args: &[String]) -> Option<()> {
    let agent = args.first()?.as_str();
    let (status, session) = match agent {
        "claude" | "codex" => {
            let mut input = String::new();
            std::io::stdin()
                .take(16 << 20)
                .read_to_string(&mut input)
                .ok()?;
            let v: Value = serde_json::from_str(&input).ok()?;
            let session = v["session_id"].as_str().map(String::from);
            (classify(&v)?, session)
        }
        "opencode" => (
            parse_status(args.get(1)?)?,
            args.get(2).filter(|s| !s.is_empty()).cloned(),
        ),
        _ => return None,
    };
    let mut chain = Vec::new();
    let mut pid = std::os::unix::process::parent_id();
    while chain.len() < CHAIN && pid > 1 {
        chain.push(pid);
        match crate::sources::parent_pid(pid) {
            Some(p) => pid = p,
            None => break,
        }
    }
    let report = Report {
        agent: agent.into(),
        session,
        status,
        at: now(),
        chain,
    };
    write(&reports_dir()?, &report)
}

/// The state a Claude Code / Codex hook event means, or `None` for events
/// that say nothing about it.
fn classify(v: &Value) -> Option<HostStatus> {
    let tool = v["tool_name"].as_str().unwrap_or("");
    Some(match v["hook_event_name"].as_str()? {
        "SessionStart" | "Stop" => HostStatus::Idle,
        "UserPromptSubmit" | "PostToolUse" => HostStatus::Working,
        // a question to the human is a tool call that waits on them
        "PreToolUse" if matches!(tool, "AskUserQuestion" | "request_user_input") => {
            HostStatus::Blocked
        }
        "PreToolUse" => HostStatus::Working,
        "PermissionRequest" => HostStatus::Blocked,
        // Claude Code's notices: only a permission or input dialog counts;
        // "waiting for your input" a minute after a turn is just idle
        "Notification" => match v["notification_type"].as_str() {
            Some("permission_prompt" | "elicitation_dialog") => HostStatus::Blocked,
            Some(_) => return None,
            None if v["message"]
                .as_str()
                .is_some_and(|m| m.contains("permission")) =>
            {
                HostStatus::Blocked
            }
            None => return None,
        },
        _ => return None,
    })
}

fn parse_status(s: &str) -> Option<HostStatus> {
    Some(match s {
        "working" => HostStatus::Working,
        "blocked" => HostStatus::Blocked,
        "idle" => HostStatus::Idle,
        _ => return None,
    })
}

fn status_name(s: HostStatus) -> &'static str {
    match s {
        HostStatus::Working => "working",
        HostStatus::Blocked => "blocked",
        HostStatus::Done | HostStatus::Idle => "idle",
    }
}

/// One file per session (or, without an id, per hook parent), replaced whole.
fn write(dir: &Path, r: &Report) -> Option<()> {
    std::fs::create_dir_all(dir).ok()?;
    let key = match &r.session {
        Some(s) => format!("{}-{s}", r.agent),
        None => format!("{}-pid{}", r.agent, r.chain.first()?),
    };
    let key: String = key
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let body = json!({
        "agent": r.agent,
        "session": r.session,
        "status": status_name(r.status),
        "at": r.at,
        "chain": r.chain,
    });
    let path = dir.join(format!("{key}.json"));
    let tmp = dir.join(format!(".{key}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, body.to_string()).ok()?;
    std::fs::rename(&tmp, path).ok()
}

fn parse(text: &str) -> Option<Report> {
    let v: Value = serde_json::from_str(text).ok()?;
    Some(Report {
        agent: v["agent"].as_str()?.into(),
        session: v["session"].as_str().map(String::from),
        status: parse_status(v["status"].as_str()?)?,
        at: v["at"].as_u64()?,
        chain: v["chain"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|p| p.as_u64())
                    .map(|p| p as u32)
                    .collect()
            })
            .unwrap_or_default(),
    })
}

// --- reading them ------------------------------------------------------------

/// The report files, re-read only when they change.
#[derive(Default)]
pub struct Reports {
    files: HashMap<PathBuf, (SystemTime, Report)>,
}

impl Reports {
    pub fn refresh(&mut self) {
        let Some(dir) = reports_dir() else { return };
        self.refresh_in(&dir, now());
    }

    fn refresh_in(&mut self, dir: &Path, now: u64) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            self.files.clear();
            return;
        };
        let mut seen = Vec::new();
        for e in entries.flatten() {
            let path = e.path();
            if path.extension().is_none_or(|x| x != "json") {
                continue;
            }
            let Ok(mtime) = e.metadata().and_then(|m| m.modified()) else {
                continue;
            };
            seen.push(path.clone());
            if self.files.get(&path).is_some_and(|(m, _)| *m == mtime) {
                continue;
            }
            match std::fs::read_to_string(&path)
                .ok()
                .as_deref()
                .and_then(parse)
            {
                Some(r) => {
                    self.files.insert(path, (mtime, r));
                }
                None => {
                    self.files.remove(&path);
                }
            }
        }
        self.files.retain(|p, _| seen.contains(p));
        self.files.retain(|p, (_, r)| {
            let old = now.saturating_sub(r.at) > REPORT_KEEP;
            if old {
                let _ = std::fs::remove_file(p);
            }
            !old
        });
    }

    /// The newest report from this session: made by its process (it's in
    /// the hook's ancestry) or naming its session id — and made since the
    /// process started, so a resumed session's old reports don't count.
    pub fn find(
        &self,
        agent: &str,
        pid: u32,
        started: u64,
        session: Option<&str>,
    ) -> Option<&Report> {
        self.files
            .values()
            .map(|(_, r)| r)
            .filter(|r| r.agent == agent && r.at + 1 >= started)
            .filter(|r| {
                r.chain.contains(&pid) || (session.is_some() && r.session.as_deref() == session)
            })
            .max_by_key(|r| (r.chain.contains(&pid), r.at))
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
}

// --- installing --------------------------------------------------------------

/// The events registered with Claude Code and Codex.
const CLAUDE_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PermissionRequest",
    "PostToolUse",
    "Notification",
    "Stop",
];
/// Codex has no `Notification`; its `PermissionRequest` covers approvals.
const CODEX_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PermissionRequest",
    "PostToolUse",
    "Stop",
];

/// The marina to run from a hook (a path that survives upgrades).
fn marina_path() -> PathBuf {
    crate::verbs::stable_exe().unwrap_or_else(|| PathBuf::from("marina"))
}

fn hook_command(agent: &str) -> String {
    format!(
        "{} hook {agent}",
        crate::launch::sh_quote_for(&marina_path().to_string_lossy(), false)
    )
}

/// One of ours: a hook running `marina hook <agent>`.
fn is_ours(hook: &Value, agent: &str) -> bool {
    hook["command"]
        .as_str()
        .is_some_and(|c| c.contains("marina") && c.ends_with(&format!(" hook {agent}")))
}

/// `settings["hooks"]` with ours removed (and any groups that empties).
fn strip(settings: &mut Value, agent: &str) -> bool {
    let Some(hooks) = settings.get_mut("hooks").and_then(Value::as_object_mut) else {
        return false;
    };
    let mut changed = false;
    for groups in hooks.values_mut() {
        let Some(groups) = groups.as_array_mut() else {
            continue;
        };
        for g in groups.iter_mut() {
            if let Some(list) = g.get_mut("hooks").and_then(Value::as_array_mut) {
                let before = list.len();
                list.retain(|h| !is_ours(h, agent));
                changed |= list.len() != before;
            }
        }
        groups.retain(|g| g["hooks"].as_array().is_none_or(|l| !l.is_empty()));
    }
    hooks.retain(|_, g| g.as_array().is_none_or(|g| !g.is_empty()));
    if hooks.is_empty() {
        settings.as_object_mut().map(|o| o.remove("hooks"));
    }
    changed
}

/// Add ours, replacing any earlier copy (its path may have moved).
fn add(settings: &mut Value, agent: &str, events: &[&str], command: &str) {
    strip(settings, agent);
    if !settings.is_object() {
        *settings = json!({});
    }
    let hooks = settings
        .as_object_mut()
        .expect("an object")
        .entry("hooks")
        .or_insert_with(|| json!({}));
    if !hooks.is_object() {
        *hooks = json!({});
    }
    for ev in events {
        let groups = hooks
            .as_object_mut()
            .expect("an object")
            .entry(ev.to_string())
            .or_insert_with(|| json!([]));
        if let Some(groups) = groups.as_array_mut() {
            groups.push(json!({
                "hooks": [{ "type": "command", "command": command, "timeout": 5 }]
            }));
        }
    }
}

#[cfg(test)]
fn has_ours(settings: &Value, agent: &str) -> bool {
    our_command(settings, agent).is_some()
}

fn our_command(settings: &Value, agent: &str) -> Option<String> {
    settings["hooks"].as_object().and_then(|h| {
        h.values()
            .filter_map(Value::as_array)
            .flatten()
            .filter_map(|g| g["hooks"].as_array())
            .flatten()
            .find(|h| is_ours(h, agent))
            .and_then(|h| h["command"].as_str().map(String::from))
    })
}

/// The program a hook command runs: its first word, single-quoted or not.
fn program(command: &str) -> PathBuf {
    let c = command.trim_start();
    let word = match c.strip_prefix('\'') {
        Some(rest) => rest.split('\'').next().unwrap_or(""),
        None => c.split(' ').next().unwrap_or(""),
    };
    PathBuf::from(word)
}

/// Hooked, unless the program it runs is missing.
fn hooked(program: PathBuf) -> Hooked {
    if program.is_absolute() && !program.is_file() {
        Hooked::Broken(program)
    } else {
        Hooked::Yes
    }
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn env_dir(var: &str) -> Option<PathBuf> {
    std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// Where each agent keeps its config — when it's installed (the directory
/// exists).
fn claude_settings() -> Option<PathBuf> {
    let dir = env_dir("CLAUDE_CONFIG_DIR").or_else(|| Some(home()?.join(".claude")))?;
    dir.is_dir().then(|| dir.join("settings.json"))
}

fn codex_hooks() -> Option<PathBuf> {
    let dir = env_dir("CODEX_HOME").or_else(|| Some(home()?.join(".codex")))?;
    dir.is_dir().then(|| dir.join("hooks.json"))
}

fn opencode_plugin() -> Option<PathBuf> {
    let dir = env_dir("XDG_CONFIG_HOME")
        .or_else(|| Some(home()?.join(".config")))?
        .join("opencode");
    if !dir.is_dir() {
        return None;
    }
    // opencode reads `plugins/` (and the older `plugin/`)
    let sub = if dir.join("plugin").is_dir() && !dir.join("plugins").is_dir() {
        "plugin"
    } else {
        "plugins"
    };
    Some(dir.join(sub).join("marina.js"))
}

/// opencode 2's TUI entrypoint, in a plugin directory beside the 1.x file.
fn opencode_tui_path(plugin: &Path) -> PathBuf {
    plugin.with_file_name("marina").join("tui.js")
}

fn read_json(path: &Path) -> Result<Value, String> {
    match std::fs::read_to_string(path) {
        Ok(t) if t.trim().is_empty() => Ok(json!({})),
        Ok(t) => serde_json::from_str(&t)
            .map_err(|e| format!("{} isn't valid JSON ({e}); left alone", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(json!({})),
        Err(e) => Err(format!("couldn't read {}: {e}", path.display())),
    }
}

/// Write `v` over `path`, keeping the previous file as `<name>.marina-backup`.
fn write_json(path: &Path, v: &Value) -> Result<(), String> {
    if path.exists() {
        let backup = path.with_extension("json.marina-backup");
        std::fs::copy(path, &backup)
            .map_err(|e| format!("couldn't back up {}: {e}", path.display()))?;
    }
    let text = serde_json::to_string_pretty(v).expect("a JSON value") + "\n";
    let tmp = path.with_extension("json.marina-tmp");
    std::fs::write(&tmp, text)
        .and_then(|()| std::fs::rename(&tmp, path))
        .map_err(|e| format!("couldn't write {}: {e}", path.display()))
}

/// The opencode 1.x plugin (`plugins/marina.js`, a server plugin): the state
/// and root session of each event, handed to `marina hook opencode`.
/// Sub-sessions (subagents) report as their root.
fn opencode_source(marina: &str) -> String {
    let marina = serde_json::to_string(marina).expect("a string");
    format!(
        r#"// installed by marina (`marina hooks --install`); `marina hooks --uninstall` removes it.
// Tells marina when a session is working, waiting on you, or idle (opencode 1.x;
// opencode 2 loads marina/tui.js beside this file instead).
const MARINA = {marina};
const parents = new Map();
const STATES = new Map([
  ["permission.asked", "blocked"],
  ["permission.updated", "blocked"],
  ["question.asked", "blocked"],
  ["permission.replied", "working"],
  ["question.replied", "working"],
  ["question.rejected", "working"],
  ["session.idle", "idle"],
]);
const BUSY = new Set(["busy", "retry", "pending", "running", "streaming", "working", "active"]);

function report(state, id) {{
  while (id && parents.has(id)) id = parents.get(id);
  try {{
    Bun.spawn([MARINA, "hook", "opencode", state, id ?? ""], {{
      stdin: "ignore", stdout: "ignore", stderr: "ignore",
    }}).unref();
  }} catch {{}}
}}

const server = async () => ({{
  event: async ({{ event }}) => {{
    const p = event?.properties ?? {{}};
    if (p.info?.id && p.info.parentID) parents.set(p.info.id, p.info.parentID);
    const id = typeof p.sessionID === "string" ? p.sessionID : undefined;
    let state = STATES.get(event?.type);
    if (event?.type === "session.status") {{
      const kind = typeof p.status === "string" ? p.status : p.status?.type;
      state = BUSY.has(String(kind).toLowerCase()) ? "working" : kind === "idle" ? "idle" : undefined;
    }}
    if (state) report(state, id);
  }},
}});

// opencode 2 checks for an id and setup(); its events reach marina/tui.js
export default {{ id: "marina", server, setup() {{}} }};
"#
    )
}

/// The opencode 2 plugin (`plugins/marina/tui.js`, found as a plugin
/// directory's TUI entrypoint): the same reports, from the TUI's event feed.
fn opencode_tui_source(marina: &str) -> String {
    let marina = serde_json::to_string(marina).expect("a string");
    format!(
        r#"// installed by marina (`marina hooks --install`); `marina hooks --uninstall` removes it.
// Tells marina when a session is working, waiting on you, or idle (opencode 2).
const MARINA = {marina};

function report(state, id) {{
  try {{
    Bun.spawn([MARINA, "hook", "opencode", state, id ?? ""], {{
      stdin: "ignore", stdout: "ignore", stderr: "ignore",
    }}).unref();
  }} catch {{}}
}}

function setup(api) {{
  const parents = new Map();
  // open permission prompts and forms, per root session
  const waiting = new Map();
  const root = (id) => {{
    const seen = new Set();
    while (typeof id === "string" && !seen.has(id)) {{
      seen.add(id);
      const parent = api.data?.session?.get?.(id)?.parentID ?? parents.get(id);
      if (!parent) return id;
      id = parent;
    }}
    return id;
  }};
  const ask = (id, key, open) => {{
    const set = waiting.get(id) ?? new Set();
    if (open) set.add(key); else set.delete(key);
    waiting.set(id, set);
    report(set.size ? "blocked" : "working", id);
  }};
  const unsubscribe = api.data?.listen?.(({{ details: event }}) => {{
    const data = event?.data;
    if (!data) return;
    if (event.type === "session.created" && data.parentID) parents.set(data.sessionID, data.parentID);
    const id = root(event.type === "form.created" ? data.form?.sessionID : data.sessionID);
    if (!id) return;
    switch (event.type) {{
      case "permission.asked": return ask(id, `p:${{data.id}}`, true);
      case "permission.replied": return ask(id, `p:${{data.requestID}}`, false);
      case "form.created": return ask(id, `f:${{data.form?.id}}`, true);
      case "form.replied":
      case "form.cancelled": return ask(id, `f:${{data.id}}`, false);
      case "session.execution.started": return report("working", id);
      case "session.execution.succeeded":
      case "session.execution.interrupted":
      case "session.execution.failed":
        waiting.delete(id);
        return report("idle", id);
    }}
  }});
  return () => unsubscribe?.();
}}

export default {{ id: "marina", setup }};
"#
    )
}

#[derive(Debug, PartialEq)]
pub enum Hooked {
    /// Ours are registered.
    Yes,
    /// Registered, but the marina they run is gone (moved, uninstalled).
    Broken(PathBuf),
    No,
    /// The agent isn't installed.
    Absent,
}

/// Whether each agent has marina's hooks: `(agent, state, where)`.
pub fn installed() -> Vec<(&'static str, Hooked, Option<PathBuf>)> {
    let json_state = |p: Option<PathBuf>, agent: &str| match p {
        None => (Hooked::Absent, None),
        Some(p) => {
            let cmd = read_json(&p).ok().and_then(|v| our_command(&v, agent));
            let state = match cmd {
                Some(c) => hooked(program(&c)),
                None => Hooked::No,
            };
            (state, Some(p))
        }
    };
    let (c, cp) = json_state(claude_settings(), "claude");
    let (x, xp) = json_state(codex_hooks(), "codex");
    let (o, op) = match opencode_plugin() {
        None => (Hooked::Absent, None),
        Some(p) if p.is_file() => {
            // `const MARINA = "<path>";`
            let path = std::fs::read_to_string(&p).ok().and_then(|src| {
                let line = src.lines().find(|l| l.starts_with("const MARINA = "))?;
                serde_json::from_str::<String>(
                    line["const MARINA = ".len()..].trim_end_matches(';'),
                )
                .ok()
            });
            (
                path.map_or(Hooked::Yes, |m| hooked(PathBuf::from(m))),
                Some(p),
            )
        }
        Some(p) => (Hooked::No, Some(p)),
    };
    vec![
        ("Claude Code", c, cp),
        ("Codex", x, xp),
        ("opencode", o, op),
    ]
}

/// `marina hooks --install`.
pub fn install() -> i32 {
    let marina = marina_path().to_string_lossy().into_owned();
    let mut failed = false;
    let mut any = false;
    let mut json_agent =
        |name: &str, path: Option<PathBuf>, agent: &str, events: &[&str], note: &str| {
            let Some(path) = path else { return };
            any = true;
            match read_json(&path).and_then(|mut v| {
                add(&mut v, agent, events, &hook_command(agent));
                write_json(&path, &v)
            }) {
                Ok(()) => println!("{name}: hooks added to {}{note}", path.display()),
                Err(e) => {
                    eprintln!("{name}: {e}");
                    failed = true;
                }
            }
        };
    json_agent(
        "Claude Code",
        claude_settings(),
        "claude",
        CLAUDE_EVENTS,
        " (new sessions pick them up)",
    );
    json_agent(
        "Codex",
        codex_hooks(),
        "codex",
        CODEX_EVENTS,
        " (Codex asks you to review them once, on its next start)",
    );
    if let Some(path) = opencode_plugin() {
        any = true;
        let tui = opencode_tui_path(&path);
        let res = tui
            .parent()
            .map(std::fs::create_dir_all)
            .unwrap_or(Ok(()))
            .and_then(|()| std::fs::write(&path, opencode_source(&marina)))
            .and_then(|()| std::fs::write(&tui, opencode_tui_source(&marina)));
        match res {
            Ok(()) => println!(
                "opencode: plugin written to {} (restart opencode to load it)",
                path.display()
            ),
            Err(e) => {
                eprintln!("opencode: couldn't write {}: {e}", path.display());
                failed = true;
            }
        }
    }
    if !any {
        eprintln!("marina: no Claude Code, Codex or opencode config found to add hooks to");
        return 1;
    }
    println!("they run {marina}; `marina hooks --uninstall` removes them");
    i32::from(failed)
}

/// `marina hooks --uninstall`.
pub fn uninstall() -> i32 {
    let mut failed = false;
    for (name, path, agent) in [
        ("Claude Code", claude_settings(), "claude"),
        ("Codex", codex_hooks(), "codex"),
    ] {
        let Some(path) = path.filter(|p| p.is_file()) else {
            continue;
        };
        let res = read_json(&path).and_then(|mut v| match strip(&mut v, agent) {
            true => write_json(&path, &v).map(|()| true),
            false => Ok(false),
        });
        match res {
            Ok(true) => println!("{name}: hooks removed from {}", path.display()),
            Ok(false) => {}
            Err(e) => {
                eprintln!("{name}: {e}");
                failed = true;
            }
        }
    }
    if let Some(path) = opencode_plugin().filter(|p| p.is_file()) {
        let tui = opencode_tui_path(&path);
        let _ = std::fs::remove_file(&tui);
        if let Some(dir) = tui.parent() {
            let _ = std::fs::remove_dir(dir); // only when empty
        }
        match std::fs::remove_file(&path) {
            Ok(()) => println!("opencode: removed {}", path.display()),
            Err(e) => {
                eprintln!("opencode: couldn't remove {}: {e}", path.display());
                failed = true;
            }
        }
    }
    i32::from(failed)
}

/// `marina hooks` / `--status`.
pub fn status() -> i32 {
    for (name, state, path) in installed() {
        let path = path.map(|p| p.display().to_string()).unwrap_or_default();
        match state {
            Hooked::Yes => println!("{name:<12} hooked      {path}"),
            Hooked::Broken(m) => println!(
                "{name:<12} broken      {path} runs {}, which is gone (marina hooks --install)",
                m.display()
            ),
            Hooked::No => println!("{name:<12} not hooked  (marina hooks --install)"),
            Hooked::Absent => println!("{name:<12} not installed"),
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_map_to_states() {
        let ev = |j: Value| classify(&j);
        assert_eq!(
            ev(json!({"hook_event_name": "UserPromptSubmit"})),
            Some(HostStatus::Working)
        );
        assert_eq!(
            ev(json!({"hook_event_name": "Stop"})),
            Some(HostStatus::Idle)
        );
        assert_eq!(
            ev(json!({"hook_event_name": "PermissionRequest"})),
            Some(HostStatus::Blocked)
        );
        assert_eq!(
            ev(json!({"hook_event_name": "PreToolUse", "tool_name": "AskUserQuestion"})),
            Some(HostStatus::Blocked),
            "a question to the human waits on them"
        );
        assert_eq!(
            ev(json!({"hook_event_name": "PreToolUse", "tool_name": "Bash"})),
            Some(HostStatus::Working)
        );
        assert_eq!(
            ev(
                json!({"hook_event_name": "Notification", "notification_type": "permission_prompt"})
            ),
            Some(HostStatus::Blocked)
        );
        assert_eq!(
            ev(json!({"hook_event_name": "Notification", "notification_type": "idle_prompt"})),
            None,
            "idle a minute after a turn is not a question"
        );
        assert_eq!(
            ev(
                json!({"hook_event_name": "Notification", "message": "Claude needs your permission to use Bash"})
            ),
            Some(HostStatus::Blocked)
        );
        assert_eq!(ev(json!({"hook_event_name": "SubagentStop"})), None);
    }

    #[test]
    fn reports_round_trip_and_match_by_ancestry_or_session() {
        let dir = std::env::temp_dir().join(format!("marina-reports-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let r = Report {
            agent: "claude".into(),
            session: Some("abc/../x".into()),
            status: HostStatus::Blocked,
            at: 1000,
            chain: vec![50, 40, 1],
        };
        write(&dir, &r).unwrap();
        let files: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().collect();
        assert_eq!(files.len(), 1, "no temp file left behind");
        assert!(!files[0].file_name().to_string_lossy().contains('/'));

        let mut reps = Reports::default();
        reps.refresh_in(&dir, 1010);
        assert_eq!(
            reps.find("claude", 40, 900, None),
            Some(&r),
            "its process ran the hook"
        );
        assert_eq!(
            reps.find("claude", 77, 900, Some("abc/../x")),
            Some(&r),
            "its session id"
        );
        assert_eq!(reps.find("claude", 77, 900, None), None);
        assert_eq!(reps.find("codex", 40, 900, None), None, "another agent's");
        assert_eq!(
            reps.find("claude", 40, 2000, None),
            None,
            "made before it started"
        );

        // a week on, it's gone — from disk too
        reps.refresh_in(&dir, 1000 + REPORT_KEEP + 1);
        assert!(reps.is_empty());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn install_keeps_others_hooks_and_order_and_replaces_its_own() {
        let mut v: Value = serde_json::from_str(
            r#"{"theme":"dark","hooks":{"SessionStart":[{"matcher":"startup","hooks":[{"type":"command","command":"bash herdr.sh session"}]}]},"model":"opus"}"#,
        )
        .unwrap();
        add(&mut v, "claude", CLAUDE_EVENTS, "/old/marina hook claude");
        add(
            &mut v,
            "claude",
            CLAUDE_EVENTS,
            "'/new path/marina' hook claude",
        );
        assert!(has_ours(&v, "claude"));
        let keys: Vec<&String> = v.as_object().unwrap().keys().collect();
        assert_eq!(
            keys,
            ["theme", "hooks", "model"],
            "the file's order is kept"
        );
        let start = v["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(start.len(), 2, "herdr's hook and one of ours");
        assert_eq!(start[0]["hooks"][0]["command"], "bash herdr.sh session");
        let all = v.to_string();
        assert!(!all.contains("/old/marina"), "the earlier copy is replaced");
        assert_eq!(v["hooks"]["Stop"].as_array().unwrap().len(), 1);

        assert!(strip(&mut v, "claude"));
        assert!(!has_ours(&v, "claude"));
        assert_eq!(
            v["hooks"].as_object().unwrap().len(),
            1,
            "only herdr's left"
        );
        let mut empty = json!({"hooks": {}});
        add(&mut empty, "codex", CODEX_EVENTS, "marina hook codex");
        strip(&mut empty, "codex");
        assert_eq!(empty, json!({}), "nothing of ours left behind");
    }

    #[test]
    fn finds_the_program_a_hook_runs() {
        assert_eq!(
            program("'/a b/marina' hook claude"),
            PathBuf::from("/a b/marina")
        );
        assert_eq!(
            program("/opt/homebrew/bin/marina hook codex"),
            PathBuf::from("/opt/homebrew/bin/marina")
        );
        assert_eq!(
            hooked(PathBuf::from("/nowhere/marina")),
            Hooked::Broken("/nowhere/marina".into())
        );
        assert_eq!(
            hooked(PathBuf::from("marina")),
            Hooked::Yes,
            "found on PATH at run time"
        );
    }

    #[test]
    fn the_opencode_2_plugin_listens_to_the_tui() {
        let src = opencode_tui_source("/x/marina");
        assert!(src.contains(r#"const MARINA = "/x/marina";"#));
        assert!(src.contains("api.data?.listen?.("));
        assert!(src.contains("case \"permission.asked\": return ask(id, `p:${data.id}`, true);"));
        assert!(src.contains("export default { id: \"marina\", setup };"));
    }

    #[test]
    fn the_opencode_plugin_quotes_its_path() {
        let src = opencode_source("/a \"b\"/marina");
        assert!(src.contains(r#"const MARINA = "/a \"b\"/marina";"#));
        assert!(src.contains("export default { id: \"marina\", server, setup() {} };"));
    }
}
