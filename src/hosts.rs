//! Terminal hosts: whatever owns an agent session's terminal — a multiplexer
//! (herdr, tmux) or nothing marina can drive. A host lets marina *jump* to a
//! session (focus its pane) and, for richer hosts, read what they know about
//! it: herdr reports the agent's status (including **blocked** on a question,
//! which marina can't see from outside) and the session's task title.
//!
//! Each host is an adapter keyed by something marina already has — the
//! agent's pid (herdr), its tty (tmux, iTerm2, Terminal, WezTerm) or its
//! directory (Ghostty, whose scripting doesn't expose ttys) — so adding one
//! is another lookup arm plus a `jump` arm. Without a host, jumping falls
//! back to copying a resume command.
//!
//! Multiplexers are queried only when their server process is running,
//! through their own CLIs, with a deadline, at most every few seconds.
//! Terminal apps are recognised from the agent's ancestry alone and only
//! scripted on a keystroke — jumping, and for iTerm2 also the preview and
//! answering (AppleScript on macOS; `wezterm cli`) — so sampling never
//! triggers an automation prompt.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::sources::ProcInfo;

/// The terminal pane a process runs in, from its environment: the four
/// variables below and nothing else. Zellij and kitty set them in every
/// process they start, so they name the pane exactly.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TermEnv {
    /// `ZELLIJ_SESSION_NAME`, `ZELLIJ_PANE_ID`.
    pub zellij: Option<(String, String)>,
    /// `KITTY_WINDOW_ID`, and `KITTY_LISTEN_ON` when remote control
    /// listens on a socket.
    pub kitty: Option<(String, Option<String>)>,
}

impl TermEnv {
    pub fn from_environ(environ: &[std::ffi::OsString]) -> Option<TermEnv> {
        let mut get = HashMap::new();
        for kv in environ {
            let kv = kv.to_string_lossy();
            if let Some((k, v)) = kv.split_once('=') {
                if matches!(
                    k,
                    "ZELLIJ_SESSION_NAME"
                        | "ZELLIJ_PANE_ID"
                        | "KITTY_WINDOW_ID"
                        | "KITTY_LISTEN_ON"
                ) {
                    get.insert(k.to_string(), v.to_string());
                }
            }
        }
        // short, plain values only: these go into command lines as argv
        let ok = |v: &String| {
            !v.is_empty()
                && v.len() <= 200
                && v.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_.:/@".contains(c))
        };
        let val = |k: &str| get.get(k).filter(|v| ok(v)).cloned();
        let t = TermEnv {
            zellij: val("ZELLIJ_SESSION_NAME").zip(val("ZELLIJ_PANE_ID")),
            kitty: val("KITTY_WINDOW_ID").map(|w| (w, val("KITTY_LISTEN_ON"))),
        };
        (t.zellij.is_some() || t.kitty.is_some()).then_some(t)
    }
}

const TTL: Duration = Duration::from_secs(3);
const TIMEOUT: Duration = Duration::from_millis(1500);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostKind {
    Herdr,
    Tmux,
    ITerm,
    Terminal,
    Ghostty,
    WezTerm,
    /// A Zellij pane: target `<session> <pane id>`.
    Zellij,
    /// A kitty window with remote control listening: target
    /// `<window id> <socket>`.
    Kitty,
}

impl HostKind {
    pub fn as_str(self) -> &'static str {
        match self {
            HostKind::Herdr => "herdr",
            HostKind::Tmux => "tmux",
            HostKind::ITerm => "iterm",
            HostKind::Terminal => "terminal",
            HostKind::Ghostty => "ghostty",
            HostKind::WezTerm => "wezterm",
            HostKind::Zellij => "zellij",
            HostKind::Kitty => "kitty",
        }
    }

    /// Proper name, for sentences.
    pub fn name(self) -> &'static str {
        match self {
            HostKind::Herdr => "herdr",
            HostKind::Tmux => "tmux",
            HostKind::ITerm => "iTerm2",
            HostKind::Terminal => "Terminal",
            HostKind::Ghostty => "Ghostty",
            HostKind::WezTerm => "WezTerm",
            HostKind::Zellij => "Zellij",
            HostKind::Kitty => "kitty",
        }
    }

    /// The terminal app that owns a process, from one ancestor's argv0.
    fn terminal_app(argv0: &str) -> Option<HostKind> {
        let base = argv0.rsplit('/').next().unwrap_or(argv0);
        if argv0.contains("/iTerm.app/") || base.starts_with("iTermServer") {
            Some(HostKind::ITerm)
        } else if argv0.contains("/Terminal.app/") {
            Some(HostKind::Terminal)
        } else if argv0.contains("/Ghostty.app/") || base == "ghostty" {
            Some(HostKind::Ghostty)
        } else if base == "wezterm-gui" || base == "wezterm-mux-server" {
            Some(HostKind::WezTerm)
        } else if argv0.contains("/kitty.app/") || base == "kitty" {
            Some(HostKind::Kitty)
        } else {
            None
        }
    }
}

/// What a host reports about an agent's state, when it can.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostStatus {
    Working,
    /// Waiting on the human: an approval prompt or a question.
    Blocked,
    /// Finished and not yet looked at.
    Done,
    Idle,
}

/// Where an agent session lives and what its host says about it.
#[derive(Clone, Debug, PartialEq)]
pub struct HostInfo {
    pub kind: HostKind,
    /// The host's id for the pane: herdr `w2S:p1`, tmux `main:1.0`; the tty
    /// for iTerm2/Terminal/WezTerm, the directory for Ghostty.
    pub target: String,
    pub status: Option<HostStatus>,
    /// The session's task title (herdr: the terminal title the agent sets).
    pub title: Option<String>,
    /// The host's view of the session id — fresher than env markers, which
    /// older helpers keep from before a `/clear`.
    pub session: Option<String>,
    /// The terminal app's own process, when it is a GUI app marina can bring
    /// forward: the one a multiplexer's client runs in (raised after the
    /// multiplexer switches panes), or the terminal itself.
    pub app_pid: Option<u32>,
    /// For a multiplexer, the terminal app its client runs in (herdr in
    /// Ghostty).
    pub outer: Option<HostKind>,
}

