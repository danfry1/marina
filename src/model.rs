//! The immutable data model. See DESIGN.md "Type contract" and CONTEXT.md.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::hosts::HostStatus;

/// Stable identity used to follow the cursor selection across snapshots.
/// Port-primary for listeners (with the anchor-fingerprint reuse guard);
/// command-based for port-less watched targets.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum TargetKey {
    Port(u16),
    Command {
        project: String,
        label: String,
        cwd: PathBuf,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TargetKind {
    Listener,
    Watched,
}

/// Representative process. `(pid, start_time)` is the reuse-guard fingerprint.
#[derive(Clone)]
pub struct Anchor {
    pub pid: u32,
    pub start_time: u64,
}

/// One row in the cockpit: a developer workload acted on by verbs.
#[derive(Clone)]
pub struct Target {
    pub key: TargetKey,
    pub kind: TargetKind,
    pub ports: Vec<u16>, // empty for Watched
    pub anchor: Anchor,
    pub anchor_argv: Vec<String>, // captured for `restart`
    pub pids: Vec<u32>,           // in-boundary subtree
    /// `(pid, start_time)` for every subtree pid — the fingerprint verbs check
    /// before signalling, so a recycled pid number is never hit.
    pub pid_starts: Vec<(u32, u64)>,
    pub project: String,
    pub command_label: String,
    pub cwd: PathBuf,
    pub git_branch: Option<String>,
    pub cpu_pct: f32,   // EWMA-smoothed, subtree rollup
    pub mem_bytes: u64, // subtree rollup
    pub url: Option<Url>,
    /// Listening on a non-loopback address (0.0.0.0 / ::) — reachable from the LAN.
    pub exposed: bool,
    /// Docker container name when this target is a published container port.
    /// Verbs act via `docker stop/restart/logs` instead of signals.
    pub container: Option<String>,
    /// Who started it — an agent session, editor, terminal, or nobody now
    /// (detached). `None` when the owner can't be named.
    pub launcher: Option<crate::launcher::Launcher>,
}

/// A running coding-agent session (`claude`, `opencode`, `codex`, …). Not a
/// Target: verbs, selectors and `ls` never see it, so no kill — group, project
/// selector or `--orphaned` — can take an agent down with its servers.
#[derive(Clone)]
pub struct Agent {
    pub name: String,
    pub pid: u32,
    pub start_time: u64,
    /// Resumable session id, from its children's env markers or its own argv.
    pub session: Option<String>,
    pub cwd: PathBuf,
    pub project: String,
    pub git_branch: Option<String>,
    /// Program path + arg count only, like targets (argv can hold secrets).
    pub argv0: String,
    pub argc: usize,
    /// Agent + descendants, minus servers (their targets own them) and nested
    /// agent sessions (their own rows).
    pub pids: Vec<u32>,
    pub cpu_pct: f32,   // EWMA-smoothed, rollup over `pids`
    pub mem_bytes: u64, // rollup over `pids`
    /// Direct children: tool shells and helpers (MCP servers, …).
    pub children: Vec<AgentChild>,
    /// Its terminal device, and that terminal's last output / input.
    pub tty_path: Option<PathBuf>,
    pub tty: Option<crate::sources::TtyActivity>,
    /// The multiplexer pane it runs in (herdr, tmux), if marina can drive it.
    pub host: Option<crate::hosts::HostInfo>,
    /// Its working tree: uncommitted / untracked / unpushed work.
    pub git: Option<crate::git::GitStatus>,
    /// The session's task title from the agent itself (opencode), when no
    /// host reports one.
    pub title: Option<String>,
    /// What the session last said, from its transcript (Claude Code).
    pub summary: Option<crate::transcript::Summary>,
    /// Inferred, for sessions no host reports on: a turn is under way (its
    /// transcript ends in your prompt, or a tool call or result, not the
    /// record a turn ends with), yet nothing runs — no shell, a still screen.
    /// A thinking agent animates its screen and a running tool has a child
    /// or a spinner; this is an approval prompt or a question to the human.
    pub waiting: bool,
    /// The git work tree it's in: the nearest directory up from `cwd` with
    /// a `.git` (a linked worktree has its own). Sessions sharing one can
    /// overwrite each other's edits.
    pub tree: Option<PathBuf>,
    /// What the agent last reported of itself, through marina's hooks.
    pub report: Option<crate::hooks::Report>,
}

/// A target this busy (smoothed, whole subtree) or this big is called heavy
/// — a runaway test runner, a leak — and flagged in the servers view.
pub const HEAVY_CPU: f32 = 90.0;
pub const HEAVY_MEM: u64 = 2 << 30;

impl Target {
    pub fn is_heavy(&self) -> bool {
        !self.pids.is_empty() && (self.cpu_pct >= HEAVY_CPU || self.mem_bytes >= HEAVY_MEM)
    }
}

/// Smoothed CPU% at or above which an agent with no shell and no terminal
/// counts as working.
pub const AGENT_BUSY_CPU: f32 = 4.0;
/// Terminal output this recent means the agent is working.
pub const WORKING_OUTPUT_SECS: u64 = 5;
/// An idle agent whose terminal has had no input this long is called stale.
pub const STALE_AFTER_SECS: u64 = 24 * 3600;

/// One direct child of an agent, labelled like a target (never raw argv).
#[derive(Clone)]
pub struct AgentChild {
    pub pid: u32,
    pub start_time: u64,
    /// `cargo test`, `vite`, `node mcp-server.js`; `shell` for an idle one.
    pub label: String,
    /// A shell the agent is running a command in (vs a long-lived helper).
    pub shell: bool,
    pub cpu_pct: f32,   // subtree, unsmoothed
    pub mem_bytes: u64, // subtree
}

/// The git work tree containing `dir`: the nearest ancestor (or itself)
/// holding a `.git` directory or file — except a repo at `$HOME` (dotfiles),
/// which would make every session under it look like it shares one.
pub fn work_tree(dir: &std::path::Path) -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    dir.ancestors()
        .find(|d| d.join(".git").exists())
        .filter(|d| Some(d.to_path_buf()) != home)
        .map(PathBuf::from)
}

