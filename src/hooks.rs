//! Agent hooks: marina acting at the moments that matter, without anyone
//! having to remember to ask it.
//!
//! `marina hooks install` registers `marina hook <event>` with Claude Code:
//!
//! - **session-start** — tell the agent what's already running (so it reuses
//!   a server instead of starting a duplicate) and this worktree's port.
//! - **pre-bash** — when a Bash command looks like it starts a dev server:
//!   note if this project's server is already up, or if the command
//!   hard-codes a port (suggest `$(marina port)`). Never blocks; adds context
//!   only. Runs on *every* Bash call, so the snapshot is built only for
//!   commands that match.
//! - **session-end** — stop the servers this session started (its
//!   `CLAUDE_CODE_SESSION_ID`), so agents clean up after themselves. Skipped
//!   on `/clear`, which ends the session id but not the work.
//!
//! The text builders are pure; I/O lives in [`run`].

use std::path::Path;

use crate::model::{Snapshot, Target};

/// Dev-server start commands, matched on whitespace tokens (not substrings):
/// `pnpm dev`, `npm run dev`, `yarn start`, `vite`, `next dev`, `cargo run`,
/// `uvicorn`, `rails s`, … — the moments a duplicate server is born.
pub fn starts_dev_server(cmd: &str) -> bool {
    let toks: Vec<&str> = cmd
        .split(|c: char| c.is_whitespace() || matches!(c, ';' | '&' | '|' | '(' | ')'))
        .filter(|t| !t.is_empty())
        .collect();
    let base = |t: &str| t.rsplit('/').next().unwrap_or(t).to_string();
    let scripts = ["dev", "start", "serve", "preview", "develop"];
    toks.iter().enumerate().any(|(i, t)| {
        let b = base(t);
        let next = toks.get(i + 1).copied().unwrap_or("");
        let after = toks.get(i + 2).copied().unwrap_or("");
        match b.as_str() {
            // package managers: `pnpm dev`, `npm run dev`, `bun run start`
            "npm" | "pnpm" | "yarn" | "bun" => {
                scripts.contains(&next) || (next == "run" && scripts.contains(&after))
            }
            "next" | "nuxt" | "astro" | "remix" => scripts.contains(&next),
            "vite" | "uvicorn" | "gunicorn" | "hypercorn" | "nodemon" | "air" => true,
            "cargo" => next == "run" || (next == "watch"),
            "rails" => next == "s" || next == "server",
            "flask" => next == "run",
            "manage.py" => next == "runserver",
            "mix" => next == "phx.server",
            "go" => next == "run",
            "http.server" => true,
            _ => false,
        }
    })
}

/// A port written into the command: `--port 3000`, `--port=3000`, `-p 3000`,
/// `PORT=3000`. `$(marina port)` and other substitutions don't count.
pub fn hardcoded_port(cmd: &str) -> Option<u16> {
    let toks: Vec<&str> = cmd.split_whitespace().collect();
    for (i, t) in toks.iter().enumerate() {
        let val = if let Some(v) = t.strip_prefix("--port=") {
            Some(v)
        } else if let Some(v) = t.strip_prefix("PORT=") {
            Some(v)
        } else if matches!(*t, "--port" | "-p") {
            toks.get(i + 1).copied()
        } else {
            None
        };
        if let Some(p) = val.and_then(|v| v.trim_matches(['"', '\'']).parse::<u16>().ok()) {
            if p != 0 {
                return Some(p);
            }
        }
    }
    None
}

