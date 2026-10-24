//! `marina demo`: the TUI on made-up sessions — to try marina with no agents
//! running, and to record the README's demo the same way every time.
//!
//! Nothing real is touched. Approve, reject, reply and end change the
//! made-up sessions; every other verb only says what it would do (see
//! `main`), and nothing is written to the state dir. The pids are above any
//! real pid, so even a verb that slipped through could reach no process.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use crate::git::GitStatus;
use crate::hooks::Report;
use crate::hosts::{HostInfo, HostKind, HostStatus};
use crate::launcher::{Launcher, LauncherKind};
use crate::model::{
    Agent, AgentChild, Anchor, Snapshot, Target, TargetKey, TargetKind, Url, UrlScheme,
};
use crate::msg::{SamplerCtl, SamplerMsg};
use crate::sources::TtyActivity;
use crate::transcript::{Ended, Summary};

/// Above macOS's 99999 and Linux's 2^22.
const PID: u32 = 2_000_000_000;

#[derive(Clone, Copy, PartialEq, Debug)]
enum St {
    Working,
    Blocked,
    /// Finished its turn.
    Idle,
}

/// One scripted change: `after` seconds into the previous state, become
/// `st`, saying `reply`, with `tool` pending or `shell` running.
#[derive(Clone)]
struct Step {
    after: u64,
    st: St,
    reply: Option<&'static str>,
    tool: Option<&'static str>,
    shell: Option<&'static str>,
    pr: Option<&'static str>,
    git: Option<GitStatus>,
}

fn step(after: u64, st: St) -> Step {
    Step {
        after,
        st,
        reply: None,
        tool: None,
        shell: None,
        pr: None,
        git: None,
    }
}

impl Step {
    fn says(mut self, r: &'static str) -> Self {
        self.reply = Some(r);
        self
    }
    fn tool(mut self, t: &'static str) -> Self {
        self.tool = Some(t);
        self
    }
    fn shell(mut self, s: &'static str) -> Self {
        self.shell = Some(s);
        self
    }
    fn pr(mut self, p: &'static str) -> Self {
        self.pr = Some(p);
        self
    }
    fn git(mut self, g: GitStatus) -> Self {
        self.git = Some(g);
        self
    }
}

struct Sess {
    agent: &'static str,
    project: &'static str,
    branch: &'static str,
    host: HostKind,
    title: &'static str,
    prompt: String,
    reply: String,
    tool: Option<&'static str>,
    shell: Option<&'static str>,
    pr: Option<&'static str>,
    git: GitStatus,
    st: St,
    /// When `st` began.
    at: u64,
    /// Seen already: parked rather than unread.
    read: bool,
    started: u64,
    input: u64,
    tokens: u64,
    /// What happens next, unprompted.
    next: Vec<Step>,
    on_yes: Vec<Step>,
    on_no: Vec<Step>,
    on_reply: Vec<Step>,
}

impl Sess {
    fn new(agent: &'static str, project: &'static str, title: &'static str, now: u64) -> Self {
        Sess {
            agent,
            project,
            branch: "main",
            host: HostKind::Tmux,
            title,
            prompt: String::new(),
            reply: String::new(),
            tool: None,
            shell: None,
            pr: None,
            git: GitStatus::default(),
            st: St::Idle,
            at: now,
            read: false,
            started: now - 3 * 3600,
            input: now - 60,
            tokens: 60_000,
            next: Vec::new(),
            on_yes: vec![
                step(0, St::Working),
                step(4, St::Idle).says("Done — that went through cleanly."),
            ],
            on_no: vec![step(0, St::Idle).says("Okay, I've left it. What should I do instead?")],
            on_reply: vec![
                step(0, St::Working),
                step(6, St::Idle).says("Done. Nothing else is pending."),
            ],
        }
    }

    fn apply(&mut self, s: &Step, now: u64) {
        self.st = s.st;
        self.at = now;
        self.tool = s.tool;
        self.shell = s.shell;
        if let Some(r) = s.reply {
            self.reply = r.into();
            self.read = false;
        }
        if s.pr.is_some() {
            self.pr = s.pr;
        }
        if let Some(g) = s.git {
            self.git = g;
        }
    }

    /// Run any scripted steps that are due.
    fn advance(&mut self, now: u64) {
        while let Some(s) = self.next.first().cloned() {
            if now < self.at + s.after {
                break;
            }
            self.next.remove(0);
            self.apply(&s, now);
        }
    }

