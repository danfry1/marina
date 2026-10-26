//! `marina demo`: the TUI on a made-up machine — a few projects' dev
//! servers, a database container, the agent sessions that started them, one
//! server orphaned by a session that ended and one left on the LAN. Nothing
//! real is read or touched: verbs act on the made-up servers (a kill takes
//! the row away, a restart starts it over), logs and diffs are made up too,
//! and jumping or opening only says what it would do.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::git::GitStatus;
use crate::hosts::{HostInfo, HostKind, HostStatus};
use crate::launcher::{Launcher, LauncherKind};
use crate::model::{
    Agent, AgentChild, Anchor, Snapshot, Target, TargetKey, TargetKind, Url, UrlScheme,
};
use crate::msg::{DemoAct, SamplerCtl, SamplerMsg};

/// Made-up pids start above any real pid limit, so nothing can mistake one
/// for a live process.
const PID: u32 = 2_000_000_000;

const MB: u64 = 1024 * 1024;

/// A made-up agent session.
struct Sess {
    agent: &'static str,
    project: &'static str,
    session: &'static str,
    branch: &'static str,
    title: &'static str,
    status: HostStatus,
    /// A command it's running now.
    shell: Option<&'static str>,
    host: HostKind,
    /// Seconds since its terminal last had input (days: stale).
    quiet: u64,
    git: GitStatus,
}

/// A made-up server.
struct Srv {
    project: &'static str,
    label: &'static str,
    port: u16,
    cpu: f32,
    mem: u64,
    /// Seconds it has been up.
    up: u64,
    branch: Option<&'static str>,
    /// Index into the sessions that started it; `None`: a terminal.
    by: Option<usize>,
    /// Its session has ended: an orphan.
    orphan: bool,
    exposed: bool,
    container: Option<&'static str>,
    scheme: UrlScheme,
}

fn git(changed: u32, untracked: u32, ins: u32, del: u32, ahead: u32) -> GitStatus {
    GitStatus {
        changed,
        untracked,
        insertions: ins,
        deletions: del,
        ahead: Some(ahead),
        behind: Some(0),
    }
}

fn sessions() -> Vec<Sess> {
    vec![
        Sess {
            agent: "claude",
            project: "checkout",
            session: "5e551000-aaaa-4aaa-8aaa-000000000001",
            branch: "saved-cards",
            title: "Saved cards at checkout",
            status: HostStatus::Working,
            shell: Some("pnpm test checkout"),
            host: HostKind::Herdr,
            quiet: 60,
            git: git(9, 3, 301, 40, 0),
        },
        Sess {
            agent: "codex",
            project: "api",
            session: "5e551000-aaaa-4aaa-8aaa-000000000002",
            branch: "rate-limit",
            title: "Rate limiting for the public API",
            status: HostStatus::Idle,
            shell: None,
            host: HostKind::Tmux,
            quiet: 3 * 60,
            git: git(3, 1, 88, 4, 0),
        },
        Sess {
            agent: "claude",
            project: "web",
            session: "5e551000-aaaa-4aaa-8aaa-000000000003",
            branch: "main",
            title: "Fix the flaky date tests",
            status: HostStatus::Working,
            shell: Some("pnpm vitest run"),
            host: HostKind::Herdr,
            quiet: 20,
            git: git(2, 0, 19, 7, 0),
        },
        Sess {
            agent: "codex",
            project: "search",
            session: "5e551000-aaaa-4aaa-8aaa-000000000004",
            branch: "pgvector-spike",
            title: "Try a vector index",
            status: HostStatus::Idle,
            shell: None,
            host: HostKind::Ghostty,
            quiet: 3 * 86_400,
            git: git(0, 0, 0, 0, 0),
        },
    ]
}