fn port_list(t: &Target) -> String {
    t.ports
        .iter()
        .map(|p| format!(":{p}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn who(t: &Target) -> String {
    match &t.launcher {
        Some(l) => format!(" (via {})", l.describe()),
        None => String::new(),
    }
}

/// Session-start context: what's running for *this* project (reuse it), how
/// many other dev servers exist, and this worktree's port. `None` when there
/// is nothing worth saying.
pub fn session_context(snap: &Snapshot, root: Option<&Path>, port: Option<u16>) -> Option<String> {
    let mine: Vec<&Target> = snap
        .targets
        .iter()
        .filter(|t| root.is_some_and(|r| t.cwd.starts_with(r)) && !t.ports.is_empty())
        .collect();
    let others = snap.targets.len() - mine.len();
    let orphans = snap
        .targets
        .iter()
        .filter(|t| t.launcher.as_ref().is_some_and(|l| l.is_orphaned()))
        .count();
    let mut lines = Vec::new();
    if !mine.is_empty() {
        lines.push(
            "Dev servers already running for this project — reuse them rather than starting duplicates:"
                .to_string(),
        );
        for t in &mine {
            lines.push(format!(
                "- {} · {} · {}{}",
                t.project,
                t.command_label,
                port_list(t),
                who(t)
            ));
        }
    }
    if let Some(p) = port {
        lines.push(format!(
            "This worktree's dev port is {p} (from `marina port`); start servers with `--port $(marina port)` rather than a hard-coded port."
        ));
    }
    if others > 0 && !lines.is_empty() {
        lines.push(format!(
            "{others} other dev server{} running on this machine (`marina ls`).",
            if others == 1 { " is" } else { "s are" }
        ));
    }
    if orphans > 0 {
        lines.push(format!(
            "{orphans} server{} left behind by ended agent sessions (`marina ls --orphaned`).",
            if orphans == 1 { " was" } else { "s were" }
        ));
    }
    if lines.is_empty() {
        return None;
    }
    lines.push(
        "marina: `marina who <port>` explains a port; `marina kill --mine` stops servers you started."
            .into(),
    );
    Some(lines.join("\n"))
}

/// Pre-Bash context for a dev-server start: this project's server is already
/// up, and/or the command hard-codes a port. `None` = say nothing.
pub fn pre_bash_context(
    cmd: &str,
    snap: &Snapshot,
    root: Option<&Path>,
    port: Option<u16>,
) -> Option<String> {
    let mut notes = Vec::new();
    let running: Vec<&Target> = snap
        .targets
        .iter()
        .filter(|t| root.is_some_and(|r| t.cwd.starts_with(r)) && !t.ports.is_empty())
        .collect();
    for t in &running {
        notes.push(format!(
            "marina: {} · {} is already running on {}{} — reuse it, or stop it first with `marina free {}`.",
            t.project,
            t.command_label,
            port_list(t),
            who(t),
            t.ports[0]
        ));
    }
    if let Some(p) = hardcoded_port(cmd) {
        match port {
            Some(mine) if mine != p => notes.push(format!(
                "marina: this command hard-codes port {p}; this worktree's port is {mine} — prefer `--port $(marina port)` so parallel worktrees don't collide."
            )),
            _ => {}
        }
        if let Some(t) = snap.targets.iter().find(|t| t.ports.contains(&p)) {
            if !running.iter().any(|r| r.key == t.key) {
                notes.push(format!(
                    "marina: port {p} is already taken by {} · {}{}.",
                    t.project,
                    t.command_label,
                    who(t)
                ));
            }
        }
    }
    (!notes.is_empty()).then(|| notes.join("\n"))
}

/// Targets a Claude Code session started, by its session id.
pub fn session_targets<'a>(snap: &'a Snapshot, session_id: &str) -> Vec<&'a Target> {
    snap.targets
        .iter()
        .filter(|t| {
            t.launcher
                .as_ref()
                .and_then(|l| l.session.as_deref())
                .is_some_and(|s| s == session_id)
        })
        .collect()
}

// --- runtime: `marina hook <event>` ---------------------------------------

/// Hook stdin is small JSON from Claude Code; cap what we read regardless.
const MAX_INPUT: u64 = 1 << 20;

/// Entry point for `marina hook <event>`, called by Claude Code with the
/// hook's JSON on stdin. Always exits 0 — a broken hook must never get in the
/// way of the agent's actual work; problems go to stderr (Claude's debug log).
pub fn run(event: &str) -> i32 {
    use std::io::Read;
    let mut raw = String::new();
    let _ = std::io::stdin().take(MAX_INPUT).read_to_string(&mut raw);
    let input: serde_json::Value = serde_json::from_str(&raw).unwrap_or_default();
    match event {
        "session-start" => on_session_start(&input),
        "pre-bash" => on_pre_bash(&input),
        "session-end" => on_session_end(&input),
        other => eprintln!("marina hook: unknown event {other:?}"),
    }
    0
}