impl HostInfo {
    /// Where, in a column: `herdr·ghostty`, `ghostty`, `tmux·iterm`.
    pub fn short(&self) -> String {
        match self.outer {
            Some(o) => format!("{}·{}", self.kind.as_str(), o.as_str()),
            None => self.kind.as_str().to_string(),
        }
    }

    /// `herdr pane w2S:p1 in Ghostty`, `iTerm2 session on /dev/ttys004`, …
    pub fn describe(&self) -> String {
        let place = self.place();
        match self.outer {
            Some(o) => format!("{place} in {}", o.name()),
            None => place,
        }
    }

    fn place(&self) -> String {
        let t = &self.target;
        match self.kind {
            HostKind::Herdr => format!("herdr pane {t}"),
            HostKind::Tmux => format!("tmux pane {t}"),
            HostKind::ITerm => format!("iTerm2 session on {t}"),
            HostKind::Terminal => format!("Terminal tab on {t}"),
            HostKind::Ghostty => format!("Ghostty terminal in {t}"),
            HostKind::WezTerm => format!("WezTerm pane on {t}"),
            HostKind::Zellij => {
                let (s, p) = t.split_once(' ').unwrap_or((t, ""));
                format!("Zellij pane {p} in session {s}")
            }
            HostKind::Kitty => {
                let w = t.split_once(' ').map(|x| x.0).unwrap_or(t);
                format!("kitty window {w}")
            }
        }
    }
}

#[derive(Deserialize)]
struct HerdrList {
    result: HerdrAgents,
}
#[derive(Deserialize)]
struct HerdrAgents {
    agents: Vec<HerdrAgent>,
}
#[derive(Deserialize)]
struct HerdrAgent {
    pane_id: String,
    agent_status: Option<String>,
    terminal_title_stripped: Option<String>,
    agent_session: Option<HerdrSession>,
}
#[derive(Deserialize)]
struct HerdrSession {
    value: Option<String>,
}
#[derive(Deserialize)]
struct HerdrProcInfo {
    result: HerdrProcResult,
}
#[derive(Deserialize)]
struct HerdrProcResult {
    process_info: HerdrProc,
}
#[derive(Deserialize)]
struct HerdrProc {
    foreground_process_group_id: Option<u32>,
}

fn herdr_status(s: &str) -> Option<HostStatus> {
    match s {
        "working" => Some(HostStatus::Working),
        "blocked" => Some(HostStatus::Blocked),
        "done" => Some(HostStatus::Done),
        "idle" => Some(HostStatus::Idle),
        _ => None, // `unknown`: no claim either way
    }
}

/// Parse `herdr agent list` JSON into (pane id, info).
fn parse_herdr_list(json: &str) -> Vec<HostInfo> {
    let Ok(list) = serde_json::from_str::<HerdrList>(json) else {
        return Vec::new();
    };
    list.result
        .agents
        .into_iter()
        .map(|a| HostInfo {
            kind: HostKind::Herdr,
            target: a.pane_id,
            status: a.agent_status.as_deref().and_then(herdr_status),
            title: a.terminal_title_stripped.filter(|t| !t.trim().is_empty()),
            session: a
                .agent_session
                .and_then(|s| s.value)
                .filter(|v| crate::launcher::is_session_id(v)),
            app_pid: None,
            outer: None,
        })
        .collect()
}

fn parse_herdr_pid(json: &str) -> Option<u32> {
    serde_json::from_str::<HerdrProcInfo>(json)
        .ok()?
        .result
        .process_info
        .foreground_process_group_id
}

/// `tmux list-panes -a -F '#{pane_tty}\t#{session_name}:#{window_index}.#{pane_index}'`
fn parse_tmux_panes(text: &str) -> HashMap<PathBuf, String> {
    text.lines()
        .filter_map(|l| l.split_once('\t'))
        .map(|(tty, target)| (PathBuf::from(tty), target.to_string()))
        .collect()
}

fn run(program: &str, args: &[&str]) -> Option<String> {
    let mut cmd = Command::new(program);
    cmd.args(args);
    crate::exec::output(cmd, TIMEOUT)
}

/// Host lookups, cached across sampler ticks.
#[derive(Default)]
pub struct Hosts {
    at: Option<Instant>,
    /// agent pid -> herdr pane (pane -> pid is stable while the agent lives).
    herdr_pids: HashMap<String, u32>,
    herdr: Vec<HostInfo>,
    tmux: HashMap<PathBuf, String>,
}