    /// Play `steps`: the first at once, the rest on the clock.
    fn play(&mut self, steps: Vec<Step>, now: u64) {
        let mut steps = steps.into_iter();
        if let Some(first) = steps.next() {
            self.apply(&first, now);
        }
        self.next = steps.collect();
    }
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

fn scenario(now: u64) -> Vec<Sess> {
    let mut out = Vec::new();

    // waiting on an approval
    let mut s = Sess::new("claude", "checkout", "Saved cards at checkout", now);
    s.branch = "saved-cards";
    s.prompt = "Add saved cards to checkout, behind the payments flag".into();
    s.reply =
        "The schema change is ready. I need to run the migration against the dev database.".into();
    s.st = St::Blocked;
    s.tool = Some("Bash: pnpm prisma migrate dev --name saved_cards");
    s.at = now - 40;
    s.git = git(6, 2, 214, 31, 0);
    s.tokens = 92_000;
    s.on_yes = vec![
        step(0, St::Working).shell("pnpm prisma migrate dev"),
        step(5, St::Working).shell("pnpm test checkout"),
        step(6, St::Idle)
            .says("Migration applied and the checkout tests pass (48). Ready for review.")
            .git(git(9, 3, 301, 40, 0)),
    ];
    out.push(s);

    // waiting on a question
    let mut s = Sess::new("codex", "api", "Rate limiting for the public API", now);
    s.branch = "rate-limit";
    s.prompt = "Add rate limiting to the public endpoints".into();
    s.reply =
        "There are two ways to key the limit. Should it apply per user, or per API key?".into();
    s.st = St::Blocked;
    s.at = now - 3 * 60;
    s.git = git(3, 1, 88, 4, 0);
    s.on_reply = vec![
        step(0, St::Working),
        step(4, St::Working).shell("cargo test rate_limit"),
        step(5, St::Idle)
            .says("Done: the limit is per API key, 100 requests a minute, with tests.")
            .git(git(5, 1, 162, 9, 0)),
    ];
    out.push(s);

    // working: tests now, done soon
    let mut s = Sess::new("claude", "web", "Fix the flaky date tests", now);
    s.prompt = "The date tests fail around midnight UTC — find out why and fix it".into();
    s.st = St::Working;
    s.shell = Some("pnpm vitest run");
    s.git = git(2, 0, 19, 7, 0);
    s.next = vec![step(12, St::Idle).says(
        "Fixed: the tests built dates in local time. All 214 pass now. Want me to open a PR?",
    )];
    out.push(s);

    // working, then stops to ask for approval
    let mut s = Sess::new("opencode", "mobile", "Settings screen redesign", now);
    s.branch = "settings-v2";
    s.prompt = "Redesign the settings screen to match the new Figma".into();
    s.st = St::Working;
    s.git = git(4, 1, 140, 66, 0);
    s.next = vec![step(17, St::Blocked)
        .says("I'd like to replace the old settings screen entirely.")
        .tool("Edit: src/screens/Settings.tsx")];
    s.on_yes = vec![
        step(0, St::Working),
        step(6, St::Idle)
            .says("The new settings screen is in, with dark mode. Screenshots are in the PR.")
            .pr("https://github.com/acme/mobile/pull/88"),
    ];
    out.push(s);

    // waiting, in a terminal marina can't type into
    let mut s = Sess::new("claude", "infra", "Staging database upgrade", now);
    s.host = HostKind::Ghostty;
    s.prompt = "Upgrade staging Postgres to 17".into();
    s.reply = "The plan looks right: one database replaced, nothing destroyed.".into();
    s.st = St::Blocked;
    s.tool = Some("Bash: terraform apply -target=module.staging_db");
    s.at = now - 6 * 60;
    out.push(s);

    // finished, unread, with a PR
    let mut s = Sess::new("claude", "docs", "Quickstart rewrite", now);
    s.prompt = "Rewrite the quickstart for the new CLI".into();
    s.reply = "Rewrote the quickstart and fixed 12 broken links. The PR is open.".into();
    s.pr = Some("https://github.com/acme/docs/pull/212");
    s.at = now - 9 * 60;
    s.git = git(0, 0, 0, 0, 0);
    out.push(s);

    // finished and read: parked
    let mut s = Sess::new("claude", "landing", "Pricing page copy", now);
    s.prompt = "Tighten the pricing page copy".into();
    s.reply = "Shortened every plan description and committed it.".into();
    s.read = true;
    s.at = now - 2 * 3600;
    s.input = now - 2 * 3600;
    s.git = git(0, 0, 0, 0, 0);
    out.push(s);

    // forgotten for days
    let mut s = Sess::new("codex", "search", "Try a vector index", now);
    s.prompt = "Spike: would pgvector beat the current search?".into();
    s.reply = "pgvector was 3x faster on the sample set; notes are in SPIKE.md.".into();
    s.read = true;
    s.at = now - 3 * 86_400;
    s.started = now - 4 * 86_400;
    s.input = now - 3 * 86_400;
    s.git = git(0, 0, 0, 0, 0);
    out.push(s);

    out
}

fn dir(project: &str) -> PathBuf {
    PathBuf::from("/Users/you/dev").join(project)
}

fn agent_of(i: usize, s: &Sess, now: u64) -> Agent {
    let pid = PID + 100 * i as u32;
    let mut children = vec![AgentChild {
        pid: pid + 1,
        start_time: s.started,
        label: "npm exec chrome-devtools-mcp".into(),
        shell: false,
        cpu_pct: 0.0,
        mem_bytes: 60 << 20,
    }];
    if let Some(cmd) = s.shell {
        children.push(AgentChild {
            pid: pid + 2,
            start_time: s.at,
            label: cmd.into(),
            shell: true,
            cpu_pct: 85.0,
            mem_bytes: 300 << 20,
        });
    }
    let status = match s.st {
        St::Working => HostStatus::Working,
        St::Blocked => HostStatus::Blocked,
        St::Idle => HostStatus::Idle,
    };
    let session = format!("{:08x}-demo-4000-8000-{:012x}", 0x5e55_1000 + i, i);
    let working = s.st == St::Working;
    let output = if working { now } else { s.at };
    let host = HostInfo {
        kind: s.host,
        target: if s.host == HostKind::Tmux {
            format!("demo:{i}.0")
        } else {
            dir(s.project).display().to_string()
        },
        status: None,
        title: None,
        session: Some(session.clone()),
        app_pid: None,
        outer: (s.host == HostKind::Tmux).then_some(HostKind::Ghostty),
    };
    Agent {
        name: s.agent.into(),
        pid,
        start_time: s.started,
        session: Some(session.clone()),
        cwd: dir(s.project),
        project: s.project.into(),
        git_branch: Some(s.branch.into()),
        argv0: s.agent.into(),
        argc: 1,
        pids: children.iter().map(|c| c.pid).chain([pid]).collect(),
        cpu_pct: if working { 12.0 } else { 0.3 },
        mem_bytes: (420 + 37 * i as u64) << 20,
        children,
        tty_path: None,
        tty: Some(TtyActivity {
            output,
            input: s.input,
        }),
        host: Some(host),
        git: Some(s.git),
        title: None,
        summary: Some(Summary {
            title: Some(s.title.into()),
            last_prompt: Some(s.prompt.clone()),
            last_reply: (!s.reply.is_empty() && !working).then(|| s.reply.clone()),
            recap: None,
            pending_tool: s.tool.map(str::to_string),
            pending_since: s.tool.map(|_| s.at),
            in_turn: s.st != St::Idle,
            pr_url: s.pr.map(str::to_string),
            // a read session's reply is no news
            updated: (!s.read).then_some(s.at),
            context_tokens: Some(s.tokens),
            model: Some(
                if s.agent == "codex" {
                    "gpt-5.5"
                } else {
                    "claude-opus-5-5"
                }
                .into(),
            ),
            cwd: Some(dir(s.project)),
            turn_ended: (s.st == St::Idle).then_some(s.at),
        }),
        waiting: false,
        tree: Some(dir(s.project)),
        report: Some(Report {
            agent: s.agent.into(),
            session: Some(session),
            status,
            at: s.at,
            chain: vec![pid],
        }),
    }
}

fn target(
    project: &str,
    label: &str,
    port: u16,
    cpu: f32,
    mem_mb: u64,
    url: Option<(UrlScheme, String)>,
    by: Option<&Agent>,
) -> Target {
    let pid = PID + 50_000 + port as u32;
    Target {
        key: TargetKey::Port(port),
        kind: TargetKind::Listener,
        ports: vec![port],
        anchor: Anchor { pid, start_time: 0 },
        anchor_argv: vec![],
        pids: vec![pid],
        pid_starts: vec![(pid, 0)],
        project: project.into(),
        command_label: label.into(),
        cwd: dir(project),
        git_branch: by.and_then(|a| a.git_branch.clone()),
        cpu_pct: cpu,
        mem_bytes: mem_mb << 20,
        url: url.map(|(scheme, value)| Url { scheme, value }),
        exposed: false,
        container: None,
        launcher: by.map(Agent::as_launcher),
    }
}

fn snapshot(seq: u64, sessions: &[Sess], ended: &[Ended], now: u64) -> Snapshot {
    let agents: Vec<Agent> = sessions
        .iter()
        .enumerate()
        .map(|(i, s)| agent_of(i, s, now))
        .collect();
    let by = |p: &str| agents.iter().find(|a| a.project == p);
    let http = |p: u16| Some((UrlScheme::Http, format!("http://localhost:{p}")));
    let mut targets = vec![
        target(
            "checkout",
            "next dev",
            3000,
            1.2,
            380,
            http(3000),
            by("checkout"),
        ),
        target("api", "cargo run", 8080, 0.4, 42, http(8080), by("api")),
        target("web", "vite", 5173, 0.8, 210, http(5173), by("web")),
        // left running by the forgotten spike, and running away
        target(
            "search",
            "storybook",
            6006,
            164.0,
            2900,
            http(6006),
            by("search"),
        ),
        target(
            "checkout",
            "postgres",
            5432,
            0.1,
            64,
            Some((UrlScheme::Postgres, "postgres://localhost:5432".to_string())),
            None,
        ),
    ];
    // up for: checkout's next dev, api, web, the spike's storybook, the database
    let ups = [2 * 3600, 40 * 60, 5 * 3600, 3 * 86_400, 6 * 86_400];
    for (t, up) in targets.iter_mut().zip(ups) {
        t.anchor.start_time = now - up;
        t.pid_starts = vec![(t.anchor.pid, now - up)];
    }
    targets[4].container = Some("checkout-db-1".into());
    targets[4].launcher = Some(Launcher {
        kind: LauncherKind::Detached,
        name: "docker".into(),
        pid: None,
        alive: true,
        session: None,
        cwd: None,
        start_time: 0,
    });
    Snapshot {
        seq,
        targets,
        agents,
        ended: ended.to_vec(),
        error: None,
    }
}

fn ended(now: u64) -> Vec<Ended> {
    vec![Ended {
        id: "0e9c2b7a-demo-4000-8000-000000000001".into(),
        at: now - 5 * 3600,
        cwd: dir("billing"),
        summary: Summary {
            title: Some("Invoice PDF export".into()),
            last_prompt: Some("Export invoices as PDF".into()),
            last_reply: Some("PDF export works for single invoices; batches are next.".into()),
            updated: Some(now - 5 * 3600),
            ..Default::default()
        },
    }]
}

/// The pane text the preview shows, by pid.
fn screens() -> &'static Mutex<HashMap<u32, Vec<String>>> {
    static S: OnceLock<Mutex<HashMap<u32, Vec<String>>>> = OnceLock::new();
    S.get_or_init(Default::default)
}