/// The project root of the session's cwd (from the hook input, else
/// `$CLAUDE_PROJECT_DIR`), canonicalized like `marina port` does.
fn root_of(input: &serde_json::Value) -> Option<std::path::PathBuf> {
    let cwd = input
        .get("cwd")
        .and_then(|v| v.as_str())
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("CLAUDE_PROJECT_DIR").map(std::path::PathBuf::from))?;
    let cwd = cwd.canonicalize().ok()?;
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    crate::resolve::project_root(&cwd, home.as_deref())
}

/// This root's existing `marina port` lease — read-only: a hook never creates
/// assignments as a side effect of a session starting.
fn leased_port(root: Option<&Path>) -> Option<u16> {
    let root = root?;
    let store = crate::ports::Store::open().ok()?;
    crate::ports::lookup(&store.load(), root, None)
}

fn snapshot() -> Snapshot {
    crate::sampler::Sampler::new().build()
}

fn emit(event: &str, context: String) {
    let out = serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": event,
            "additionalContext": context,
        }
    });
    println!("{out}");
}

fn on_session_start(input: &serde_json::Value) {
    let root = root_of(input);
    let snap = snapshot();
    if let Some(ctx) = session_context(&snap, root.as_deref(), leased_port(root.as_deref())) {
        emit("SessionStart", ctx);
    }
}

/// Runs before *every* Bash call: bail out in microseconds unless the command
/// looks like a dev-server start, and never return a permission decision —
/// context only (`allow` would silently auto-approve the command).
fn on_pre_bash(input: &serde_json::Value) {
    if input.get("tool_name").and_then(|v| v.as_str()) != Some("Bash") {
        return;
    }
    let Some(cmd) = input
        .pointer("/tool_input/command")
        .and_then(|v| v.as_str())
    else {
        return;
    };
    if !starts_dev_server(cmd) {
        return;
    }
    let root = root_of(input);
    let snap = snapshot();
    if let Some(ctx) = pre_bash_context(cmd, &snap, root.as_deref(), leased_port(root.as_deref())) {
        emit("PreToolUse", ctx);
    }
}

/// Stop what this session started. Skipped for `/clear` and `/resume`: the
/// conversation id changes but the person is still working, often on the
/// very servers it started.
fn on_session_end(input: &serde_json::Value) {
    let reason = input.get("reason").and_then(|v| v.as_str()).unwrap_or("");
    if matches!(reason, "clear" | "resume") {
        return;
    }
    let Some(session) = input
        .get("session_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    else {
        return;
    };
    let snap = snapshot();
    let targets = session_targets(&snap, session);
    if targets.is_empty() {
        return;
    }
    let mut pid_starts = Vec::new();
    for t in &targets {
        match &t.container {
            Some(c) => {
                let _ = std::process::Command::new("docker")
                    .args(["stop", c])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status();
            }
            None => pid_starts.extend(&t.pid_starts),
        }
    }
    // Inside the hook's time budget: SIGTERM, up to 2s, then SIGKILL.
    crate::verbs::kill_blocking(&pid_starts, std::time::Duration::from_secs(2));
    eprintln!(
        "marina: session ended — stopped {} server{} it started",
        targets.len(),
        if targets.len() == 1 { "" } else { "s" }
    );
}

// --- install: `marina hooks install | uninstall | status` -------------------

/// (Claude Code event, our hook name, matcher, timeout seconds). SessionEnd's
/// default budget is 1.5s — too little to stop a server — so every hook sets
/// its own timeout.
const EVENTS: &[(&str, &str, Option<&str>, u64)] = &[
    ("SessionStart", "session-start", None, 10),
    ("PreToolUse", "pre-bash", Some("Bash"), 5),
    ("SessionEnd", "session-end", None, 10),
];

/// Is this settings hook handler one of ours?
fn is_marina_handler(h: &serde_json::Value) -> bool {
    h.get("command")
        .and_then(|c| c.as_str())
        .is_some_and(|c| c.contains("marina") && c.contains(" hook "))
}

