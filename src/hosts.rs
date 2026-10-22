//! Terminal hosts: whatever owns an agent session's terminal — a multiplexer
//! (herdr, tmux) or nothing marina can drive. A host lets marina *jump* to a
//! session (focus its pane) and, for richer hosts, read what they know about
//! it: herdr reports the agent's status (including **blocked** on a question,
//! which marina can't see from outside) and the session's task title.
//!
//! Each host is an adapter keyed by something marina already has — the
//! agent's pid (herdr) or its tty (tmux) — so adding one (WezTerm, kitty,
//! iTerm) is another `scan` arm plus a `jump` arm. Without a host, jumping
//! falls back to copying a resume command.
//!
//! Hosts are queried only when their server process is running, through
//! their own CLIs, with a deadline, at most every few seconds.

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
}

impl HostKind {
    pub fn as_str(self) -> &'static str {
        match self {
            HostKind::Herdr => "herdr",
            HostKind::Tmux => "tmux",
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
    /// The host's id for the pane: herdr `w2S:p1`, tmux `main:1.0`.
    pub target: String,
    pub status: Option<HostStatus>,
    /// The session's task title (herdr: the terminal title the agent sets).
    pub title: Option<String>,
    /// The host's view of the session id — fresher than env markers, which
    /// older helpers keep from before a `/clear`.
    pub session: Option<String>,
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

    /// The host of the agent `pid` whose terminal is `tty`, if any.
    pub fn lookup(
        &mut self,
        procs: &HashMap<u32, ProcInfo>,
        pid: u32,
        tty: Option<&Path>,
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
        let target = self.tmux.get(tty?)?;
        Some(HostInfo {
            kind: HostKind::Tmux,
            target: target.clone(),
            status: None,
            title: None,
            session: None,
        })
    }
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
}
