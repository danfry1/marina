//! Agent control: the preview pane (`p`), replying / approving / rejecting
//! from marina (`r` / `y` / `n`), and the inbox view (`a`).
//!
//! marina only ever types into a session on an explicit keystroke on that
//! session's row, and only through a multiplexer that can address the pane
//! (herdr, tmux) — never by synthesising keystrokes into a GUI terminal.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use ratatui::layout::Rect;

use super::format::unix_now;
use super::state::{App, Entry};
use crate::hosts::{self, HostInfo, Send};
use crate::model::Agent;

/// The inbox keeps its order this long after a keypress, so rows don't
/// move under the cursor while you browse.
pub(crate) const INBOX_HOLD: Duration = Duration::from_secs(8);

/// How often an open preview re-reads the session's screen.
pub(crate) const SCREEN_EVERY: Duration = Duration::from_millis(1500);
/// Screen rows fetched for the preview.
pub(crate) const SCREEN_ROWS: usize = 60;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PreviewMode {
    /// From the transcript: last prompt, reply, recap, pending tool.
    Reply,
    /// The session's live screen, via its multiplexer.
    Screen,
}

/// A fetched screen for one agent.
pub(crate) struct Screen {
    pub pid: u32,
    pub at: Instant,
    pub rows: Vec<String>,
}

#[derive(Default)]
pub(crate) struct Control {
    pub preview: Option<PreviewMode>,
    pub screen: Option<Screen>,
    /// A screen read is in flight (guards double fetches).
    pub screen_pending: bool,
    /// Bumped whenever marina sends to a session: a screen read requested
    /// before then is out of date when it lands, and is dropped.
    pub screen_gen: u64,
    /// Reply being typed: (agent pid, text).
    pub reply: Option<(u32, String)>,
    /// The inbox view replaces the project/session table.
    pub inbox: bool,
    /// Inbox list: first visible row, and (screen row, agent pid) per
    /// visible card row, for mouse hits.
    pub list_top: usize,
    pub hits: Vec<(u16, Entry)>,
    pub list_rect: Rect,
    /// Detail pane: where it was drawn, its scroll, and whose it was.
    pub detail_rect: Rect,
    pub detail_scroll: usize,
    pub detail_pid: Option<u32>,
    /// What you've read: session id -> transcript time last seen.
    pub seen: Seen,
    /// Canned replies on keys 1-9 (`[agents] quick`).
    pub quick: Vec<String>,
    /// A new session being set up (`N`).
    pub new_session: Option<NewDraft>,
    /// `X` pressed once on this agent: press again to end it.
    pub pending_end: Option<(u32, Instant)>,
}

/// `N`: where and with what to start a session.
pub(crate) struct NewDraft {
    pub dir: PathBuf,
    pub agent: String,
    pub prompt: String,
    pub worktree: bool,
}

/// The newest transcript time you've seen per session, so a session that
/// said something since reads as unread. Kept in marina's state dir across
/// runs; with no file yet, everything current counts as read.
#[derive(Default)]
pub(crate) struct Seen {
    path: Option<PathBuf>,
    map: HashMap<String, u64>,
    /// No file existed: mark what's there now as read on first sight.
    seed: bool,
    dirty: bool,
}

impl Seen {
    pub fn load(path: PathBuf) -> Seen {
        let map: Option<HashMap<String, u64>> = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok());
        Seen {
            // missing or unreadable: start with everything read, not unread
            seed: map.is_none(),
            map: map.unwrap_or_default(),
            path: Some(path),
            dirty: false,
        }
    }

    fn mark(&mut self, id: &str, at: u64) {
        let e = self.map.entry(id.to_string()).or_insert(0);
        if at > *e {
            *e = at;
            self.dirty = true;
        }
    }

    fn get(&self, id: &str) -> Option<u64> {
        self.map.get(id).copied()
    }

    /// Write if changed, keeping only `live` sessions.
    fn save(&mut self, live: &[&str]) {
        if !self.dirty {
            return;
        }
        self.dirty = false;
        self.map.retain(|k, _| live.contains(&k.as_str()));
        if let Some(p) = &self.path {
            if let Some(dir) = p.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            // write then rename: a reader never sees half a file
            if let Ok(json) = serde_json::to_string(&self.map) {
                let tmp = p.with_extension(format!("json.{}", std::process::id()));
                if std::fs::write(&tmp, json).is_ok() {
                    let _ = std::fs::rename(&tmp, p);
                }
            }
        }
    }
}