impl Hosts {
    /// Refresh (at most every `TTL`) whichever hosts are running.
    fn refresh(&mut self, procs: &HashMap<u32, ProcInfo>) {
        if self.at.is_some_and(|at| at.elapsed() < TTL) {
            return;
        }
        self.at = Some(Instant::now());
        let running = |name: &str, arg: Option<&str>| {
            procs.values().any(|p| {
                p.argv
                    .first()
                    .is_some_and(|a| a.rsplit('/').next() == Some(name))
                    && arg.is_none_or(|x| p.argv.iter().any(|a| a == x))
            })
        };
        self.herdr = if running("herdr", Some("server")) {
            run("herdr", &["agent", "list"])
                .map(|j| parse_herdr_list(&j))
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        // pane -> agent pid: ask only for panes we haven't resolved, and
        // forget panes that are gone or whose pid is no longer alive.
        let panes: Vec<String> = self.herdr.iter().map(|h| h.target.clone()).collect();
        self.herdr_pids
            .retain(|pane, pid| panes.contains(pane) && procs.contains_key(pid));
        for pane in panes {
            if let std::collections::hash_map::Entry::Vacant(slot) = self.herdr_pids.entry(pane) {
                if let Some(pid) = run("herdr", &["pane", "process-info", "--pane", slot.key()])
                    .and_then(|j| parse_herdr_pid(&j))
                {
                    slot.insert(pid);
                }
            }
        }
        self.tmux = if procs.values().any(|p| p.name.starts_with("tmux")) {
            run(
                "tmux",
                &[
                    "list-panes",
                    "-a",
                    "-F",
                    "#{pane_tty}\t#{session_name}:#{window_index}.#{pane_index}",
                ],
            )
            .map(|t| parse_tmux_panes(&t))
            .unwrap_or_default()
        } else {
            HashMap::new()
        };
    }

    /// The host of the agent `pid` in `cwd` whose terminal is `tty`, if any.
    pub fn lookup(
        &mut self,
        procs: &HashMap<u32, ProcInfo>,
        pid: u32,
        tty: Option<&Path>,
        cwd: &Path,
    ) -> Option<HostInfo> {
        self.refresh(procs);
        let by_pid = self
            .herdr_pids
            .iter()
            .find(|(_, &p)| p == pid)
            .and_then(|(pane, _)| self.herdr.iter().find(|h| &h.target == pane));
        if let Some(h) = by_pid {
            let mut h = h.clone();
            (h.outer, h.app_pid) = outer_of(procs, pid, "herdr").unzip();
            return Some(h);
        }
        let tty = tty?;
        let mut outer = None;
        let term = procs
            .get(&pid)
            .and_then(|p| p.term.clone())
            .unwrap_or_default();
        let (kind, target, app_pid) = match self.tmux.get(tty) {
            Some(pane) => {
                let o = outer_of(procs, pid, "tmux");
                outer = o.map(|o| o.0);
                (HostKind::Tmux, pane.clone(), o.map(|o| o.1))
            }
            // Zellij and kitty name the pane in its processes' env; tmux's
            // own pane listing wins over env inherited from outside it
            None if term.zellij.is_some() => {
                let (s, p) = term.zellij.clone().unwrap_or_default();
                let o = outer_of(procs, pid, "zellij");
                outer = o.map(|o| o.0);
                (HostKind::Zellij, format!("{s} {p}"), o.map(|o| o.1))
            }
            None if term.kitty.as_ref().is_some_and(|k| k.1.is_some()) => {
                let (w, sock) = term.kitty.clone().unwrap_or_default();
                let app = terminal_of(procs, pid).map(|t| t.1);
                (
                    HostKind::Kitty,
                    format!("{w} {}", sock.unwrap_or_default()),
                    app,
                )
            }
            None => match terminal_of(procs, pid)? {
                // kitty without a remote-control socket: nothing to drive
                (HostKind::Kitty, _) => return None,
                (HostKind::Ghostty, app) => {
                    (HostKind::Ghostty, cwd.display().to_string(), Some(app))
                }
                // iTermServer isn't the app; the app is found by name
                (HostKind::ITerm, _) => (HostKind::ITerm, tty.display().to_string(), None),
                (kind, app) => (kind, tty.display().to_string(), Some(app)),
            },
        };
        Some(HostInfo {
            kind,
            target,
            status: None,
            title: None,
            session: None,
            app_pid,
            outer,
        })
    }
}

/// The terminal app a multiplexer session `pid` is shown in: up its own
/// ancestry (herdr's server runs under the client that started it), else
/// wherever a `client` process (`herdr`, `tmux`) runs — a detached server
/// has no terminal above it. With several clients, the first found.
fn outer_of(procs: &HashMap<u32, ProcInfo>, pid: u32, client: &str) -> Option<(HostKind, u32)> {
    terminal_of(procs, pid).or_else(|| {
        procs
            .values()
            .filter(|p| {
                p.argv
                    .first()
                    .is_some_and(|a| a.rsplit('/').next() == Some(client))
            })
            .find_map(|p| terminal_of(procs, p.pid))
    })
}

/// The terminal app `pid` runs in, and that app's pid: the first ancestor
/// that is one.
fn terminal_of(procs: &HashMap<u32, ProcInfo>, pid: u32) -> Option<(HostKind, u32)> {
    let app = |p: &ProcInfo| p.argv.first().and_then(|a| HostKind::terminal_app(a));
    let mut cur = pid;
    for _ in 0..64 {
        let p = procs.get(&cur)?;
        if cur != pid {
            if let Some(kind) = app(p) {
                return Some((kind, cur));
            }
        }
        cur = match p.ppid {
            Some(pp) => pp,
            // A root `login` hides its parent from us; a terminal app's
            // children don't hide. Only a few terminal processes to ask.
            None => {
                return procs
                    .values()
                    .filter_map(|t| Some((app(t)?, t.pid)))
                    .find(|&(_, t)| crate::sources::child_pids(t).contains(&cur));
            }
        };
    }
    None
}

/// Run an AppleScript `on run argv` handler with `args` — passed as argv,
/// never spliced into the script. `Some(stdout)` on success.
fn osascript(script: &str, args: &[&str]) -> Option<String> {
    let mut cmd = Command::new("osascript");
    cmd.arg("-e").arg(script).args(args);
    // a first jump may wait on the automation permission prompt
    crate::exec::output(cmd, Duration::from_secs(20)).map(|s| s.trim().to_string())
}

/// After a multiplexer switched panes, bring the terminal app showing it
/// forward too, in case marina is in another window.
fn raise(host: &HostInfo, msg: String) -> String {
    match (host.outer, host.app_pid) {
        (Some(o), Some(pid)) if activate_pid(pid) => format!("{msg} · raised {}", o.name()),
        _ => msg,
    }
}

/// Bring the GUI app with process id `pid` to the front.
fn activate_pid(pid: u32) -> bool {
    if !cfg!(target_os = "macos") {
        return false;
    }
    let script = format!(
        "ObjC.import('AppKit'); var a = $.NSRunningApplication.runningApplicationWithProcessIdentifier({pid}); a ? a.activateWithOptions(0) : false"
    );
    let mut cmd = Command::new("osascript");
    cmd.args(["-l", "JavaScript", "-e", &script]);
    crate::exec::output(cmd, TIMEOUT).is_some_and(|o| o.trim() == "true")
}

const ITERM_FOCUS: &str = r#"on run argv
  tell application "iTerm2"
    repeat with w in windows
      repeat with t in tabs of w
        repeat with s in sessions of t
          if tty of s is (item 1 of argv) then
            select w
            tell t to select
            tell s to select
            activate
            return "ok"
          end if
        end repeat
      end repeat
    end repeat
  end tell
  return "missing"
end run"#;

const TERMINAL_FOCUS: &str = r#"on run argv
  tell application "Terminal"
    repeat with w in windows
      repeat with t in tabs of w
        if tty of t is (item 1 of argv) then
          set selected tab of w to t
          set index of w to 1
          activate
          return "ok"
        end if
      end repeat
    end repeat
  end tell
  return "missing"
end run"#;

/// Ghostty doesn't expose ttys: focus a terminal in the session's directory,
/// preferring one whose title is the agent's (Claude Code titles its
/// terminal `✳ …`, or a braille spinner while working; opencode `OC | …`) so
/// marina's own terminal in the same directory isn't picked. JXA, because it
/// can address the instance by pid — AppleScript by name reaches only one of
/// several running Ghostty instances. argv: pid (0 = by name), dir, agent.
const GHOSTTY_FOCUS: &str = r#"function run(argv) {
  var pid = parseInt(argv[0], 10), dir = argv[1], agent = argv[2].toLowerCase();
  var g = pid > 0 ? Application(pid) : Application("Ghostty");
  var ts = g.terminals().filter(function (t) { return t.workingDirectory() === dir; });
  if (ts.length === 0) return "missing";
  var mine = function (t) {
    var n = t.name(), l = n.toLowerCase(), c = n.charCodeAt(0);
    return l.indexOf(agent) >= 0 || n.charAt(0) === "\u2733" ||
      (c >= 0x2800 && c <= 0x28ff) || (agent === "opencode" && l.indexOf("oc |") === 0);
  };
  var hits = ts.filter(mine);
  g.focus((hits.length ? hits : ts)[0]);
  g.activate();
  return String(hits.length || ts.length);
}"#;