/// The made-up session's screen, for the preview pane.
pub fn screen(pid: u32) -> Option<Vec<String>> {
    screens().lock().ok()?.get(&pid).cloned()
}

fn screen_of(s: &Sess) -> Vec<String> {
    let mut rows = vec![format!("> {}", s.prompt), String::new()];
    match (s.st, s.shell, s.tool) {
        (St::Working, Some(cmd), _) => {
            rows.push(format!("⏺ Bash({cmd})"));
            rows.push("  ⎿  Running…".into());
        }
        (St::Working, None, _) => rows.push("✻ Thinking…".into()),
        (St::Blocked, _, Some(tool)) => {
            rows.push(format!("⏺ {}", s.reply));
            rows.push(String::new());
            rows.push(format!(" {tool}"));
            rows.push(" Do you want to proceed?".into());
            rows.push(" ❯ 1. Yes".into());
            rows.push("   2. No, and tell the agent what to do differently".into());
        }
        _ => rows.push(format!("⏺ {}", s.reply)),
    }
    rows
}

/// Which session a host target names (`demo:3.0`, or the directory).
fn find(sessions: &[Sess], target: &str) -> Option<usize> {
    if let Some(i) = target
        .strip_prefix("demo:")
        .and_then(|t| t.split('.').next())
        .and_then(|i| i.parse().ok())
    {
        return Some(i);
    }
    sessions
        .iter()
        .position(|s| dir(s.project).display().to_string() == target)
}