/// What the main loop should do after a control key.
pub enum Action {
    None,
    /// Send this to the agent's host on a background thread.
    Send(HostInfo, Send),
}

impl App {
    // --- preview ------------------------------------------------------------

    /// `p`: open the preview (the transcript view when there is one, else
    /// the screen), switch between the two, close from the second.
    pub fn toggle_preview(&mut self) {
        let a = self.selected_agent();
        if self.ctl.inbox {
            // the inbox always shows the detail: p flips conversation/screen
            let screen = a.is_some_and(|a| a.host.as_ref().is_some_and(|h| h.kind.interactive()));
            self.ctl.preview = match self.ctl.preview {
                Some(PreviewMode::Screen) => None,
                _ if screen => Some(PreviewMode::Screen),
                _ => {
                    self.set_status("marina can show the screen of herdr and tmux panes only");
                    None
                }
            };
            self.ctl.detail_scroll = 0;
            self.dirty = true;
            return;
        }
        let has_reply = a.is_some_and(|a| a.summary.is_some());
        let has_screen = a.is_some_and(|a| a.host.as_ref().is_some_and(|h| h.kind.interactive()));
        self.drop_screen();
        self.ctl.preview = match self.ctl.preview {
            None if has_reply || !has_screen => Some(PreviewMode::Reply),
            None => Some(PreviewMode::Screen),
            Some(PreviewMode::Reply) if has_screen => Some(PreviewMode::Screen),
            Some(_) => None,
        };
        self.dirty = true;
    }

    /// The selected agent's host, when the open preview wants a fresh screen
    /// of it and none is being read. The caller reads it off-thread and hands
    /// the rows to `screen_ready`.
    pub fn screen_due(&mut self) -> Option<(u32, u64, HostInfo)> {
        if self.ctl.preview.is_none() || self.ctl.screen_pending {
            return None;
        }
        let a = self.selected_agent()?;
        let host = a.host.clone().filter(|h| h.kind.interactive())?;
        let fresh = self
            .ctl
            .screen
            .as_ref()
            .is_some_and(|s| s.pid == a.pid && s.at.elapsed() < SCREEN_EVERY);
        if fresh {
            return None;
        }
        let pid = a.pid;
        self.ctl.screen_pending = true;
        Some((pid, self.ctl.screen_gen, host))
    }

    pub fn screen_ready(&mut self, pid: u32, generation: u64, rows: Option<Vec<String>>) {
        self.ctl.screen_pending = false;
        if generation != self.ctl.screen_gen {
            return; // read before marina last sent something: stale
        }
        self.ctl.screen = Some(Screen {
            pid,
            at: Instant::now(),
            rows: rows.unwrap_or_default(),
        });
        self.dirty = true;
    }

    /// The fetched screen of agent `pid`, if it's the current one.
    pub(crate) fn screen_of(&self, pid: u32) -> Option<&[String]> {
        self.ctl
            .screen
            .as_ref()
            .filter(|s| s.pid == pid)
            .map(|s| s.rows.as_slice())
    }

    /// Forget the fetched screen (after sending, or when it's no longer
    /// being watched), so nothing acts on an old one.
    pub(crate) fn drop_screen(&mut self) {
        self.ctl.screen = None;
        self.ctl.screen_gen += 1;
    }