fn servers() -> Vec<Srv> {
    let srv = |project, label, port, cpu, mem, up, by| Srv {
        project,
        label,
        port,
        cpu,
        mem,
        up,
        branch: None,
        by,
        orphan: false,
        exposed: false,
        container: None,
        scheme: UrlScheme::Http,
    };
    vec![
        Srv {
            branch: Some("saved-cards"),
            ..srv(
                "checkout",
                "next dev",
                3000,
                1.2,
                380 * MB,
                2 * 3600,
                Some(0),
            )
        },
        Srv {
            container: Some("checkout-db-1"),
            scheme: UrlScheme::Postgres,
            ..srv("checkout", "postgres", 5432, 0.1, 64 * MB, 6 * 86_400, None)
        },
        Srv {
            branch: Some("rate-limit"),
            exposed: true,
            ..srv("api", "cargo run", 8080, 0.4, 42 * MB, 40 * 60, Some(1))
        },
        srv("web", "vite", 5173, 0.8, 210 * MB, 5 * 3600, Some(2)),
        Srv {
            orphan: true,
            ..srv("docs", "astro dev", 4321, 0.2, 120 * MB, 86_400, None)
        },
        Srv {
            branch: Some("pgvector-spike"),
            ..srv(
                "search",
                "storybook",
                6006,
                164.0,
                2970 * MB,
                3 * 86_400,
                Some(3),
            )
        },
    ]
}

fn dir(project: &str) -> PathBuf {
    PathBuf::from("/Users/you/dev").join(project)
}

/// The made-up machine, and what has been done to it.
struct World {
    start: u64,
    sessions: Vec<Sess>,
    servers: Vec<Srv>,
    /// Restarted: up since then.
    restarted: Vec<(u16, u64)>,
    seq: u64,
}

impl World {
    fn new(now: u64) -> Self {
        World {
            start: now,
            sessions: sessions(),
            servers: servers(),
            restarted: Vec::new(),
            seq: 0,
        }
    }

    fn act(&mut self, act: DemoAct, now: u64) {
        let port = |k: &TargetKey| match k {
            TargetKey::Port(p) => Some(*p),
            TargetKey::Command { .. } => None,
        };
        match act {
            DemoAct::Stop(keys) => {
                let ports: Vec<u16> = keys.iter().filter_map(port).collect();
                self.servers.retain(|s| !ports.contains(&s.port));
            }
            DemoAct::Restart(keys) => {
                for p in keys.iter().filter_map(port) {
                    self.restarted.retain(|(q, _)| *q != p);
                    self.restarted.push((p, now));
                }
            }
        }
    }

    fn snapshot(&mut self, now: u64) -> Snapshot {
        self.seq += 1;
        // a little movement in the numbers, as a live machine has
        let wobble = |i: usize| ((self.seq as f32 + i as f32 * 1.7).sin() * 0.15) + 1.0;
        let agents: Vec<Agent> = self
            .sessions
            .iter()
            .enumerate()
            .map(|(i, s)| agent(i, s, now, wobble(i)))
            .collect();
        let targets = self
            .servers
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let since = self
                    .restarted
                    .iter()
                    .find(|(p, _)| *p == s.port)
                    .map(|&(_, at)| at)
                    .unwrap_or(self.start.saturating_sub(s.up));
                target(i, s, &self.sessions, &agents, since, wobble(i + 7))
            })
            .collect();
        Snapshot {
            seq: self.seq,
            targets,
            agents,
            error: None,
        }
    }
}