/// Like [`crate::sampler::spawn`], from the made-up sessions. A
/// `SamplerCtl::Demo` acts on one of them.
pub fn spawn() -> (Receiver<SamplerMsg>, Sender<SamplerCtl>) {
    let (tx, rx) = mpsc::channel();
    let (ctl_tx, ctl_rx) = mpsc::channel::<SamplerCtl>();
    thread::spawn(move || {
        let start = crate::ui::unix_now();
        let mut sessions = scenario(start);
        let ended = ended(start);
        let mut gone = vec![false; sessions.len()];
        let mut seq = 0;
        loop {
            let now = crate::ui::unix_now();
            for s in sessions.iter_mut() {
                s.advance(now);
            }
            if let Ok(mut m) = screens().lock() {
                for (i, s) in sessions.iter().enumerate() {
                    m.insert(PID + 100 * i as u32, screen_of(s));
                }
            }
            seq += 1;
            let mut snap = snapshot(seq, &sessions, &ended, now);
            // ended sessions keep their index (and pid) for the rest
            let mut i = 0;
            snap.agents.retain(|_| {
                i += 1;
                !gone[i - 1]
            });
            if tx.send(SamplerMsg::Snapshot(Arc::new(snap))).is_err() {
                break;
            }
            match ctl_rx.recv_timeout(Duration::from_secs(1)) {
                Ok(SamplerCtl::Demo(target, what)) => {
                    let Some(i) = find(&sessions, &target) else {
                        continue;
                    };
                    let now = crate::ui::unix_now();
                    let s = &mut sessions[i];
                    match what {
                        crate::hosts::Send::Approve => {
                            let steps = std::mem::take(&mut s.on_yes);
                            s.play(steps, now);
                        }
                        crate::hosts::Send::Reject => {
                            let steps = std::mem::take(&mut s.on_no);
                            s.play(steps, now);
                        }
                        crate::hosts::Send::Reply(text) if text == "/exit" || text == "/quit" => {
                            gone[i] = true;
                        }
                        crate::hosts::Send::Reply(text) => {
                            s.prompt = text;
                            let steps = std::mem::take(&mut s.on_reply);
                            s.play(steps, now);
                        }
                    }
                }
                Ok(SamplerCtl::Refresh) | Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
    });
    (rx, ctl_tx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scripted_steps_run_on_the_clock() {
        let now = 1_000_000;
        let mut all = scenario(now);
        let web = all.iter_mut().find(|s| s.project == "web").unwrap();
        web.advance(now + 5);
        assert_eq!(web.st, St::Working);
        web.advance(now + 12);
        assert_eq!(web.st, St::Idle);
        assert!(web.reply.contains("214 pass"));
    }

    #[test]
    fn approving_plays_its_script() {
        let now = 1_000_000;
        let mut all = scenario(now);
        let s = &mut all[0];
        let steps = std::mem::take(&mut s.on_yes);
        s.play(steps, now);
        assert_eq!(
            (s.st, s.shell),
            (St::Working, Some("pnpm prisma migrate dev"))
        );
        s.advance(now + 5);
        s.advance(now + 11);
        assert_eq!(s.st, St::Idle);
    }

    #[test]
    fn the_inbox_has_one_of_everything() {
        let now = 1_000_000;
        let snap = snapshot(1, &scenario(now), &ended(now), now);
        let st: Vec<_> = snap.agents.iter().map(|a| a.host_status()).collect();
        assert!(st.contains(&Some(HostStatus::Blocked)));
        assert!(st.contains(&Some(HostStatus::Working)));
        assert!(snap.agents.iter().any(|a| a.is_stale(now)));
        assert!(snap.targets.iter().any(|t| t.is_heavy()));
        // no made-up pid is a real one
        assert!(snap
            .agents
            .iter()
            .flat_map(|a| &a.pids)
            .all(|&p| p > 4_194_304));
    }
}