    /// Is `a` at an approval / question dialog right now? Its host says so
    /// (herdr), or its screen shows one.
    /// A host that reports status is trusted; otherwise a screen read in
    /// the last couple of seconds showing a dialog also counts. (Sending
    /// re-reads the screen anyway; this only decides what's offered.)
    pub(crate) fn waiting_on_you(&self, a: &Agent) -> bool {
        if a.host.as_ref().is_some_and(|h| h.status.is_some()) {
            return a.is_blocked();
        }
        let fresh_dialog = self.ctl.screen.as_ref().is_some_and(|s| {
            s.pid == a.pid && s.at.elapsed() < SCREEN_EVERY * 2 && hosts::shows_dialog(&s.rows)
        });
        a.is_blocked() || fresh_dialog
    }

    /// Scroll the detail pane (`[` / `]`, PgUp / PgDn, wheel). Positive is
    /// further down the conversation, or further back up the screen.
    pub fn detail_scroll_by(&mut self, delta: isize) {
        self.ctl.detail_scroll = self.ctl.detail_scroll.saturating_add_signed(delta);
        self.dirty = true;
    }

    /// Is a detail pane showing (inbox, or the preview)?
    pub fn detail_visible(&self) -> bool {
        self.ctl.inbox || self.ctl.preview.is_some()
    }

    /// Start a new selection's detail at the top.
    pub(crate) fn sync_detail(&mut self) {
        let pid = self.selected_agent().map(|a| a.pid);
        if pid != self.ctl.detail_pid {
            self.ctl.detail_pid = pid;
            self.ctl.detail_scroll = 0;
            self.drop_screen();
        }
    }

    /// Mouse: the inbox card at (`x`, `y`), if any.
    pub(crate) fn card_at(&self, x: u16, y: u16) -> Option<Entry> {
        let r = self.ctl.list_rect;
        if x < r.x || x >= r.x + r.width {
            return None;
        }
        self.ctl
            .hits
            .iter()
            .find(|(row, _)| *row == y)
            .map(|(_, e)| e.clone())
    }

    /// Other live sessions in `a`'s git work tree.
    pub(crate) fn tree_mates(&self, a: &Agent) -> Vec<&Agent> {
        let Some(t) = a.tree.as_deref() else {
            return Vec::new();
        };
        self.snapshot
            .agents
            .iter()
            .filter(|o| o.pid != a.pid && o.tree.as_deref() == Some(t))
            .collect()
    }

    // --- unread -----------------------------------------------------------------

    /// Has `a` said something since you last looked? (Not while it's still
    /// working: the reply isn't finished.)
    pub(crate) fn is_unread(&self, a: &Agent, now: u64) -> bool {
        let (Some(id), Some(at)) = (
            a.session.as_deref(),
            a.summary.as_ref().and_then(|s| s.updated),
        ) else {
            return false;
        };
        !a.is_working(now) && self.ctl.seen.get(id).is_none_or(|seen| at > seen)
    }

    /// Inbox order: 0 needs you, 1 new (unread, or herdr's *done*), 3 the
    /// rest, 4 stale. Working isn't its own group — a session doesn't jump
    /// when it starts or stops a step; its dot says it's working.
    pub(crate) fn inbox_urgency(&self, a: &Agent, now: u64) -> u8 {
        match a.urgency(now) {
            0 => 0,
            1 => 1,
            _ if self.is_unread(a, now) => 1,
            4 => 4,
            _ => 3,
        }
    }

    /// After each snapshot: what you're looking at now is read (the
    /// selected session in the inbox or preview); seed on a first run;
    /// persist changes.
    pub(crate) fn update_seen(&mut self) {
        let mut marks: Vec<(String, u64)> = Vec::new();
        let seed = std::mem::take(&mut self.ctl.seen.seed);
        // only while you're using marina: an unattended inbox reads nothing
        let in_use = self.last_input.elapsed() < Duration::from_secs(60);
        let viewing = (in_use && (self.ctl.inbox || self.ctl.preview.is_some()))
            .then(|| self.selected_agent().map(|a| a.pid))
            .flatten();
        for a in &self.snapshot.agents {
            let (Some(id), Some(at)) = (
                a.session.as_deref(),
                a.summary.as_ref().and_then(|s| s.updated),
            ) else {
                continue;
            };
            if seed || viewing == Some(a.pid) {
                marks.push((id.to_string(), at));
            }
        }
        for (id, at) in marks {
            self.ctl.seen.mark(&id, at);
        }
        let live: Vec<&str> = self
            .snapshot
            .agents
            .iter()
            .filter_map(|a| a.session.as_deref())
            .collect();
        self.ctl.seen.save(&live);
    }

