//! opencode sessions. opencode keeps its session id inside its background
//! service, not in the process's argv or environment, so marina asks the
//! service — `opencode session list` in the session's directory — and matches
//! sessions to running opencode processes by recency: a session last updated
//! after a process started belongs to it, newest session to newest process.
//!
//! Only asked while the background service (`opencode serve --service`) is
//! already running, so marina never starts it; at most every few seconds per
//! directory, with a deadline.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::model::Agent;
use crate::sources::ProcInfo;

const TTL: Duration = Duration::from_secs(10);
const TIMEOUT: Duration = Duration::from_millis(1500);

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct Session {
    pub id: String,
    #[serde(default)]
    pub title: Option<String>,
    /// Unix milliseconds.
    #[serde(default)]
    pub updated: u64,
}

/// `opencode session list --format json`: newest first.
fn parse(json: &str) -> Vec<Session> {
    serde_json::from_str::<Vec<Session>>(json)
        .unwrap_or_default()
        .into_iter()
        .filter(|s| crate::launcher::is_session_id(&s.id))
        .collect()
}

fn list(dir: &Path) -> Option<Vec<Session>> {
    let mut cmd = Command::new("opencode");
    cmd.current_dir(dir)
        .args(["session", "list", "--format", "json", "-n", "20"]);
    crate::exec::output(cmd, TIMEOUT).map(|j| parse(&j))
}

/// Pair processes with sessions: both newest first, a session only going to
/// a process that started before it was last updated.
fn assign(agents: &mut [&mut Agent], sessions: &[Session]) {
    agents.sort_by_key(|a| std::cmp::Reverse(a.start_time));
    let mut next = 0;
    for a in agents.iter_mut() {
        // the newest unclaimed session, if it was touched since `a` started
        let Some(s) = sessions
            .get(next)
            .filter(|s| s.updated / 1000 >= a.start_time)
        else {
            continue;
        };
        next += 1;
        a.session = Some(s.id.clone());
        if a.title.is_none() {
            a.title = s.title.clone().filter(|t| !t.trim().is_empty());
        }
    }
}

/// Session lists per directory, cached across sampler ticks.
#[derive(Default)]
pub struct Sessions {
    cache: HashMap<PathBuf, (Instant, Vec<Session>)>,
}

impl Sessions {
    /// Fill in session ids (and titles) for opencode agents that have none.
    pub fn fill(&mut self, procs: &HashMap<u32, ProcInfo>, agents: &mut [Agent]) {
        let mut by_dir: HashMap<PathBuf, Vec<&mut Agent>> = HashMap::new();
        for a in agents.iter_mut() {
            if a.name == "opencode" && a.session.is_none() {
                by_dir.entry(a.cwd.clone()).or_default().push(a);
            }
        }
        if by_dir.is_empty() || !service_running(procs) {
            self.cache.clear();
            return;
        }
        self.cache.retain(|d, _| by_dir.contains_key(d));
        for (dir, mut group) in by_dir {
            let fresh = self
                .cache
                .get(&dir)
                .is_some_and(|(at, _)| at.elapsed() < TTL);
            if !fresh {
                let sessions = list(&dir).unwrap_or_default();
                self.cache.insert(dir.clone(), (Instant::now(), sessions));
            }
            assign(&mut group, &self.cache[&dir].1);
        }
    }
}

fn service_running(procs: &HashMap<u32, ProcInfo>) -> bool {
    procs.values().any(|p| {
        crate::launcher::agent_service(&p.argv) == Some("opencode")
            && p.argv.iter().any(|a| a == "--service")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_session_list() {
        let j = r#"[{"id":"ses_e97f0d992ffeb9yYMVgmZLlv6x","title":"Test run",
            "updated":1792747187517,"created":1792747185795,"directory":"/x"},
            {"id":"$(rm -rf)","updated":1}]"#;
        let s = parse(j);
        assert_eq!(s.len(), 1, "malformed ids are dropped");
        assert_eq!(s[0].title.as_deref(), Some("Test run"));
        assert_eq!(s[0].updated, 1792747187517);
        assert!(parse("not json").is_empty());
    }

    #[test]
    fn newest_session_goes_to_newest_process_that_predates_it() {
        let ses = |id: &str, updated_secs: u64| Session {
            id: id.into(),
            title: Some(format!("t-{id}")),
            updated: updated_secs * 1000,
        };
        let mut old = Agent::sample(1, "x", "p");
        let mut new = Agent::sample(2, "x", "p");
        let mut newest = Agent::sample(3, "x", "p");
        for (a, start) in [(&mut old, 100), (&mut new, 200), (&mut newest, 300)] {
            a.start_time = start;
            a.session = None;
            a.title = None;
        }
        // newest process (started 300) has no session yet: nothing updated since
        let sessions = [ses("b", 250), ses("a", 150)];
        let mut group = vec![&mut old, &mut newest, &mut new];
        assign(&mut group, &sessions);
        assert_eq!(newest.session, None);
        assert_eq!(new.session.as_deref(), Some("b"));
        assert_eq!(new.title.as_deref(), Some("t-b"));
        assert_eq!(old.session.as_deref(), Some("a"));
    }
}