/// Helpers that several sessions each run their own copy of — every Claude
/// session starting `chrome-devtools-mcp`, say: `(label, copies, total mem)`,
/// biggest first. What it would save to share them, or turn them off.
pub fn repeated_helpers<'a>(
    agents: impl IntoIterator<Item = &'a Agent>,
) -> Vec<(String, usize, u64)> {
    let mut by_label: HashMap<&str, (usize, u64)> = HashMap::new();
    for c in agents
        .into_iter()
        .flat_map(|a| &a.children)
        .filter(|c| !c.shell)
    {
        let e = by_label.entry(c.label.as_str()).or_default();
        e.0 += 1;
        e.1 += c.mem_bytes;
    }
    let mut out: Vec<(String, usize, u64)> = by_label
        .into_iter()
        .filter(|&(_, (n, _))| n > 1)
        .map(|(l, (n, m))| (l.to_string(), n, m))
        .collect();
    out.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)));
    out
}

impl Agent {
    /// The agent as the launcher its servers would name — what session
    /// grouping and the `via` styling share.
    pub fn as_launcher(&self) -> crate::launcher::Launcher {
        crate::launcher::Launcher {
            kind: crate::launcher::LauncherKind::Agent,
            name: self.name.clone(),
            pid: Some(self.pid),
            alive: true,
            session: self.session.clone(),
            cwd: Some(self.cwd.clone()),
            start_time: self.start_time,
        }
    }

    /// How much it wants you, for the inbox: 0 waiting on you, 1 finished
    /// (herdr's *done*), 2 working, 3 idle, 4 stale.
    pub fn urgency(&self, now: u64) -> u8 {
        if self.is_blocked() {
            0
        } else if self.host_status() == Some(HostStatus::Done) {
            1
        } else if self.is_working(now) {
            2
        } else if self.is_stale(now) {
            4
        } else {
            3
        }
    }

    /// Unix seconds it last did anything: when its transcript last
    /// recorded something, else its terminal's last output. (Terminal
    /// output alone misleads: a multiplexer redrawing every pane touches
    /// them all at once.) While working, now-ish either way.
    pub fn last_active(&self) -> u64 {
        let tty = self.tty.map(|t| t.output).unwrap_or(0);
        match self.summary.as_ref().and_then(|s| s.updated) {
            Some(said) if self.shells().next().is_none() => said,
            Some(said) => said.max(tty),
            None => tty,
        }
    }

    /// When it last settled — its last turn ended — for a stable inbox
    /// order: unchanged while it works. Falls back to `last_active`.
    pub fn settled_at(&self) -> u64 {
        self.summary
            .as_ref()
            .and_then(|s| s.turn_ended)
            .unwrap_or_else(|| self.last_active())
    }