    /// Jumping to a session reads it too.
    pub fn mark_read(&mut self, pid: u32) {
        let hit = self
            .snapshot
            .agents
            .iter()
            .find(|a| a.pid == pid)
            .and_then(|a| Some((a.session.clone()?, a.summary.as_ref()?.updated?)));
        if let Some((id, at)) = hit {
            self.ctl.seen.mark(&id, at);
        }
    }

    // --- approve / reject / reply ------------------------------------------

    /// `y` / `n` on the selected session.
    pub fn answer(&mut self, approve: bool) -> Action {
        let Some(a) = self.selected_agent() else {
            return Action::None;
        };
        let Some(host) = a.host.clone().filter(|h| h.kind.interactive()) else {
            self.set_status(cant_type(a));
            return Action::None;
        };
        if !self.waiting_on_you(a) {
            let hint = if a.host.as_ref().is_some_and(|h| h.status.is_some()) {
                ""
            } else {
                " (p shows its screen)"
            };
            self.set_status(format!(
                "{} isn't waiting on a question{hint}",
                self.agent_label(a)
            ));
            return Action::None;
        }
        let what = if approve { Send::Approve } else { Send::Reject };
        self.drop_screen(); // re-read after it reacts
        Action::Send(host, what)
    }

    /// `r`: start typing a reply to the selected session.
    pub fn start_reply(&mut self) {
        let Some(a) = self.selected_agent() else {
            return;
        };
        if !a.host.as_ref().is_some_and(|h| h.kind.interactive()) {
            let msg = cant_type(a);
            self.set_status(msg);
            return;
        }
        self.ctl.reply = Some((a.pid, String::new()));
        self.dirty = true;
    }

    /// `1`-`9`: open the reply box with that canned reply, to send with
    /// Enter (or edit first).
    pub fn quick_reply(&mut self, n: usize) {
        let Some(text) = self.ctl.quick.get(n.wrapping_sub(1)).cloned() else {
            return;
        };
        self.start_reply();
        if let Some((_, t)) = self.ctl.reply.as_mut() {
            *t = text;
        }
    }

    pub fn set_quick_replies(&mut self, quick: Vec<String>) {
        self.ctl.quick = quick.into_iter().take(9).collect();
    }

    /// `1 commit it · 2 push it …` for hints.
    pub(crate) fn quick_hint(&self) -> String {
        self.ctl
            .quick
            .iter()
            .enumerate()
            .map(|(i, q)| format!("{} {q}", i + 1))
            .collect::<Vec<_>>()
            .join(" · ")
    }

    pub fn is_replying(&self) -> bool {
        self.ctl.reply.is_some()
    }

    pub fn reply_push(&mut self, c: char) {
        if let Some((_, t)) = self.ctl.reply.as_mut() {
            t.push(c);
            self.dirty = true;
        }
    }

    pub fn reply_backspace(&mut self) {
        if let Some((_, t)) = self.ctl.reply.as_mut() {
            t.pop();
            self.dirty = true;
        }
    }

    pub fn reply_cancel(&mut self) {
        self.ctl.reply = None;
        self.set_status("reply discarded");
    }

    /// Enter in the reply box: send it to the session it was started on.
    pub fn reply_commit(&mut self) -> Action {
        let Some((pid, text)) = self.ctl.reply.take() else {
            return Action::None;
        };
        self.dirty = true;
        let text = text.trim().to_string();
        if text.is_empty() {
            return Action::None;
        }
        let Some(a) = self.snapshot.agents.iter().find(|a| a.pid == pid) else {
            self.set_status("that session has ended — reply not sent");
            return Action::None;
        };
        let Some(host) = a.host.clone().filter(|h| h.kind.interactive()) else {
            self.set_status(cant_type(a));
            return Action::None;
        };
        self.drop_screen();
        Action::Send(host, Send::Reply(text))
    }