fn agent(i: usize, s: &Sess, now: u64, wobble: f32) -> Agent {
    let pid = PID + 100 * i as u32;
    let mut children = vec![AgentChild {
        pid: pid + 1,
        start_time: now - 3600,
        label: "npm exec chrome-devtools-mcp".into(),
        shell: false,
        cpu_pct: 0.1,
        mem_bytes: 48 * MB,
    }];
    if let Some(cmd) = s.shell {
        children.push(AgentChild {
            pid: pid + 2,
            start_time: now - 20,
            label: cmd.into(),
            shell: true,
            cpu_pct: 38.0 * wobble,
            mem_bytes: 160 * MB,
        });
    }
    let host = match s.host {
        HostKind::Herdr => Some((format!("w{i}:p1"), Some(HostKind::Ghostty))),
        HostKind::Tmux => Some((format!("work:{i}.0"), Some(HostKind::ITerm))),
        HostKind::Ghostty => Some((dir(s.project).display().to_string(), None)),
        _ => None,
    };
    Agent {
        name: s.agent.into(),
        pid,
        start_time: now - 4 * 3600 - s.quiet,
        session: Some(s.session.into()),
        cwd: dir(s.project),
        project: s.project.into(),
        git_branch: Some(s.branch.into()),
        argv0: s.agent.into(),
        argc: 1,
        pids: vec![pid, pid + 1],
        cpu_pct: if s.status == HostStatus::Working {
            6.0 * wobble
        } else {
            0.8
        },
        mem_bytes: 520 * MB,
        children,
        tty_path: None,
        tty: Some(crate::sources::TtyActivity {
            output: now
                - if s.status == HostStatus::Working {
                    1
                } else {
                    s.quiet
                },
            input: now - s.quiet,
        }),
        host: host.map(|(target, outer)| HostInfo {
            kind: s.host,
            target,
            // only herdr watches the screen and reports a status
            status: (s.host == HostKind::Herdr).then_some(s.status),
            title: (s.host == HostKind::Herdr).then(|| s.title.into()),
            session: None,
            app_pid: None,
            outer,
        }),
        git: Some(s.git),
        title: Some(s.title.into()),
    }
}

fn target(
    i: usize,
    s: &Srv,
    sessions: &[Sess],
    agents: &[Agent],
    since: u64,
    wobble: f32,
) -> Target {
    let pid = PID + 10_000 + 10 * i as u32;
    let launcher = match (s.by, s.orphan) {
        (Some(n), _) => Some(agents[n].as_launcher()),
        (None, true) => Some(Launcher {
            kind: LauncherKind::Agent,
            name: "claude".into(),
            pid: Some(PID + 9_999),
            alive: false,
            session: Some("5e551000-aaaa-4aaa-8aaa-0000000000e1".into()),
            cwd: Some(dir(s.project)),
            start_time: since,
        }),
        (None, false) if s.container.is_some() => None,
        (None, false) => Some(Launcher {
            kind: LauncherKind::Terminal,
            name: "ghostty".into(),
            pid: None,
            alive: true,
            session: None,
            cwd: None,
            start_time: since,
        }),
    };
    let branch = s
        .branch
        .map(str::to_string)
        .or_else(|| s.by.map(|n| sessions[n].branch.to_string()))
        .or_else(|| s.container.is_none().then(|| "main".into()));
    let url = match s.scheme {
        UrlScheme::Postgres => format!("postgres://localhost:{}", s.port),
        _ => format!("http://localhost:{}", s.port),
    };
    Target {
        key: TargetKey::Port(s.port),
        kind: TargetKind::Listener,
        ports: vec![s.port],
        anchor: Anchor {
            pid,
            start_time: since,
        },
        anchor_argv: vec![s.label.into()],
        pids: vec![pid, pid + 1, pid + 2],
        pid_starts: Vec::new(),
        project: s.project.into(),
        command_label: s.label.into(),
        cwd: dir(s.project),
        git_branch: branch,
        cpu_pct: s.cpu * wobble,
        mem_bytes: s.mem,
        url: Some(Url {
            scheme: s.scheme,
            value: url,
        }),
        exposed: s.exposed,
        container: s.container.map(str::to_string),
        launcher,
    }
}