    /// The session's task title: the host's (herdr), else the agent's own,
    /// else its transcript's.
    pub fn title(&self) -> Option<&str> {
        self.host
            .as_ref()
            .and_then(|h| h.title.as_deref())
            .or(self.title.as_deref())
            .or(self.summary.as_ref().and_then(|s| s.title.as_deref()))
    }

    pub fn shells(&self) -> impl Iterator<Item = &AgentChild> {
        self.children.iter().filter(|c| c.shell)
    }

    /// Doing something right now: a shell is running a command, or its
    /// terminal is being written to (a working agent animates its spinner and
    /// streams; an idle one leaves the screen alone). Without a terminal, CPU
    /// is the fallback: an idle agent TUI sits around 1%.
    pub fn is_working(&self, now: u64) -> bool {
        if self.shells().next().is_some() {
            return true;
        }
        // A host that watches the screen knows better than timestamps.
        if let Some(st) = self.host_status() {
            return st == HostStatus::Working;
        }
        match self.tty {
            Some(t) => now.saturating_sub(t.output) <= WORKING_OUTPUT_SECS,
            None => self.cpu_pct >= AGENT_BUSY_CPU,
        }
    }

    /// When its terminal last had input (keystrokes, or focus events from
    /// switching to it), else when it started. Transcript mtimes are no substitute: idle sessions' files
    /// get touched too.
    pub fn last_input(&self) -> u64 {
        self.tty.map(|t| t.input).unwrap_or(self.start_time)
    }

    /// What a host (herdr) says it's doing, else what it reported itself.
    pub fn host_status(&self) -> Option<HostStatus> {
        self.host
            .as_ref()
            .and_then(|h| h.status)
            .or_else(|| self.reported())
    }

    /// Its own last report (hooks), unless overtaken: its transcript
    /// recorded something since (you typed, or interrupted it). A running
    /// shell after an approval prompt means you approved.
    pub fn reported(&self) -> Option<HostStatus> {
        let r = self.report.as_ref()?;
        let newer = self.summary.as_ref().and_then(|s| s.updated);
        if newer.is_some_and(|u| u > r.at + crate::hooks::REPORT_SLACK) {
            return None;
        }
        match r.status {
            HostStatus::Blocked if self.shells().next().is_some() => Some(HostStatus::Working),
            st => Some(st),
        }
    }

    /// Waiting on a human: an approval prompt or a question (host-reported).
    pub fn is_blocked(&self) -> bool {
        match self.host_status() {
            Some(st) => st == HostStatus::Blocked,
            None => self.waiting,
        }
    }

    /// The pending tool call, with paths inside its directory made relative:
    /// `Write: hello.txt` rather than the whole absolute path.
    pub fn pending_tool(&self) -> Option<String> {
        let t = self.summary.as_ref()?.pending_tool.as_deref()?;
        let dir = format!("{}/", self.cwd.display());
        Some(t.replace(&dir, ""))
    }

    /// Set `waiting` (see there) as of `now`.
    pub fn infer_waiting(&mut self, now: u64) {
        let Some(s) = self
            .summary
            .as_ref()
            .filter(|s| s.pending_tool.is_some() || s.in_turn)
        else {
            self.waiting = false;
            return;
        };
        let idle = self.host_status().is_none() && self.shells().next().is_none();
        let still = self
            .tty
            .is_some_and(|t| now.saturating_sub(t.output) > WORKING_OUTPUT_SECS);
        // An approved Bash call starts its shell at once, so one pending for
        // a few seconds with no shell is at its prompt — whose screen
        // Claude Code keeps redrawing, so "still" can't tell.
        let bash_unstarted = s
            .pending_tool
            .as_deref()
            .is_some_and(|t| t.starts_with("Bash:"))
            && s.pending_since
                .is_some_and(|at| now.saturating_sub(at) > WORKING_OUTPUT_SECS);
        self.waiting = idle && self.tty.is_some() && (still || bash_unstarted);
    }

    /// Not working, and its terminal has had no input for over a day —
    /// likely a forgotten session holding memory.
    pub fn is_stale(&self, now: u64) -> bool {
        !self.is_working(now)
            && !self.is_blocked()
            && self.last_input() != 0
            && now.saturating_sub(self.last_input()) >= STALE_AFTER_SECS
    }

