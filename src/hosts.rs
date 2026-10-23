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
//! scripted when jumping (AppleScript on macOS; `wezterm cli`), so sampling
//! never triggers an automation prompt.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::sources::ProcInfo;

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
}

impl HostInfo {
    /// `herdr pane w2S:p1`, `iTerm2 session on /dev/ttys004`, …
    pub fn describe(&self) -> String {
        let t = &self.target;
        match self.kind {
            HostKind::Herdr => format!("herdr pane {t}"),
            HostKind::Tmux => format!("tmux pane {t}"),
            HostKind::ITerm => format!("iTerm2 session on {t}"),
            HostKind::Terminal => format!("Terminal tab on {t}"),
            HostKind::Ghostty => format!("Ghostty terminal in {t}"),
            HostKind::WezTerm => format!("WezTerm pane on {t}"),
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
            return Some(h.clone());
        }
        let tty = tty?;
        let (kind, target) = match self.tmux.get(tty) {
            Some(pane) => (HostKind::Tmux, pane.clone()),
            None => match terminal_of(procs, pid)? {
                HostKind::Ghostty => (HostKind::Ghostty, cwd.display().to_string()),
                kind => (kind, tty.display().to_string()),
            },
        };
        Some(HostInfo {
            kind,
            target,
            status: None,
            title: None,
            session: None,
        })
    }
}

/// The terminal app `pid` runs in: the first ancestor that is one.
fn terminal_of(procs: &HashMap<u32, ProcInfo>, pid: u32) -> Option<HostKind> {
    let mut cur = procs.get(&pid)?.ppid;
    for _ in 0..64 {
        let p = procs.get(&cur?)?;
        if let Some(kind) = p.argv.first().and_then(|a| HostKind::terminal_app(a)) {
            return Some(kind);
        }
        cur = p.ppid;
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
/// reporting how many were there.
const GHOSTTY_FOCUS: &str = r#"on run argv
  tell application "Ghostty"
    set ts to every terminal whose working directory is (item 1 of argv)
    if (count of ts) is 0 then return "missing"
    focus (item 1 of ts)
    activate
    return (count of ts) as text
  end tell
end run"#;

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

/// Focus the session's pane. Returns a line for the status bar.
pub fn jump(host: &HostInfo) -> Result<String, String> {
    match host.kind {
        HostKind::Herdr => run("herdr", &["agent", "focus", &host.target])
            .map(|_| format!("focused herdr pane {}", host.target))
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
                Ok(format!("switched to tmux pane {}", host.target))
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
        HostKind::Ghostty => match osascript(GHOSTTY_FOCUS, &[&host.target]).as_deref() {
            Some("1") => Ok(format!("focused the {}", host.describe())),
            Some(n) if n.parse::<u32>().is_ok() => Ok(format!(
                "focused a Ghostty terminal in {} ({n} are open there)",
                host.target
            )),
            Some(_) => Err(format!("no {} any more", host.describe())),
            None => Err(
                "couldn't script Ghostty — allow marina's terminal under System Settings › Privacy › Automation"
                    .into(),
            ),
        },
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(
            h.lookup(&procs, 22, None, dir),
            None,
            "no tty, nothing to match"
        );
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