/// Like [`crate::sampler::spawn`], from the made-up machine. A
/// [`SamplerCtl::Demo`] acts on it; a refresh rebuilds at once.
pub fn spawn() -> (Receiver<SamplerMsg>, Sender<SamplerCtl>) {
    let (tx, rx) = mpsc::channel();
    let (ctl_tx, ctl_rx) = mpsc::channel::<SamplerCtl>();
    thread::spawn(move || {
        let mut world = World::new(crate::ui::unix_now());
        loop {
            let snap = world.snapshot(crate::ui::unix_now());
            if tx.send(SamplerMsg::Snapshot(Arc::new(snap))).is_err() {
                break;
            }
            match ctl_rx.recv_timeout(Duration::from_secs(1)) {
                Ok(SamplerCtl::Demo(act)) => world.act(act, crate::ui::unix_now()),
                Ok(SamplerCtl::Refresh) | Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
    });
    (rx, ctl_tx)
}

/// What `T` shows for a made-up server: a dev server's start-up and a few
/// requests.
pub fn log_lines(project: &str, label: &str, port: Option<u16>) -> Vec<String> {
    let port = port.map(|p| p.to_string()).unwrap_or_default();
    match label {
        "postgres" => vec![
            "LOG:  database system is ready to accept connections".into(),
            "LOG:  checkpoint starting: time".into(),
            "LOG:  checkpoint complete: wrote 12 buffers (0.1%)".into(),
        ],
        "cargo run" => vec![
            "   Compiling api v0.3.0 (/Users/you/dev/api)".into(),
            "    Finished `dev` profile in 4.21s".into(),
            format!("     Running `target/debug/api` — listening on 0.0.0.0:{port}"),
            "GET /v1/search 200 12ms".into(),
            "GET /v1/search 429 1ms  rate limited (key …9f2c)".into(),
        ],
        "storybook" => vec![
            "info => Starting manager..".into(),
            format!("info => Storybook started on http://localhost:{port}"),
            "WARN Rebuilding: 418 stories changed".into(),
            "WARN Rebuilding: 418 stories changed".into(),
            "WARN Rebuilding: 418 stories changed".into(),
        ],
        _ => vec![
            format!("> {project}@0.1.0 dev"),
            format!("> {label}"),
            String::new(),
            format!("  ➜  Local:   http://localhost:{port}/"),
            "  ✓ Ready in 1.8s".into(),
            " GET / 200 in 142ms".into(),
            " GET /checkout 200 in 88ms".into(),
        ],
    }
}

/// What `D` shows for a made-up work tree.
pub fn diff_lines(project: &str) -> Vec<String> {
    vec![
        format!(" src/{project}/index.ts | 42 +++++++++++++++++++++----"),
        " src/lib/flags.ts        |  6 ++++-".into(),
        " 2 files changed, 41 insertions(+), 7 deletions(-)".into(),
        String::new(),
        format!("+++ b/src/{project}/index.ts"),
        "+  if (flags.payments) {".into(),
        "+    return <SavedCards user={user} />".into(),
        "+  }".into(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_demo_has_one_of_everything_a_cockpit_shows() {
        let now = 1_000_000;
        let mut w = World::new(now);
        let snap = w.snapshot(now);
        let orphan = snap
            .targets
            .iter()
            .filter(|t| t.launcher.as_ref().is_some_and(|l| l.is_orphaned()))
            .count();
        assert_eq!(orphan, 1);
        assert!(snap.targets.iter().any(|t| t.exposed));
        assert!(snap.targets.iter().any(|t| t.container.is_some()));
        assert!(snap.agents.iter().any(|a| a.is_stale(now)));
        assert!(snap.agents.iter().any(|a| a.is_working(now)));
        // no made-up pid is a real one, and none can be signalled
        assert!(snap.targets.iter().all(|t| t.pid_starts.is_empty()));
        assert!(snap.agents.iter().all(|a| a.pid >= PID));
    }

    #[test]
    fn stopping_and_restarting_act_on_the_made_up_servers() {
        let now = 1_000_000;
        let mut w = World::new(now);
        w.act(DemoAct::Stop(vec![TargetKey::Port(4321)]), now);
        let snap = w.snapshot(now);
        assert!(snap.targets.iter().all(|t| t.ports != [4321]));
        w.act(DemoAct::Restart(vec![TargetKey::Port(3000)]), now + 50);
        let snap = w.snapshot(now + 50);
        let t = snap.targets.iter().find(|t| t.ports == [3000]).unwrap();
        assert_eq!(t.anchor.start_time, now + 50);
    }
}