    /// Stale, none of its servers still running, and nothing uncommitted or
    /// unpushed: it looks finished. (Closing never loses files, and the
    /// session can be resumed.)
    pub fn looks_finished(&self, now: u64, servers: usize) -> bool {
        self.is_stale(now) && servers == 0 && self.git.is_none_or(|g| g.is_clean())
    }

    /// What it's doing, for a column: `$ cargo test`, `$ vite +1`, `working`,
    /// `idle 12m` (since its terminal last changed).
    pub fn activity(&self, now: u64) -> String {
        let mut shells = self.shells();
        match (shells.next(), shells.count()) {
            (Some(c), 0) => format!("$ {}", c.label),
            (Some(c), more) => format!("$ {} +{more}", c.label),
            (None, _) if self.is_working(now) => "working".into(),
            (None, _) if self.is_blocked() => "needs you".into(),
            (None, _) if self.host_status() == Some(HostStatus::Done) => "done".into(),
            (None, _) => match self.tty {
                Some(t) => format!("idle {}", crate::ui::fmt_age(now.saturating_sub(t.output))),
                None => "idle".into(),
            },
        }
    }

    /// Fixture: a claude session in `/Users/me/dev/<dir>` running one shell
    /// (`cargo test`) next to an MCP helper.
    #[cfg(test)]
    pub fn sample(pid: u32, session: &str, dir: &str) -> Self {
        let child = |pid: u32, label: &str, shell: bool| AgentChild {
            pid,
            start_time: 0,
            label: label.into(),
            shell,
            cpu_pct: 0.0,
            mem_bytes: 0,
        };
        Agent {
            name: "claude".into(),
            pid,
            start_time: 0,
            session: Some(session.into()),
            cwd: PathBuf::from("/Users/me/dev").join(dir),
            project: dir.into(),
            git_branch: Some("main".into()),
            argv0: "claude".into(),
            argc: 0,
            pids: vec![pid, pid + 1, pid + 2],
            cpu_pct: 1.5,
            mem_bytes: 500 * 1024 * 1024,
            children: vec![
                child(pid + 1, "cargo test", true),
                child(pid + 2, "npm exec chrome-devtools-mcp", false),
            ],
            tty_path: None,
            tty: None,
            host: None,
            git: None,
            title: None,
            summary: None,
            waiting: false,
            tree: None,
            report: None,
        }
    }
}

/// Typed, optional URL — the verb layer decides what "open" / "copy" mean per scheme.
#[derive(Clone)]
pub struct Url {
    pub scheme: UrlScheme,
    pub value: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum UrlScheme {
    Http,
    Https,
    Postgres,
    Redis,
    Mysql,
    Other,
}

impl UrlScheme {
    /// "Open in browser" only applies to web schemes.
    pub fn is_web(self) -> bool {
        matches!(self, UrlScheme::Http | UrlScheme::Https)
    }
}

/// Immutable, produced by the sampler. Held in a canonical order (by port);
/// the UI applies presentation order (sort + hysteresis) — ordering is NOT
/// baked in here. That separation is what makes "no jump under the cursor" work.
pub struct Snapshot {
    pub seq: u64,
    pub targets: Vec<Target>,
    /// Live coding-agent sessions, kept apart from `targets` (see [`Agent`]).
    pub agents: Vec<Agent>,
    /// Claude Code sessions that exited recently and can be resumed.
    pub ended: Vec<crate::transcript::Ended>,
    /// A data-source failure (e.g. the port scan errored). Shown persistently
    /// in the UI — an empty cockpit must never silently mean "enumeration broke".
    pub error: Option<String>,
}

impl Snapshot {
    pub fn empty() -> Self {
        Snapshot {
            seq: 0,
            targets: Vec::new(),
            agents: Vec::new(),
            ended: Vec::new(),
            error: None,
        }
    }