    /// The reply being typed and who it's for.
    pub(crate) fn reply_draft(&self) -> Option<(String, &str)> {
        let (pid, text) = self.ctl.reply.as_ref()?;
        let who = match self.snapshot.agents.iter().find(|a| a.pid == *pid) {
            Some(a) => self.agent_label(a),
            None => format!("pid {pid}"),
        };
        Some((who, text.as_str()))
    }

    /// The session a reply is being typed to ended: close the box (it would
    /// otherwise keep capturing keys) and say so.
    pub(crate) fn drop_orphan_reply(&mut self) {
        let gone = self
            .ctl
            .reply
            .as_ref()
            .is_some_and(|(pid, _)| !self.snapshot.agents.iter().any(|a| a.pid == *pid));
        if gone {
            self.ctl.reply = None;
            self.set_status("that session ended — reply discarded");
        }
    }

    pub(crate) fn agent_label(&self, a: &Agent) -> String {
        self.agent_session_of
            .get(&a.pid)
            .cloned()
            .unwrap_or_else(|| a.name.clone())
    }

    // --- start / end sessions ------------------------------------------------

    /// Any key but `X` cancels a half-confirmed end.
    pub fn clear_pending_end(&mut self) {
        self.ctl.pending_end = None;
    }

    pub fn is_ended_selected(&self) -> bool {
        self.selected_ended().is_some()
    }

    /// The ended session under the cursor (inbox).
    pub(crate) fn selected_ended(&self) -> Option<&crate::transcript::Ended> {
        match &self.selected {
            Some(Entry::Ended(id)) => self.snapshot.ended.iter().find(|e| &e.id == id),
            _ => None,
        }
    }

    /// Enter on an ended session: resume it where you work.
    pub fn resume_selected(&mut self) -> Option<(crate::launch::Launch, crate::launch::Place)> {
        let e = self.selected_ended()?;
        let req = crate::launch::Launch {
            dir: e.cwd.clone(),
            agent: "claude".into(),
            prompt: None,
            resume: Some(e.id.clone()),
            worktree: false,
        };
        let Some(place) = crate::launch::here() else {
            self.set_status(format!(
                "resume it with: cd {} && claude --resume {}",
                req.dir.display(),
                e.id
            ));
            return None;
        };
        self.set_status(format!("resuming in {}…", place.name()));
        Some((req, place))
    }

    /// `N`: start a session in the selected row's directory (an agent's, a
    /// server's, a group's), with an optional first prompt.
    pub fn start_new(&mut self) {
        let agent = self.selected_agent();
        let dir = agent
            .map(|a| a.cwd.clone())
            .or_else(|| self.selected_ended().map(|e| e.cwd.clone()))
            .or_else(|| self.selected_target().map(|t| t.cwd.clone()))
            .or_else(|| match &self.selected {
                Some(Entry::Group(p)) => self.group_members(p).first().map(|t| t.cwd.clone()),
                _ => None,
            })
            .or_else(|| std::env::current_dir().ok());
        let Some(dir) = dir else {
            return;
        };
        let name = agent
            .map(|a| a.name.clone())
            .filter(|n| matches!(n.as_str(), "claude" | "opencode" | "codex"))
            .unwrap_or_else(|| "claude".into());
        self.ctl.new_session = Some(NewDraft {
            dir,
            agent: name,
            prompt: String::new(),
            worktree: false,
        });
        self.dirty = true;
    }

    pub fn is_new_session(&self) -> bool {
        self.ctl.new_session.is_some()
    }

    pub fn new_push(&mut self, c: char) {
        if let Some(d) = self.ctl.new_session.as_mut() {
            d.prompt.push(c);
            self.dirty = true;
        }
    }

    pub fn new_backspace(&mut self) {
        if let Some(d) = self.ctl.new_session.as_mut() {
            d.prompt.pop();
            self.dirty = true;
        }
    }