/// The visible screen of the iTerm2 session on a tty (argv 1). Asks only a
/// running iTerm2 — `tell` alone would launch it.
const ITERM_READ: &str = r#"on run argv
  if application "iTerm2" is not running then return "marina:not-running"
  tell application "iTerm2"
    repeat with w in windows
      repeat with t in tabs of w
        repeat with s in sessions of t
          if tty of s is (item 1 of argv) then return contents of s
        end repeat
      end repeat
    end repeat
  end tell
  return "marina:missing"
end run"#;

/// Type into the iTerm2 session on a tty (argv 1): `enter`, `esc`, or
/// `text` with the text as argv 3 — as keystrokes, not a paste, like
/// `tmux send-keys -l`. Only into a running iTerm2.
const ITERM_WRITE: &str = r#"on run argv
  if application "iTerm2" is not running then return "marina:not-running"
  tell application "iTerm2"
    repeat with w in windows
      repeat with t in tabs of w
        repeat with s in sessions of t
          if tty of s is (item 1 of argv) then
            set k to item 2 of argv
            if k is "enter" then
              tell s to write text ""
            else if k is "esc" then
              tell s to write text (character id 27) newline no
            else
              tell s to write text (item 3 of argv) newline no
            end if
            return "ok"
          end if
        end repeat
      end repeat
    end repeat
  end tell
  return "missing"
end run"#;

/// The screen text of the iTerm2 session on `tty`, or why not.
fn iterm_read(tty: &str) -> Result<String, String> {
    match osascript(ITERM_READ, &[tty]).as_deref() {
        Some("marina:not-running") => Err("iTerm2 isn't running".into()),
        Some("marina:missing") => Err(format!("no iTerm2 session on {tty} any more")),
        Some(text) => Ok(text.to_string()),
        None => Err("couldn't script iTerm2 — allow marina's terminal to control it under System Settings › Privacy › Automation".into()),
    }
}

/// Type into the iTerm2 session on `tty`: `Some` once it took it.
fn iterm_write(tty: &str, key: &str, text: &str) -> Option<String> {
    osascript(ITERM_WRITE, &[tty, key, text]).filter(|o| o == "ok")
}

#[derive(Deserialize)]
struct WezPane {
    pane_id: u64,
    #[serde(default)]
    tty_name: Option<String>,
}

fn wezterm_pane(json: &str, tty: &str) -> Option<u64> {
    serde_json::from_str::<Vec<WezPane>>(json)
        .ok()?
        .into_iter()
        .find(|p| p.tty_name.as_deref() == Some(tty))
        .map(|p| p.pane_id)
}

/// The WezTerm pane on `tty`, by id.
fn wez_pane_of(tty: &str) -> Option<String> {
    run("wezterm", &["cli", "list", "--format", "json"])
        .and_then(|j| wezterm_pane(&j, tty))
        .map(|id| id.to_string())
}

/// `<session> <pane>` / `<window> <socket>` targets, split.
fn two(target: &str) -> (&str, &str) {
    target.split_once(' ').unwrap_or((target, ""))
}