    /// Fixture data for tests (UI rendering, CLI selectors). Deliberately
    /// multi-ecosystem, not just Node, with one multi-target project.
    #[cfg(test)]
    pub fn sample() -> Self {
        let t = |project: &str,
                 label: &str,
                 port: Option<u16>,
                 cpu: f32,
                 mem_mb: u64,
                 url: Option<Url>,
                 kind: TargetKind| Target {
            key: match port {
                Some(p) => TargetKey::Port(p),
                None => TargetKey::Command {
                    project: project.into(),
                    label: label.into(),
                    cwd: PathBuf::from("/tmp"),
                },
            },
            kind,
            ports: port.into_iter().collect(),
            anchor: Anchor {
                pid: 0,
                start_time: 0,
            },
            anchor_argv: vec![],
            pids: vec![],
            pid_starts: vec![],
            project: project.into(),
            command_label: label.into(),
            cwd: PathBuf::from("/Users/dev").join(project),
            git_branch: Some("main".into()),
            cpu_pct: cpu,
            mem_bytes: mem_mb * 1024 * 1024,
            url,
            exposed: false,
            container: None,
            launcher: None,
        };
        let http = |p: u16| {
            Some(Url {
                scheme: UrlScheme::Http,
                value: format!("http://localhost:{p}"),
            })
        };
        let mut targets = vec![
            t(
                "client-portal",
                "next dev",
                Some(3000),
                3.4,
                340,
                http(3000),
                TargetKind::Listener,
            ),
            t(
                "billing-api",
                "uvicorn",
                Some(8000),
                1.1,
                96,
                http(8000),
                TargetKind::Listener,
            ),
            t(
                "worker",
                "celery",
                Some(5555),
                0.4,
                72,
                http(5555),
                TargetKind::Listener,
            ),
            t(
                "client-portal",
                "postgres",
                Some(5432),
                0.2,
                410,
                Some(Url {
                    scheme: UrlScheme::Postgres,
                    value: "postgres://localhost:5432".into(),
                }),
                TargetKind::Listener,
            ),
            t(
                "design-system",
                "tsc --watch",
                None,
                0.8,
                188,
                None,
                TargetKind::Watched,
            ),
        ];
        targets[1].exposed = true; // billing-api on 0.0.0.0 — exercises the LAN badge
        Snapshot {
            seq: 0,
            targets,
            agents: Vec::new(),
            ended: Vec::new(),
            error: None,
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_pending_tool_with_nothing_running_is_waiting() {
        use crate::sources::TtyActivity;
        let mut a = Agent::sample(1, "aaaa", "x");
        a.children.retain(|c| !c.shell); // no shell running
        a.summary = Some(crate::transcript::Summary {
            pending_tool: Some("Bash: cargo publish".into()),
            ..Default::default()
        });
        a.tty = Some(TtyActivity {
            output: 100,
            input: 100,
        });
        a.infer_waiting(103);
        assert!(!a.waiting, "the screen moved 3s ago: still running");
        // a Bash prompt keeps redrawing, but its command never started
        a.summary.as_mut().unwrap().pending_since = Some(90);
        a.tty = Some(TtyActivity {
            output: 119,
            input: 100,
        });
        a.infer_waiting(120);
        assert!(a.waiting, "Bash pending 30s with no shell");
        a.summary.as_mut().unwrap().pending_since = None;
        a.tty = Some(TtyActivity {
            output: 100,
            input: 100,
        });
        a.infer_waiting(120);
        assert!(a.waiting && a.is_blocked() && a.urgency(120) == 0);
        a.summary.as_mut().unwrap().pending_tool = None;
        a.infer_waiting(120);
        assert!(!a.waiting, "nothing pending");
        a.summary.as_mut().unwrap().pending_tool = Some("Bash: sleep 60".into());
        a.children = Agent::sample(1, "aaaa", "x").children; // its shell runs
        a.infer_waiting(120);
        assert!(!a.waiting, "a running command isn't a question");
    }

    #[test]
    fn work_trees_stop_at_the_nearest_git() {
        let base = std::env::temp_dir().join(format!("marina-tree-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("repo/.git")).unwrap();
        std::fs::create_dir_all(base.join("repo/src/deep")).unwrap();
        std::fs::create_dir_all(base.join("repo/wt")).unwrap();
        std::fs::write(base.join("repo/wt/.git"), "gitdir: x").unwrap(); // a linked worktree
        assert_eq!(
            work_tree(&base.join("repo/src/deep")),
            Some(base.join("repo"))
        );
        assert_eq!(work_tree(&base.join("repo/wt")), Some(base.join("repo/wt")));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_runaway_target_is_heavy() {
        let mut t = Snapshot::sample().targets.remove(0);
        t.pids = vec![t.anchor.pid];
        t.cpu_pct = 12.0;
        t.mem_bytes = 300 << 20;
        assert!(!t.is_heavy());
        t.cpu_pct = 180.0;
        assert!(t.is_heavy(), "a vitest pinning two cores");
        t.cpu_pct = 1.0;
        t.mem_bytes = 4 << 30;
        assert!(t.is_heavy(), "a leak");
        t.pids.clear();
        assert!(!t.is_heavy(), "a container: nothing measured");
    }

    #[test]
    fn its_own_report_wins_until_something_newer_happens() {
        let mut a = Agent::sample(100, "s", "app");
        a.children.clear();
        a.tty = Some(crate::sources::TtyActivity {
            output: 119,
            input: 0,
        });
        let report = |status| crate::hooks::Report {
            agent: "claude".into(),
            session: Some("s".into()),
            status,
            at: 100,
            chain: vec![100],
        };
        a.report = Some(report(HostStatus::Blocked));
        assert!(a.is_blocked() && a.urgency(120) == 0, "it said so");
        // the screen redraws, but it reported the turn over
        a.report = Some(report(HostStatus::Idle));
        assert!(!a.is_working(120) && !a.is_blocked());
        // a prompt typed since: the transcript is newer, so infer again
        a.summary = Some(crate::transcript::Summary {
            updated: Some(110),
            ..Default::default()
        });
        assert_eq!(a.reported(), None);
        assert!(a.is_working(120), "its screen is moving");
        // approved: a shell runs, so it isn't waiting any more
        a.summary = None;
        a.report = Some(report(HostStatus::Blocked));
        a.children = Agent::sample(100, "s", "app").children;
        assert!(!a.is_blocked() && a.is_working(120));
    }

    #[test]
    fn repeated_helpers_total_copies_across_sessions() {
        let mut a = Agent::sample(1, "aaaa", "x");
        let mut b = Agent::sample(2, "bbbb", "y");
        for (ag, mem) in [(&mut a, 100), (&mut b, 300)] {
            for c in ag.children.iter_mut() {
                c.mem_bytes = mem;
            }
        }
        let helper = a.children.iter().find(|c| !c.shell).unwrap().label.clone();
        let r = repeated_helpers([&a, &b]);
        assert_eq!(r, vec![(helper, 2, 400)], "shells aren't helpers");
        assert!(repeated_helpers([&a]).is_empty(), "one copy isn't repeated");
    }

    use super::*;

    #[test]
    fn url_scheme_web_detection() {
        assert!(UrlScheme::Http.is_web());
        assert!(UrlScheme::Https.is_web());
        assert!(!UrlScheme::Postgres.is_web());
        assert!(!UrlScheme::Redis.is_web());
    }

    #[test]
    fn agent_activity_working_idle_and_stale() {
        use crate::sources::TtyActivity;
        let now = 10 * STALE_AFTER_SECS;
        let mut a = Agent::sample(1, "s", "app");
        a.start_time = now - 5 * STALE_AFTER_SECS;
        a.tty = Some(TtyActivity {
            output: now - 600,
            input: now - 2 * STALE_AFTER_SECS,
        });
        assert!(a.is_working(now) && !a.is_stale(now)); // a shell is running
        assert_eq!(a.activity(now), "$ cargo test");
        a.children[1].shell = true;
        assert_eq!(a.activity(now), "$ cargo test +1");

        a.children.clear();
        assert_eq!(a.activity(now), "idle 10m");
        assert!(a.is_stale(now), "nobody typed in two days");
        a.tty = Some(TtyActivity {
            output: now - 1, // spinner / streaming
            input: now - 2 * STALE_AFTER_SECS,
        });
        assert_eq!(a.activity(now), "working");
        assert!(!a.is_stale(now));
        // a busy CPU doesn't count when the terminal is quiet
        a.tty = Some(TtyActivity {
            output: now - 60,
            input: now - 30,
        });
        a.cpu_pct = AGENT_BUSY_CPU + 10.0;
        assert!(!a.is_working(now) && !a.is_stale(now), "typed 30s ago");

        // no terminal: CPU decides, open time stands in for input
        a.tty = None;
        assert_eq!(a.activity(now), "working");
        a.cpu_pct = 0.3;
        assert_eq!(a.activity(now), "idle");
        assert!(a.is_stale(now), "open five days, no terminal to ask");
        a.start_time = 0;
        assert!(!a.is_stale(now), "unknown age is never stale");
    }

    #[test]
    fn sample_has_a_multi_target_project() {
        let snap = Snapshot::sample();
        let portal = snap
            .targets
            .iter()
            .filter(|t| t.project == "client-portal")
            .count();
        assert_eq!(portal, 2); // next dev + postgres — exercises grouping
    }
}