    pub fn new_toggle_worktree(&mut self) {
        if let Some(d) = self.ctl.new_session.as_mut() {
            d.worktree = !d.worktree;
            self.dirty = true;
        }
    }

    pub fn new_cancel(&mut self) {
        self.ctl.new_session = None;
        self.dirty = true;
    }

    /// Enter: what to launch, and where — or why it can't.
    pub fn new_commit(&mut self) -> Option<(crate::launch::Launch, crate::launch::Place)> {
        let d = self.ctl.new_session.take()?;
        self.dirty = true;
        let Some(place) = crate::launch::here() else {
            self.set_status(
                "marina can open sessions from herdr, tmux, Ghostty, iTerm2 or Terminal — run it in one",
            );
            return None;
        };
        let prompt = Some(d.prompt.trim().to_string()).filter(|p| !p.is_empty());
        self.set_status(format!("starting {} in {}…", d.agent, place.name()));
        Some((
            crate::launch::Launch {
                dir: d.dir,
                agent: d.agent,
                prompt,
                resume: None,
                worktree: d.worktree,
            },
            place,
        ))
    }

    /// The draft, for the footer: (agent, dir, prompt, worktree, where).
    pub(crate) fn new_draft(&self) -> Option<&NewDraft> {
        self.ctl.new_session.as_ref()
    }

    /// `X`: end the selected session by sending its exit command — on a
    /// second press, after saying what it would leave behind.
    pub fn end_session(&mut self) -> Action {
        let Some(a) = self.selected_agent() else {
            return Action::None;
        };
        let Some(host) = a.host.clone().filter(|h| h.kind.interactive()) else {
            self.set_status(cant_type(a));
            return Action::None;
        };
        let pid = a.pid;
        let label = self.agent_label(a);
        // /exit while it works would be queued (or typed into its prompt),
        // and at a dialog the Esc first would reject the pending call
        if a.is_working(unix_now()) || self.waiting_on_you(a) {
            self.set_status(format!(
                "{label} is busy — let it finish (or answer it) before ending it, or Enter to jump there"
            ));
            self.ctl.pending_end = None;
            return Action::None;
        }
        let confirmed = self
            .ctl
            .pending_end
            .is_some_and(|(p, at)| p == pid && at.elapsed() < Duration::from_secs(4));
        if !confirmed {
            let mut warn = Vec::new();
            if let Some(g) = a.git.filter(|g| !g.is_clean()) {
                warn.push(g.describe());
            }
            let servers = self.agent_servers(a).len();
            if servers > 0 {
                warn.push(format!(
                    "{servers} server{} it started keep running",
                    if servers == 1 { "" } else { "s" }
                ));
            }
            let warn = if warn.is_empty() {
                String::new()
            } else {
                format!(" — {}", warn.join(" · "))
            };
            self.ctl.pending_end = Some((pid, Instant::now()));
            self.set_status(format!(
                "press X again to end {label}{warn} (resumable later)"
            ));
            return Action::None;
        }
        let exit = if a.name == "codex" { "/quit" } else { "/exit" };
        self.ctl.pending_end = None;
        self.drop_screen();
        Action::Send(host, Send::Reply(exit.into()))
    }

    // --- inbox ------------------------------------------------------------------

    /// `a`: the inbox — every session, what wants you first.
    pub fn toggle_inbox(&mut self) {
        self.ctl.inbox = !self.ctl.inbox;
        self.set_status(if self.ctl.inbox {
            "inbox: what needs you first · a to go back"
        } else {
            "back to the project view"
        });
        self.dirty = true;
    }

    pub fn inbox_open(&self) -> bool {
        self.ctl.inbox
    }