/// Run `program` with `input` on its stdin; `Some(stdout)` on success.
fn run_stdin(program: &str, args: &[&str], input: &str) -> Option<String> {
    use std::io::Write;
    let mut child = Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    child.stdin.take()?.write_all(input.as_bytes()).ok()?;
    let out = child.wait_with_output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `kitten @ --to <socket> <args…>` for window `w`'s commands.
fn kitten(sock: &str, args: &[&str]) -> Option<String> {
    let mut a = vec!["@", "--to", sock];
    a.extend_from_slice(args);
    run("kitten", &a)
}

/// Focus the session's pane. Returns a line for the status bar. `agent`
/// (`claude`, `opencode`) helps pick it where a host can only narrow down by
/// directory.
pub fn jump(host: &HostInfo, agent: &str) -> Result<String, String> {
    match host.kind {
        HostKind::Herdr => run("herdr", &["agent", "focus", &host.target])
            .map(|_| raise(host, format!("focused herdr pane {}", host.target)))
            .ok_or_else(|| format!("herdr couldn't focus {}", host.target)),
        HostKind::Tmux => {
            // Inside tmux, switch this client; outside, make it the active
            // pane so whichever client is attached shows it.
            let window = host
                .target
                .rsplit_once('.')
                .map(|(w, _)| w)
                .unwrap_or(&host.target);
            let ok = if std::env::var_os("TMUX").is_some() {
                run("tmux", &["switch-client", "-t", &host.target]).is_some()
            } else {
                run("tmux", &["select-window", "-t", window]).is_some()
            } && run("tmux", &["select-pane", "-t", &host.target]).is_some();
            if ok {
                Ok(raise(
                    host,
                    format!("switched to tmux pane {}", host.target),
                ))
            } else {
                Err(format!("tmux couldn't select {}", host.target))
            }
        }
        HostKind::ITerm | HostKind::Terminal => {
            let script = if host.kind == HostKind::ITerm {
                ITERM_FOCUS
            } else {
                TERMINAL_FOCUS
            };
            match osascript(script, &[&host.target]).as_deref() {
                Some("ok") => Ok(format!("focused the {}", host.describe())),
                Some(_) => Err(format!("no {} any more", host.describe())),
                None => Err(format!(
                    "couldn't script {} — allow marina's terminal under System Settings › Privacy › Automation",
                    host.kind.as_str()
                )),
            }
        }
        HostKind::Ghostty => {
            let pid = host.app_pid.unwrap_or(0).to_string();
            let mut cmd = Command::new("osascript");
            cmd.args(["-l", "JavaScript", "-e", GHOSTTY_FOCUS]).args([
                pid.as_str(),
                host.target.as_str(),
                agent,
            ]);
            // a first jump may wait on the automation permission prompt
            match crate::exec::output(cmd, Duration::from_secs(20))
                .map(|o| o.trim().to_string())
                .as_deref()
            {
                Some("1") => Ok(format!("focused the {}", host.describe())),
                Some(n) if n.parse::<u32>().is_ok() => Ok(format!(
                    "focused a Ghostty terminal in {} ({n} look like {agent} there)",
                    host.target
                )),
                Some(_) if host.app_pid.is_some_and(activate_pid) => Ok(format!(
                    "brought Ghostty forward — no terminal in {} reports that directory",
                    host.target
                )),
                Some(_) => Err(format!("no {} any more", host.describe())),
                None => Err(
                    "couldn't script Ghostty — allow marina's terminal under System Settings › Privacy › Automation"
                        .into(),
                ),
            }
        }
        HostKind::WezTerm => {
            let pane = run("wezterm", &["cli", "list", "--format", "json"])
                .and_then(|j| wezterm_pane(&j, &host.target))
                .ok_or_else(|| format!("no {} any more", host.describe()))?;
            let id = pane.to_string();
            run("wezterm", &["cli", "activate-pane", "--pane-id", &id])
                .ok_or_else(|| format!("wezterm couldn't activate pane {id}"))?;
            if cfg!(target_os = "macos") {
                let _ = run("open", &["-a", "WezTerm"]);
            }
            Ok(format!("focused WezTerm pane {id}"))
        }
        HostKind::Zellij => {
            let (s, p) = two(&host.target);
            run("zellij", &["--session", s, "action", "focus-pane-id", p])
                .map(|_| raise(host, format!("focused Zellij pane {p}")))
                .ok_or_else(|| format!("zellij couldn't focus pane {p} in {s}"))
        }
        HostKind::Kitty => {
            let (w, sock) = two(&host.target);
            let m = format!("id:{w}");
            kitten(sock, &["focus-window", "--match", &m])
                .ok_or_else(|| format!("kitty couldn't focus window {w}"))?;
            if let Some(app) = host.app_pid {
                activate_pid(app);
            }
            Ok(format!("focused kitty window {w}"))
        }
    }
}

/// Something to send to a session from marina — only ever on an explicit
/// keystroke on that session's row.
#[derive(Clone, Debug, PartialEq)]
pub enum Send {
    /// Accept the highlighted option of the dialog it's waiting on.
    Approve,
    /// Dismiss the dialog (Claude Code: "No, and tell Claude what to do").
    Reject,
    /// Submit a prompt; dismisses a waiting dialog first.
    Reply(String),
}

/// The hosts `interactive` names, for sentences.
pub const TYPABLE: &str = "herdr, tmux, Zellij, iTerm2, WezTerm and kitty";

impl HostKind {
    /// Hosts marina can read a session's screen from and type into. iTerm2
    /// is scripted only on a keystroke (preview, answer), never by the
    /// sampler, so sampling never raises an automation prompt.
    pub fn interactive(self) -> bool {
        matches!(
            self,
            HostKind::Herdr
                | HostKind::Tmux
                | HostKind::ITerm
                | HostKind::WezTerm
                | HostKind::Zellij
                | HostKind::Kitty
        )
    }
}

/// The last `lines` rows of the session's screen, from its multiplexer or
/// terminal app.
pub fn read_screen(host: &HostInfo, lines: usize) -> Option<Vec<String>> {
    read_screen_or_why(host, lines).ok()
}

/// [`read_screen`], saying why when it can't.
pub fn read_screen_or_why(host: &HostInfo, lines: usize) -> Result<Vec<String>, String> {
    let n = lines.to_string();
    let failed = || format!("couldn't read the {}", host.describe());
    let text = match host.kind {
        HostKind::Herdr => run(
            "herdr",
            &[
                "agent",
                "read",
                &host.target,
                "--source",
                "visible",
                "--lines",
                &n,
            ],
        )
        .ok_or_else(failed)?,
        HostKind::Tmux => {
            run("tmux", &["capture-pane", "-p", "-t", &host.target]).ok_or_else(failed)?
        }
        HostKind::ITerm => iterm_read(&host.target)?,
        HostKind::WezTerm => {
            let id = wez_pane_of(&host.target)
                .ok_or_else(|| format!("no {} any more", host.describe()))?;
            run("wezterm", &["cli", "get-text", "--pane-id", &id]).ok_or_else(failed)?
        }
        HostKind::Zellij => {
            let (s, p) = two(&host.target);
            run(
                "zellij",
                &["--session", s, "action", "dump-screen", "--pane-id", p],
            )
            .ok_or_else(failed)?
        }
        HostKind::Kitty => {
            let (w, sock) = two(&host.target);
            let m = format!("id:{w}");
            kitten(sock, &["get-text", "--match", &m]).ok_or_else(|| {
                "couldn't read the kitty window — is allow_remote_control on?".to_string()
            })?
        }
        _ => {
            return Err(format!(
                "marina can read the screen of {TYPABLE} sessions only"
            ))
        }
    };
    let mut rows: Vec<String> = text.lines().map(|l| l.trim_end().to_string()).collect();
    while rows.last().is_some_and(|l| l.is_empty()) {
        rows.pop();
    }
    let skip = rows.len().saturating_sub(lines);
    Ok(rows.split_off(skip))
}

/// Does the screen show an approval or question dialog *now*? Only the
/// bottom rows count (a dialog sits at the bottom; the same words higher up
/// are old output), and it needs a choice row: a numbered option under the
/// cursor (`❯ 1.`), or a picker's "Enter to confirm".
pub fn shows_dialog(screen: &[String]) -> bool {
    let bottom: Vec<&str> = screen
        .iter()
        .map(|l| l.trim_start())
        .filter(|l| !l.is_empty())
        .rev()
        .take(14)
        .collect();
    bottom.iter().any(|t| t.contains("Enter to confirm"))
        || bottom.iter().any(|t| {
            t.strip_prefix('❯')
                .map(str::trim_start)
                .is_some_and(|c| c.starts_with("1."))
        })
}

/// The dialog choice under the cursor, without its number: `Yes`,
/// `No, exit`. The cursor line is `❯ <choice>` followed by more indented
/// choices or numbered — unlike the input prompt's `❯ `.
pub fn highlighted_choice(screen: &[String]) -> Option<String> {
    let numbered = |t: &str| {
        let d = t.trim_start_matches(|c: char| c.is_ascii_digit());
        d.len() < t.len() && d.starts_with(". ")
    };
    screen.iter().enumerate().rev().find_map(|(i, l)| {
        let t = l.trim_start().strip_prefix('❯')?.trim();
        if t.is_empty() {
            return None;
        }
        let next_is_choice = screen[i + 1..]
            .iter()
            .find(|n| !n.trim().is_empty())
            .is_some_and(|n| n.starts_with("  ") && !n.trim_start().starts_with('─'));
        if !(numbered(t) || next_is_choice) {
            return None;
        }
        let choice = if numbered(t) {
            t.split_once(". ").map(|x| x.1).unwrap_or(t)
        } else {
            t
        };
        Some(choice.trim().to_string())
    })
}

/// Approving sends Enter, which picks the highlighted choice: only do it
/// when that choice is a yes. (Claude Code's folder-trust prompt, for one,
/// highlights "No, exit".)
fn approvable(screen: &[String]) -> Result<(), String> {
    match highlighted_choice(screen) {
        Some(c) if c.to_lowercase().starts_with("yes") => Ok(()),
        Some(c) => Err(format!(
            "not approving: the highlighted choice is \"{c}\" — answer it there (Enter jumps) or with r"
        )),
        None => Err("no choice is highlighted on its screen — nothing to approve".into()),
    }
}

/// Press Enter (`enter`) or Esc (`esc`) in a WezTerm, Zellij or kitty pane.
fn type_key(host: &HostInfo, key: &str) -> Option<String> {
    let bytes = if key == "enter" { "\r" } else { "\x1b" };
    match host.kind {
        HostKind::WezTerm => {
            let id = wez_pane_of(&host.target)?;
            run(
                "wezterm",
                &["cli", "send-text", "--pane-id", &id, "--no-paste", bytes],
            )
        }
        HostKind::Zellij => {
            let (s, p) = two(&host.target);
            let code = if key == "enter" { "13" } else { "27" };
            run(
                "zellij",
                &["--session", s, "action", "write", "--pane-id", p, code],
            )
        }
        HostKind::Kitty => {
            let (w, sock) = two(&host.target);
            let m = format!("id:{w}");
            let k = if key == "enter" { "enter" } else { "escape" };
            kitten(sock, &["send-key", "--match", &m, k])
        }
        _ => None,
    }
}

/// Type `text` into a WezTerm, Zellij or kitty pane as keystrokes, not a
/// paste — and never through anything that would interpret it.
fn type_text(host: &HostInfo, text: &str) -> Option<String> {
    match host.kind {
        HostKind::WezTerm => {
            let id = wez_pane_of(&host.target)?;
            run(
                "wezterm",
                &[
                    "cli",
                    "send-text",
                    "--pane-id",
                    &id,
                    "--no-paste",
                    "--",
                    text,
                ],
            )
        }
        HostKind::Zellij => {
            let (s, p) = two(&host.target);
            run(
                "zellij",
                &[
                    "--session",
                    s,
                    "action",
                    "write-chars",
                    "--pane-id",
                    p,
                    "--",
                    text,
                ],
            )
        }
        HostKind::Kitty => {
            // send-text interprets escapes in its argument; stdin is taken as-is
            let (w, sock) = two(&host.target);
            let m = format!("id:{w}");
            run_stdin(
                "kitten",
                &["@", "--to", sock, "send-text", "--match", &m, "--stdin"],
                text,
            )
        }
        _ => None,
    }
}

/// Send `what` to the session. Its screen is read again first, so nothing
/// acts on a stale view: approve only when the highlighted choice is a yes,
/// reject only when a dialog is up (or herdr says it's blocked), and a
/// reply dismisses a dialog first only when one is up.
pub fn send(host: &HostInfo, what: &Send) -> Result<String, String> {
    let t = host.target.as_str();
    let ok = |o: Option<String>, what: &str| {
        o.map(|_| format!("{what} → {}", host.describe()))
            .ok_or_else(|| format!("{} didn't take it", host.kind.name()))
    };
    let screen = read_screen_or_why(host, 40)?;
    let dialog = shows_dialog(&screen) || host.status == Some(HostStatus::Blocked);
    match what {
        Send::Approve => approvable(&screen)?,
        Send::Reject if !dialog => {
            return Err(
                "no dialog on its screen now — not sending Esc (it would interrupt the agent)"
                    .into(),
            )
        }
        _ => {}
    }
    match (host.kind, what) {
        (HostKind::Herdr, Send::Approve) => ok(
            run("herdr", &["agent", "send-keys", t, "enter"]),
            "approved",
        ),
        (HostKind::Herdr, Send::Reject) => {
            ok(run("herdr", &["agent", "send-keys", t, "esc"]), "rejected")
        }
        (HostKind::Herdr, Send::Reply(text)) => {
            // herdr's parser has no `--`: keep a leading dash from reading
            // as a flag
            let text = if text.starts_with('-') {
                format!(" {text}")
            } else {
                text.clone()
            };
            if dialog {
                run("herdr", &["agent", "send-keys", t, "esc"])
                    .ok_or("herdr couldn't dismiss the dialog")?;
            }
            // herdr refuses a prompt while it still sees the dialog; give
            // the agent a moment to leave it
            for _ in 0..6 {
                if let Some(o) = run("herdr", &["agent", "prompt", t, &text]) {
                    return ok(Some(o), "sent");
                }
                std::thread::sleep(Duration::from_millis(400));
            }
            Err("herdr didn't accept the prompt (is the agent still at a dialog?)".into())
        }
        (HostKind::Tmux, Send::Approve) => {
            ok(run("tmux", &["send-keys", "-t", t, "Enter"]), "approved")
        }
        (HostKind::Tmux, Send::Reject) => {
            ok(run("tmux", &["send-keys", "-t", t, "Escape"]), "rejected")
        }
        (HostKind::Tmux, Send::Reply(text)) => {
            if dialog {
                run("tmux", &["send-keys", "-t", t, "Escape"])
                    .ok_or("tmux couldn't dismiss the dialog")?;
                std::thread::sleep(Duration::from_millis(400));
            }
            // literal text, then Enter as a key
            run("tmux", &["send-keys", "-t", t, "-l", "--", text])
                .ok_or("tmux didn't take the text")?;
            ok(run("tmux", &["send-keys", "-t", t, "Enter"]), "sent")
        }
        (HostKind::WezTerm | HostKind::Zellij | HostKind::Kitty, what) => {
            let key = |k: &str| -> Option<String> { type_key(host, k) };
            match what {
                Send::Approve => ok(key("enter"), "approved"),
                Send::Reject => ok(key("esc"), "rejected"),
                Send::Reply(text) => {
                    if dialog {
                        key("esc")
                            .ok_or(format!("{} couldn't dismiss the dialog", host.kind.name()))?;
                        std::thread::sleep(Duration::from_millis(400));
                    }
                    type_text(host, text)
                        .ok_or(format!("{} didn't take the text", host.kind.name()))?;
                    ok(key("enter"), "sent")
                }
            }
        }
        (HostKind::ITerm, Send::Approve) => ok(iterm_write(t, "enter", ""), "approved"),
        (HostKind::ITerm, Send::Reject) => ok(iterm_write(t, "esc", ""), "rejected"),
        (HostKind::ITerm, Send::Reply(text)) => {
            if dialog {
                iterm_write(t, "esc", "").ok_or("iTerm2 couldn't dismiss the dialog")?;
                std::thread::sleep(Duration::from_millis(400));
            }
            iterm_write(t, "text", text).ok_or("iTerm2 didn't take the text")?;
            ok(iterm_write(t, "enter", ""), "sent")
        }
        _ => Err(format!(
            "marina can't type into {} — only {TYPABLE}",
            host.describe()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_panes_from_the_env_and_nothing_else() {
        let env =
            |kvs: &[&str]| -> Vec<std::ffi::OsString> { kvs.iter().map(|s| s.into()).collect() };
        let t = TermEnv::from_environ(&env(&[
            "ZELLIJ_SESSION_NAME=quiet-otter",
            "ZELLIJ_PANE_ID=3",
            "KITTY_WINDOW_ID=7",
            "KITTY_LISTEN_ON=unix:/tmp/kitty-123",
            "KITTY_PUBLIC_KEY=1:secret",
            "PATH=/bin",
        ]))
        .unwrap();
        assert_eq!(t.zellij, Some(("quiet-otter".into(), "3".into())));
        assert_eq!(
            t.kitty,
            Some(("7".into(), Some("unix:/tmp/kitty-123".into())))
        );
        // no socket: known window, nothing to drive it with
        let t = TermEnv::from_environ(&env(&["KITTY_WINDOW_ID=2"])).unwrap();
        assert_eq!(t.kitty, Some(("2".into(), None)));
        // values that would be unsafe as argv are dropped
        assert_eq!(
            TermEnv::from_environ(&env(&["ZELLIJ_SESSION_NAME=a b; rm", "ZELLIJ_PANE_ID=1"])),
            None
        );
        assert_eq!(TermEnv::from_environ(&env(&["HOME=/x"])), None);
    }

    #[test]
    fn parses_herdr_agent_list() {
        let json = r#"{"id":"cli:agent:list","result":{"agents":[
          {"agent":"claude","agent_session":{"agent":"claude","kind":"id","value":"41616c1a-c2d3"},
           "agent_status":"working","pane_id":"w2S:p1",
           "terminal_title_stripped":"Marina process and shell visibility"},
          {"agent":"claude","agent_status":"blocked","pane_id":"w2R:p2","terminal_title_stripped":""},
          {"agent":"codex","agent_status":"unknown","pane_id":"w3:p1",
           "agent_session":{"value":"$(rm -rf)"}}
        ],"type":"agent_list"}}"#;
        let hs = parse_herdr_list(json);
        assert_eq!(hs.len(), 3);
        assert_eq!(hs[0].target, "w2S:p1");
        assert_eq!(hs[0].status, Some(HostStatus::Working));
        assert_eq!(
            hs[0].title.as_deref(),
            Some("Marina process and shell visibility")
        );
        assert_eq!(hs[0].session.as_deref(), Some("41616c1a-c2d3"));
        assert_eq!(
            (hs[1].status, hs[1].title.as_deref()),
            (Some(HostStatus::Blocked), None)
        );
        assert_eq!((hs[2].status, hs[2].session.as_deref()), (None, None));
        assert!(parse_herdr_list("not json").is_empty());
    }

    #[test]
    fn parses_herdr_pane_pid_and_tmux_panes() {
        let j = r#"{"result":{"process_info":{"foreground_process_group_id":44070,"pane_id":"w2S:p1","shell_pid":42743}}}"#;
        assert_eq!(parse_herdr_pid(j), Some(44070));
        assert_eq!(parse_herdr_pid("{}"), None);
        let t = parse_tmux_panes("/dev/ttys004\tmain:1.0\n/dev/ttys009\twork:2.1\n");
        assert_eq!(t[Path::new("/dev/ttys009")], "work:2.1");
    }

    #[test]
    fn terminal_apps_from_ancestry() {
        let k = HostKind::terminal_app;
        assert_eq!(
            k("/Applications/iTerm.app/Contents/MacOS/iTerm2"),
            Some(HostKind::ITerm)
        );
        assert_eq!(
            k("/Users/x/Library/Application Support/iTerm2/iTermServer-3.5.0"),
            Some(HostKind::ITerm)
        );
        assert_eq!(
            k("/System/Applications/Utilities/Terminal.app/Contents/MacOS/Terminal"),
            Some(HostKind::Terminal)
        );
        assert_eq!(
            k("/Applications/Ghostty.app/Contents/MacOS/ghostty"),
            Some(HostKind::Ghostty)
        );
        assert_eq!(k("/usr/bin/ghostty"), Some(HostKind::Ghostty));
        assert_eq!(k("wezterm-gui"), Some(HostKind::WezTerm));
        assert_eq!(k("/bin/zsh"), None);
        assert_eq!(k("login"), None);
    }

    #[test]
    fn lookup_walks_up_to_the_terminal_app() {
        let p = |pid: u32, ppid: u32, argv0: &str| ProcInfo {
            pid,
            ppid: Some(ppid),
            name: argv0.rsplit('/').next().unwrap().into(),
            argv: vec![argv0.into()],
            cwd: None,
            cpu_pct: 0.0,
            mem_bytes: 0,
            start_time: 0,
            agent: None,
            term: None,
        };
        let procs: HashMap<u32, ProcInfo> = [
            p(10, 1, "/Applications/Ghostty.app/Contents/MacOS/ghostty"),
            p(11, 10, "/usr/bin/login"),
            p(12, 11, "-zsh"),
            p(13, 12, "claude"),
            p(
                20,
                1,
                "/System/Applications/Utilities/Terminal.app/Contents/MacOS/Terminal",
            ),
            p(21, 20, "-zsh"),
            p(22, 21, "claude"),
            p(30, 1, "-zsh"),
            p(31, 30, "claude"),
        ]
        .into_iter()
        .map(|p| (p.pid, p))
        .collect();
        let mut h = Hosts::default();
        let tty = Path::new("/dev/ttys007");
        let dir = Path::new("/w/app");
        let g = h.lookup(&procs, 13, Some(tty), dir).unwrap();
        assert_eq!((g.kind, g.target.as_str()), (HostKind::Ghostty, "/w/app"));
        assert_eq!(g.describe(), "Ghostty terminal in /w/app");
        let t = h.lookup(&procs, 22, Some(tty), dir).unwrap();
        assert_eq!(
            (t.kind, t.target.as_str()),
            (HostKind::Terminal, "/dev/ttys007")
        );
        assert_eq!(
            h.lookup(&procs, 31, Some(tty), dir),
            None,
            "no known terminal"
        );
        // a multiplexer's outer terminal: up the session's own ancestry, else
        // where a client of it runs (a detached server has no terminal above)
        let mut more = procs.clone();
        for q in [
            p(40, 1, "/opt/homebrew/bin/tmux"), // the server
            p(41, 40, "-zsh"),
            p(42, 41, "claude"),
            p(43, 12, "/opt/homebrew/bin/tmux"), // a client, in Ghostty
        ] {
            more.insert(q.pid, q);
        }
        assert_eq!(outer_of(&more, 42, "tmux"), Some((HostKind::Ghostty, 10)));
        assert_eq!(outer_of(&more, 13, "herdr"), Some((HostKind::Ghostty, 10)));
        assert_eq!(outer_of(&procs, 31, "tmux"), None);
        assert_eq!(
            h.lookup(&procs, 22, None, dir),
            None,
            "no tty, nothing to match"
        );
    }

    #[test]
    fn recognises_dialogs_on_screen() {
        let screen = |t: &str| t.lines().map(String::from).collect::<Vec<_>>();
        assert!(shows_dialog(&screen(
            " Bash command\n   cargo publish\n Do you want to proceed?\n ❯ 1. Yes\n   2. No"
        )));
        assert!(shows_dialog(&screen(
            "Which one?\n❯ 1. Commit it\n  2. Pause"
        )));
        assert!(!shows_dialog(&screen("⏺ Done.\n❯ \n  auto mode on")));
        let trust = screen(
            " Quick safety check: Is this a project you trust?\n ❯ No, exit\n   Yes, I trust this folder\n Enter to confirm · Esc to cancel",
        );
        assert!(shows_dialog(&trust));
        assert_eq!(highlighted_choice(&trust).as_deref(), Some("No, exit"));
        assert!(approvable(&trust).is_err(), "Enter would pick No");
        let perm =
            screen(" Do you want to proceed?\n ❯ 1. Yes\n   2. Yes, and don't ask again\n   3. No");
        assert_eq!(highlighted_choice(&perm).as_deref(), Some("Yes"));
        assert!(approvable(&perm).is_ok());
        let prompt = screen("⏺ Done.\n──────\n❯ Try \"how does x work?\"\n──────\n  repo on main");
        assert_eq!(
            highlighted_choice(&prompt),
            None,
            "the input box isn't a choice"
        );
        let ghostty = HostInfo {
            kind: HostKind::Ghostty,
            target: "/x".into(),
            status: None,
            title: None,
            session: None,
            app_pid: None,
            outer: None,
        };
        assert!(
            send(&ghostty, &Send::Approve).is_err(),
            "never types into a GUI terminal"
        );
        assert!(read_screen(&ghostty, 10).is_none());
    }

    #[test]
    fn finds_the_wezterm_pane_by_tty() {
        let j = r#"[{"window_id":0,"tab_id":0,"pane_id":3,"tty_name":"/dev/ttys001"},
                    {"window_id":0,"tab_id":1,"pane_id":7,"tty_name":"/dev/ttys009"}]"#;
        assert_eq!(wezterm_pane(j, "/dev/ttys009"), Some(7));
        assert_eq!(wezterm_pane(j, "/dev/ttys002"), None);
        assert_eq!(wezterm_pane("nope", "/dev/ttys001"), None);
    }
}