/// Remove every marina handler (and any group left empty) from `settings`.
pub fn strip_settings(settings: &mut serde_json::Value) {
    strip(settings)
}

fn strip(settings: &mut serde_json::Value) {
    let Some(hooks) = settings.get_mut("hooks").and_then(|h| h.as_object_mut()) else {
        return;
    };
    for groups in hooks.values_mut() {
        if let Some(groups) = groups.as_array_mut() {
            for g in groups.iter_mut() {
                if let Some(hs) = g.get_mut("hooks").and_then(|h| h.as_array_mut()) {
                    hs.retain(|h| !is_marina_handler(h));
                }
            }
            groups.retain(|g| {
                g.get("hooks")
                    .and_then(|h| h.as_array())
                    .is_none_or(|h| !h.is_empty())
            });
        }
    }
    hooks.retain(|_, v| v.as_array().is_none_or(|a| !a.is_empty()));
    if hooks.is_empty() {
        if let Some(obj) = settings.as_object_mut() {
            obj.remove("hooks");
        }
    }
}

/// Add marina's hooks to `settings` (replacing any previous marina entries;
/// everything else is left exactly as it was). `exe` is how to invoke marina.
pub fn install_into(
    settings: &mut serde_json::Value,
    exe: &str,
    cleanup: bool,
) -> Result<(), String> {
    if !settings.is_object() {
        return Err("settings file is not a JSON object".into());
    }
    strip(settings);
    let obj = settings.as_object_mut().expect("checked above");
    let hooks = obj.entry("hooks").or_insert_with(|| serde_json::json!({}));
    let hooks = hooks
        .as_object_mut()
        .ok_or("`hooks` in settings is not an object")?;
    for &(event, name, matcher, timeout) in EVENTS {
        if name == "session-end" && !cleanup {
            continue;
        }
        let mut group = serde_json::json!({
            "hooks": [{
                "type": "command",
                "command": format!("{exe} hook {name}"),
                "timeout": timeout,
            }]
        });
        if let Some(m) = matcher {
            group["matcher"] = serde_json::json!(m);
        }
        hooks
            .entry(event)
            .or_insert_with(|| serde_json::json!([]))
            .as_array_mut()
            .ok_or_else(|| format!("`hooks.{event}` in settings is not an array"))?
            .push(group);
    }
    Ok(())
}

/// marina's installed hooks in `settings`: (event, hook name).
pub fn installed(settings: &serde_json::Value) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let Some(hooks) = settings.get("hooks").and_then(|h| h.as_object()) else {
        return out;
    };
    for (event, groups) in hooks {
        for g in groups.as_array().into_iter().flatten() {
            for h in g
                .get("hooks")
                .and_then(|h| h.as_array())
                .into_iter()
                .flatten()
            {
                if is_marina_handler(h) {
                    let cmd = h.get("command").and_then(|c| c.as_str()).unwrap_or("");
                    let name = cmd.rsplit(' ').next().unwrap_or("").to_string();
                    out.push((event.clone(), name));
                }
            }
        }
    }
    out
}

/// User settings: `$CLAUDE_CONFIG_DIR/settings.json`, else
/// `~/.claude/settings.json`. Project: `<root>/.claude/settings.json`.
pub fn settings_path(project: bool) -> Option<std::path::PathBuf> {
    if project {
        let cwd = std::env::current_dir().ok()?.canonicalize().ok()?;
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
        let root = crate::resolve::project_root(&cwd, home.as_deref())?;
        return Some(root.join(".claude").join("settings.json"));
    }
    let dir = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(std::path::PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| {
            std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".claude"))
        })?;
    Some(dir.join("settings.json"))
}

/// How hooks should invoke marina: plain `marina` when that resolves to this
/// very binary on PATH (survives `brew upgrade`), else the absolute path.
pub fn invocation() -> String {
    let me = std::env::current_exe()
        .ok()
        .and_then(|p| p.canonicalize().ok());
    let on_path = std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|d| d.join("marina"))
            .find(|c| c.is_file())
            .and_then(|c| c.canonicalize().ok())
    });
    match (&me, &on_path) {
        (Some(a), Some(b)) if a == b => "marina".into(),
        (Some(a), _) => a.display().to_string(),
        _ => "marina".into(),
    }
}