    /// Agents in inbox order: waiting on you, finished, working, idle,
    /// stale; most recently active first within each.
    pub(crate) fn inbox_entries(&self, now: u64) -> Vec<Entry> {
        let mut agents: Vec<&Agent> = self
            .snapshot
            .agents
            .iter()
            .filter(|a| self.matches_agent(a))
            .collect();
        agents.sort_by_key(|a| {
            (
                self.inbox_urgency(a, now),
                std::cmp::Reverse(a.settled_at()),
            )
        });
        let mut out: Vec<Entry> = agents.into_iter().map(|a| Entry::Agent(a.pid)).collect();
        // then what exited lately, to resume
        let q = self.filter.to_lowercase();
        out.extend(
            self.snapshot
                .ended
                .iter()
                .filter(|e| {
                    q.is_empty()
                        || e.cwd.to_string_lossy().to_lowercase().contains(&q)
                        || e.summary
                            .title
                            .as_deref()
                            .is_some_and(|t| t.to_lowercase().contains(&q))
                })
                .map(|e| Entry::Ended(e.id.clone())),
        );
        // while you're moving through it, hold the order still: rows that
        // are still there keep their place, new ones go at the end
        let browsing = self.last_input.elapsed() < INBOX_HOLD && !self.display.is_empty();
        if browsing
            && self
                .display
                .iter()
                .all(|e| matches!(e, Entry::Agent(_) | Entry::Ended(_)))
        {
            let mut held: Vec<Entry> = self
                .display
                .iter()
                .filter(|e| out.contains(e))
                .cloned()
                .collect();
            held.extend(out.into_iter().filter(|e| !self.display.contains(e)));
            return held;
        }
        out
    }
}