/// Read settings (absent → `{}`); a file that doesn't parse is an error and
/// is never overwritten.
pub fn read_settings(path: &Path) -> Result<serde_json::Value, String> {
    match std::fs::read_to_string(path) {
        Ok(text) if text.trim().is_empty() => Ok(serde_json::json!({})),
        Ok(text) => serde_json::from_str(&text).map_err(|e| {
            format!(
                "{} isn't valid JSON ({e}) — not touching it",
                path.display()
            )
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(serde_json::json!({})),
        Err(e) => Err(format!("can't read {}: {e}", path.display())),
    }
}

/// Atomic write (temp + rename), keeping a one-time `.bak` of the original.
pub fn write_settings(path: &Path, settings: &serde_json::Value) -> Result<(), String> {
    let err = |e: std::io::Error| format!("can't write {}: {e}", path.display());
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(err)?;
    }
    let bak = path.with_extension("json.marina-bak");
    if path.exists() && !bak.exists() {
        std::fs::copy(path, &bak).map_err(err)?;
    }
    let tmp = path.with_extension("json.marina-tmp");
    let mut text = serde_json::to_string_pretty(settings).map_err(|e| e.to_string())?;
    text.push('\n');
    std::fs::write(&tmp, text).map_err(err)?;
    std::fs::rename(&tmp, path).map_err(err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::launcher::{Launcher, LauncherKind};
    use std::path::PathBuf;

    #[test]
    fn recognizes_dev_server_starts_and_ignores_the_rest() {
        for yes in [
            "pnpm dev",
            "npm run dev",
            "yarn start",
            "bun run dev --port 3000",
            "cd web && pnpm dev",
            "npx vite",
            "./node_modules/.bin/next dev",
            "cargo run --bin api",
            "uvicorn main:app --reload",
            "python manage.py runserver",
            "bin/rails s",
            "python3 -m http.server 8000",
            "PORT=3000 npm start",
        ] {
            assert!(starts_dev_server(yes), "{yes}");
        }
        for no in [
            "pnpm install",
            "npm run build",
            "npm test",
            "cargo test",
            "ls dev",
        ] {
            assert!(!starts_dev_server(no), "{no}");
        }
        // Known, accepted false positive: argument text that spells a start
        // command (`echo next dev`). The only effect is an extra context note.
        assert!(starts_dev_server("echo next dev"));
    }

    #[test]
    fn finds_hard_coded_ports_but_not_substitutions() {
        assert_eq!(hardcoded_port("pnpm dev --port 3000"), Some(3000));
        assert_eq!(hardcoded_port("vite --port=5173"), Some(5173));
        assert_eq!(hardcoded_port("rails s -p 4000"), Some(4000));
        assert_eq!(hardcoded_port("PORT=8080 npm start"), Some(8080));
        assert_eq!(hardcoded_port("pnpm dev --port $(marina port)"), None);
        assert_eq!(hardcoded_port("pnpm dev"), None);
    }

    fn snap_with(cwd: &str, port: u16, session: Option<&str>) -> Snapshot {
        let mut s = Snapshot::sample();
        s.targets.truncate(1); // client-portal · next dev · :3000
        s.targets[0].cwd = PathBuf::from(cwd);
        s.targets[0].ports = vec![port];
        s.targets[0].launcher = session.map(|id| Launcher {
            kind: LauncherKind::Agent,
            name: "claude".into(),
            pid: Some(10),
            alive: true,
            session: Some(id.into()),
            cwd: None,
            start_time: 0,
        });
        s
    }

    #[test]
    fn session_context_points_at_this_projects_running_server_and_port() {
        let snap = snap_with("/w/app-a", 3167, Some("s1"));
        let ctx = session_context(&snap, Some(Path::new("/w/app-a")), Some(3167)).unwrap();
        assert!(ctx.contains("already running for this project"), "{ctx}");
        assert!(ctx.contains("next dev · :3167 (via claude"), "{ctx}");
        assert!(ctx.contains("dev port is 3167"), "{ctx}");
        // nothing running, no port, no orphans -> silence
        let empty = Snapshot::empty();
        assert!(session_context(&empty, Some(Path::new("/w/x")), None).is_none());
    }

    #[test]
    fn pre_bash_warns_about_duplicates_and_hard_coded_ports() {
        let snap = snap_with("/w/app-a", 3167, None);
        let root = Some(Path::new("/w/app-a"));
        let dup = pre_bash_context("pnpm dev", &snap, root, Some(3167)).unwrap();
        assert!(dup.contains("already running on :3167"), "{dup}");
        assert!(dup.contains("marina free 3167"), "{dup}");
        // another worktree, hard-coding a port that belongs to someone else
        let other = Some(Path::new("/w/app-b"));
        let hc = pre_bash_context("pnpm dev --port 3167", &snap, other, Some(3402)).unwrap();
        assert!(hc.contains("this worktree's port is 3402"), "{hc}");
        assert!(
            hc.contains("port 3167 is already taken by client-portal"),
            "{hc}"
        );
        // clean case: own port via substitution, nothing running -> silence
        assert!(
            pre_bash_context("pnpm dev --port $(marina port)", &snap, other, Some(3402)).is_none()
        );
    }

    #[test]
    fn install_is_idempotent_and_leaves_other_settings_alone() {
        let mut s: serde_json::Value = serde_json::from_str(
            r#"{
              "model": "opus",
              "hooks": {
                "PreToolUse": [
                  {"matcher": "Edit", "hooks": [{"type": "command", "command": "prettier-hook"}]}
                ]
              },
              "permissions": {"allow": ["Bash(ls:*)"]}
            }"#,
        )
        .unwrap();
        install_into(&mut s, "marina", true).unwrap();
        install_into(&mut s, "marina", true).unwrap(); // twice: no duplicates
        let pre = s["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre.len(), 2, "user's hook kept, ours added once");
        assert_eq!(pre[0]["hooks"][0]["command"], "prettier-hook");
        assert_eq!(pre[1]["matcher"], "Bash");
        assert_eq!(pre[1]["hooks"][0]["command"], "marina hook pre-bash");
        assert_eq!(s["hooks"]["SessionEnd"][0]["hooks"][0]["timeout"], 10);
        // key order of the user's file is preserved
        let keys: Vec<&String> = s.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["model", "hooks", "permissions"]);
        let mut names: Vec<String> = installed(&s).into_iter().map(|(_, n)| n).collect();
        names.sort();
        assert_eq!(names, ["pre-bash", "session-end", "session-start"]);
    }

    #[test]
    fn no_cleanup_skips_session_end_and_uninstall_restores_the_file() {
        let original = serde_json::json!({
            "model": "opus",
            "hooks": {"Stop": [{"hooks": [{"type": "command", "command": "say done"}]}]}
        });
        let mut s = original.clone();
        install_into(&mut s, "/opt/bin/marina", false).unwrap();
        assert!(s["hooks"].get("SessionEnd").is_none());
        assert_eq!(installed(&s).len(), 2);
        strip(&mut s);
        assert_eq!(s, original, "uninstall leaves exactly what was there");
        // and a file that only had our hooks loses the `hooks` key entirely
        let mut only = serde_json::json!({});
        install_into(&mut only, "marina", true).unwrap();
        strip(&mut only);
        assert_eq!(only, serde_json::json!({}));
    }

    #[test]
    fn malformed_settings_are_refused() {
        let mut arr = serde_json::json!([1, 2]);
        assert!(install_into(&mut arr, "marina", true).is_err());
        let mut bad_hooks = serde_json::json!({"hooks": "nope"});
        assert!(install_into(&mut bad_hooks, "marina", true).is_err());
    }

    #[test]
    fn session_targets_match_by_session_id_only() {
        let snap = snap_with("/w/app-a", 3167, Some("s1"));
        assert_eq!(session_targets(&snap, "s1").len(), 1);
        assert!(session_targets(&snap, "s2").is_empty());
    }
}