fn cant_type(a: &Agent) -> String {
    match &a.host {
        Some(h) => format!(
            "marina can only type into herdr or tmux panes — {} is a {}; Enter jumps there",
            a.name,
            h.describe()
        ),
        None => format!("{} has no terminal marina can reach", a.name),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::model::Snapshot;

    fn snap(seq: u64, updated: u64) -> Arc<Snapshot> {
        let mut s = Snapshot::sample();
        s.seq = seq;
        let mut a = Agent::sample(7, "aaaa1111", "api");
        a.children.retain(|c| !c.shell); // idle
        a.summary = Some(crate::transcript::Summary {
            updated: Some(updated),
            last_reply: Some("Done. Want me to push?".into()),
            ..Default::default()
        });
        s.agents = vec![a];
        Arc::new(s)
    }

    #[test]
    fn a_new_reply_is_unread_until_you_look() {
        let mut app = App::new();
        app.apply(snap(1, 100));
        let now = 10_000;
        let a = app.snapshot.agents[0].clone();
        assert!(app.is_unread(&a, now), "never seen");
        assert_eq!(app.inbox_urgency(&a, now), 1, "sorts with new");

        // selecting it in the project view doesn't count as reading it
        app.selected = Some(Entry::Agent(7));
        app.apply(snap(2, 100));
        assert!(app.is_unread(&a, now));

        // the inbox shows its detail: read
        app.toggle_inbox();
        app.selected = Some(Entry::Agent(7));
        app.apply(snap(3, 100));
        assert!(!app.is_unread(&a, now));

        // it says something new while you're elsewhere
        app.toggle_inbox();
        app.selected = None;
        app.apply(snap(4, 200));
        let a = app.snapshot.agents[0].clone();
        assert!(app.is_unread(&a, now));
        app.mark_read(7);
        assert!(!app.is_unread(&a, now), "jumping to it reads it");
    }

    #[test]
    fn stale_screens_and_ended_sessions_never_drive_input() {
        let mut app = App::new();
        app.apply(snap(1, 100));
        let a = app.snapshot.agents[0].clone();
        let dialog = vec!["Do you want to proceed?".to_string(), "❯ 1. Yes".into()];
        // a read requested before marina sent something lands late: dropped
        let generation = app.ctl.screen_gen;
        app.drop_screen();
        app.screen_ready(7, generation, Some(dialog.clone()));
        assert!(app.screen_of(7).is_none());
        assert!(!app.waiting_on_you(&a));
        // a current read counts
        app.screen_ready(7, app.ctl.screen_gen, Some(dialog));
        assert!(app.waiting_on_you(&a));
        // ...but not once it's old
        app.ctl.screen.as_mut().unwrap().at -= SCREEN_EVERY * 3;
        assert!(!app.waiting_on_you(&a));

        // a reply box whose session ended closes instead of eating keys
        app.ctl.reply = Some((7, "hi".into()));
        let mut gone = Snapshot::sample();
        gone.seq = 2;
        app.apply(Arc::new(gone));
        assert!(!app.is_replying());

        // inbox clicks must land in the list, not the detail beside it
        app.ctl.list_rect = Rect::new(0, 0, 60, 30);
        app.ctl.hits = vec![(5, Entry::Agent(7))];
        assert_eq!(app.card_at(10, 5), Some(Entry::Agent(7)));
        assert_eq!(app.card_at(90, 5), None);
    }

    #[test]
    fn two_sessions_working_in_one_tree_are_flagged_once() {
        let mut app = App::new();
        let both = |seq: u64, working: bool| {
            let mut s = Snapshot::sample();
            s.seq = seq;
            let mut a = Agent::sample(1, "aaaa1111", "pix");
            let mut b = Agent::sample(2, "bbbb2222", "pix");
            for x in [&mut a, &mut b] {
                x.tree = Some(std::path::PathBuf::from("/Users/me/dev/pix"));
                if !working {
                    x.children.retain(|c| !c.shell);
                }
            }
            s.agents = vec![a, b];
            Arc::new(s)
        };
        app.apply(both(1, false));
        let a = app.snapshot.agents[0].clone();
        assert_eq!(app.tree_mates(&a).len(), 1);
        app.snapshot = both(2, true);
        let n = app.agent_notices(1_000);
        assert!(
            n.iter()
                .any(|(t, _)| t.contains("working in the same tree")),
            "{n:?}"
        );
        assert!(
            !app.agent_notices(1_001)
                .iter()
                .any(|(t, _)| t.contains("same tree")),
            "once"
        );
    }

    #[test]
    fn the_inbox_orders_by_finished_turns_and_holds_still_while_browsing() {
        let mut app = App::new();
        let make = |seq: u64, b_working: bool| {
            let mut s = Snapshot::sample();
            s.seq = seq;
            let mut a = Agent::sample(1, "aaaa1111", "a");
            let mut b = Agent::sample(2, "bbbb2222", "b");
            for (x, ended) in [(&mut a, 500), (&mut b, 100)] {
                x.summary = Some(crate::transcript::Summary {
                    turn_ended: Some(ended),
                    updated: Some(ended),
                    ..Default::default()
                });
            }
            a.children.retain(|c| !c.shell);
            if !b_working {
                b.children.retain(|c| !c.shell);
            }
            s.agents = vec![a, b];
            Arc::new(s)
        };
        app.apply(make(1, false));
        app.ctl.seen.mark("aaaa1111", 500);
        app.ctl.seen.mark("bbbb2222", 100);
        app.toggle_inbox();
        app.last_input = Instant::now() - INBOX_HOLD * 2;
        let order = app.inbox_entries(10_000);
        assert_eq!(
            order[..2],
            [Entry::Agent(1), Entry::Agent(2)],
            "newest finished turn first"
        );
        // b starts working: it stays put (no working group, same turn end)
        app.apply(make(2, true));
        assert_eq!(
            app.inbox_entries(10_000)[..2],
            [Entry::Agent(1), Entry::Agent(2)]
        );
        // while browsing, even a real reorder waits
        app.display = vec![Entry::Agent(2), Entry::Agent(1)];
        app.last_input = Instant::now();
        assert_eq!(
            app.inbox_entries(10_000)[..2],
            [Entry::Agent(2), Entry::Agent(1)]
        );
    }

    #[test]
    fn a_first_run_starts_with_everything_read() {
        let dir = std::env::temp_dir().join(format!("marina-seen-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut app = App::new();
        app.ctl.seen = Seen::load(dir.join("seen.json"));
        app.apply(snap(1, 100));
        let a = app.snapshot.agents[0].clone();
        assert!(!app.is_unread(&a, 10_000), "seeded from what's there");
        assert!(dir.join("seen.json").is_file(), "persisted");
        let again = Seen::load(dir.join("seen.json"));
        assert_eq!(again.get("aaaa1111"), Some(100));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
