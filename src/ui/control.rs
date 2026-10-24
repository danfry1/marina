//! Agent control: the preview pane (`p`), replying / approving / rejecting
//! from marina (`r` / `y` / `n`), and the inbox view (`a`).
//!
//! marina only ever types into a session on an explicit keystroke on that
//! session's row, and only where the pane can be addressed exactly and its
//! screen read first: a multiplexer (herdr, tmux) or iTerm2, by tty. Other
//! GUI terminals are never typed into.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use ratatui::layout::Rect;

use super::format::unix_now;
use super::state::{App, Entry};
use crate::hosts::{self, HostInfo, Send};
use crate::model::Agent;

/// A session's question stays *your turn* this long; after, it's parked.
const YOUR_TURN_FOR: u64 = 3 * 86_400;

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
    /// Why it couldn't be read (then `rows` is empty).
    pub error: Option<String>,
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
    /// Set aside (`d`): session id -> transcript time when you did. Parked
    /// until it records something newer.
    pub aside: Seen,
    /// Canned replies on keys 1-9 (`[agents] quick`).
    pub quick: Vec<String>,
    /// A new session being set up (`N`).
    pub new_session: Option<NewDraft>,
    /// `X` pressed once on this agent: press again to end it.
    pub pending_end: Option<(u32, Instant)>,
    /// Open on the inbox once the first snapshot shows agent sessions.
    pub start_in_inbox: bool,
    /// Said once with the first look: what's left to set up.
    pub setup_tip: Option<String>,
    /// The inbox's parked / ended groups are unfolded.
    pub parked_open: bool,
    pub ended_open: bool,
    /// The inbox section (0 needs you … 3 parked) each session was last
    /// shown in: while you browse, a row keeps its section as well as its
    /// place, so headers stay contiguous. Updated in `App::reconcile`.
    pub sections: HashMap<u32, u8>,
    /// The inbox order is held this frame — decided once per `reconcile`,
    /// so the order and the sections drawn agree.
    pub holding: bool,
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

    /// Like `load`, but a missing file is just empty.
    pub fn load_plain(path: PathBuf) -> Seen {
        let mut s = Seen::load(path);
        s.seed = false;
        s
    }

    fn unmark(&mut self, id: &str) {
        self.dirty |= self.map.remove(id).is_some();
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
    /// The same for several sessions, one after another.
    SendAll(Vec<(HostInfo, Send)>),
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
                    self.set_status(format!(
                        "marina can show the screen of {} sessions only",
                        crate::hosts::TYPABLE
                    ));
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

    pub fn screen_ready(&mut self, pid: u32, generation: u64, rows: Result<Vec<String>, String>) {
        self.ctl.screen_pending = false;
        if generation != self.ctl.screen_gen {
            return; // read before marina last sent something: stale
        }
        let (rows, error) = match rows {
            Ok(rows) => (rows, None),
            Err(why) => (Vec::new(), Some(why)),
        };
        self.ctl.screen = Some(Screen {
            pid,
            at: Instant::now(),
            rows,
            error,
        });
        self.dirty = true;
    }

    /// Why the current screen of agent `pid` couldn't be read, if it couldn't.
    pub(crate) fn screen_error(&self, pid: u32) -> Option<&str> {
        self.ctl
            .screen
            .as_ref()
            .filter(|s| s.pid == pid)
            .and_then(|s| s.error.as_deref())
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

    /// Enter on a fold row: show or hide the parked / ended sessions.
    pub fn toggle_fold(&mut self) -> bool {
        match self.selected {
            Some(Entry::Fold(0)) => self.ctl.parked_open = !self.ctl.parked_open,
            Some(Entry::Fold(_)) => self.ctl.ended_open = !self.ctl.ended_open,
            _ => return false,
        }
        // let the list reflow now, not after the browsing hold
        self.last_input = Instant::now() - INBOX_HOLD;
        self.dirty = true;
        true
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

    /// Set aside with `d`, and nothing new since.
    pub(crate) fn is_aside(&self, a: &Agent) -> bool {
        let Some(id) = a.session.as_deref() else {
            return false;
        };
        let at = a.summary.as_ref().and_then(|s| s.updated).unwrap_or(0);
        self.ctl.aside.get(id).is_some_and(|when| when >= at)
    }

    /// The inbox's groups, by what's needed of you: 0 *needs you* (an
    /// approval or a question is up), 1 *your turn* (it finished and asks
    /// you something, or has a reply you haven't read), 2 *working*,
    /// 3 *parked* (finished, nothing asked — or set aside).
    pub(crate) fn inbox_urgency(&self, a: &Agent, now: u64) -> u8 {
        if self.waiting_on_you(a) {
            return 0;
        }
        if a.is_working(now) {
            return 2;
        }
        if self.is_aside(a) {
            return 3;
        }
        let recent = now.saturating_sub(a.settled_at()) < YOUR_TURN_FOR;
        let asks = a.summary.as_ref().is_some_and(|s| s.ask().is_some());
        let done = a.host_status() == Some(crate::hosts::HostStatus::Done);
        if (asks && recent) || done || self.is_unread(a, now) {
            1
        } else {
            3
        }
    }

    /// Why `a` is in its inbox group, in a few words, and how sure marina
    /// is: it said so (hooks, herdr), or marina is inferring it.
    pub(crate) fn inbox_reason(&self, a: &Agent, now: u64) -> String {
        let source = if a.host.as_ref().is_some_and(|h| h.status.is_some()) {
            "herdr says so"
        } else if a.reported().is_some() {
            "it reported this itself"
        } else {
            "inferred"
        };
        match self.inbox_urgency(a, now) {
            0 if a.host_status().is_none() => {
                if a.summary.as_ref().is_some_and(|s| s.pending_tool.is_some()) {
                    "inferred: a tool call is pending and nothing is running".into()
                } else {
                    "its screen shows a dialog".into()
                }
            }
            0 => format!("waiting on an approval or a question — {source}"),
            2 if a.shells().next().is_some() => "running a command".into(),
            2 if a.host_status().is_some() => format!("working — {source}"),
            2 if a.tty.is_some() => "inferred: its screen is changing".into(),
            2 => "inferred: it's using CPU".into(),
            1 if a.host_status() == Some(crate::hosts::HostStatus::Done) => {
                "herdr marks it done and unread".into()
            }
            1 if a.summary.as_ref().is_some_and(|s| s.ask().is_some()) => {
                "its last reply asks you something · d sets it aside".into()
            }
            1 => "a reply you haven't read · d sets it aside".into(),
            _ if self.is_aside(a) => {
                "set aside until it does something new · d brings it back".into()
            }
            _ if a.summary.as_ref().is_some_and(|s| s.ask().is_some()) => format!(
                "it asked something {} ago — over 3 days, so parked",
                super::format::fmt_age(now.saturating_sub(a.settled_at()))
            ),
            _ if a.summary.is_none() => "no transcript, and nothing running".into(),
            _ => "finished, and its last reply asks nothing".into(),
        }
    }

    /// `d`: set the selected session aside (parked until it records
    /// something new), or bring it back.
    pub fn toggle_aside(&mut self) {
        let Some(a) = self.selected_agent() else {
            return;
        };
        let label = self.agent_label(a);
        let Some(id) = a.session.clone() else {
            self.set_status(format!(
                "{label}: marina doesn't know its session id, so can't set it aside"
            ));
            return;
        };
        let now = unix_now();
        if self.is_aside(a) {
            self.ctl.aside.unmark(&id);
            self.set_status(format!("{label} is back in your inbox"));
        } else if self.waiting_on_you(a) || a.is_working(now) {
            self.set_status(format!(
                "{label} is busy — only a finished session can be set aside"
            ));
            return;
        } else {
            let at = a.summary.as_ref().and_then(|s| s.updated).unwrap_or(now);
            self.ctl.aside.mark(&id, at);
            self.set_status(format!(
                "{label} set aside: parked until it does something new · d on it brings it back"
            ));
        }
        let live: Vec<String> = self
            .snapshot
            .agents
            .iter()
            .filter_map(|a| a.session.clone())
            .collect();
        let live: Vec<&str> = live.iter().map(String::as_str).collect();
        self.ctl.aside.save(&live);
        // the row moves now, not after the browsing hold
        self.last_input = Instant::now() - INBOX_HOLD;
        self.dirty = true;
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
    /// `X` on the *parked* fold: end every parked session marina can type
    /// into, after a second press.
    fn end_parked(&mut self) -> Action {
        let now = unix_now();
        let parked: Vec<(HostInfo, &'static str, bool)> = self
            .snapshot
            .agents
            .iter()
            .filter(|a| self.inbox_urgency(a, now) == 3)
            .filter_map(|a| {
                let host = a.host.clone().filter(|h| h.kind.interactive())?;
                let exit = if a.name == "codex" { "/quit" } else { "/exit" };
                Some((host, exit, a.git.is_some_and(|g| !g.is_clean())))
            })
            .collect();
        let total = self
            .snapshot
            .agents
            .iter()
            .filter(|a| self.inbox_urgency(a, now) == 3)
            .count();
        if parked.is_empty() {
            self.set_status(if total == 0 {
                "nothing is parked".to_string()
            } else {
                format!(
                    "none of the {total} parked sessions is in {} — end them in their windows",
                    hosts::TYPABLE
                )
            });
            return Action::None;
        }
        let confirmed = self
            .ctl
            .pending_end
            .is_some_and(|(p, at)| p == 0 && at.elapsed() < Duration::from_secs(4));
        if !confirmed {
            let dirty = parked.iter().filter(|p| p.2).count();
            let mut note = String::new();
            if dirty > 0 {
                note.push_str(&format!(" — {dirty} with uncommitted work (kept on disk)"));
            }
            if parked.len() < total {
                note.push_str(&format!(
                    " · {} marina can't type into stay",
                    total - parked.len()
                ));
            }
            self.ctl.pending_end = Some((0, Instant::now()));
            self.set_status(format!(
                "press X again to end {} parked session{}{note} (resumable later)",
                parked.len(),
                if parked.len() == 1 { "" } else { "s" }
            ));
            return Action::None;
        }
        self.ctl.pending_end = None;
        self.drop_screen();
        Action::SendAll(
            parked
                .into_iter()
                .map(|(h, exit, _)| (h, Send::Reply(exit.into())))
                .collect(),
        )
    }

    pub fn end_session(&mut self) -> Action {
        if self.selected == Some(Entry::Fold(0)) {
            return self.end_parked();
        }
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
        let subs = a.subagents_running();
        if subs > 0 && !self.waiting_on_you(a) {
            let (n, them) = if subs == 1 {
                ("a subagent".to_string(), "it")
            } else {
                (format!("{subs} subagents"), "them")
            };
            self.set_status(format!(
                "{label} has {n} still running — ending it would stop {them}; let {them} finish first"
            ));
            self.ctl.pending_end = None;
            return Action::None;
        }
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
        // a fresh view: nothing held over from the other one, or from the
        // inbox's last visit
        self.ctl.sections.clear();
        self.ctl.holding = false;
        self.last_input = Instant::now() - INBOX_HOLD;
        self.set_status(if self.ctl.inbox {
            "inbox: what needs you first · a to go back"
        } else {
            "back to the project view"
        });
        self.dirty = true;
    }

    pub fn set_start_in_inbox(&mut self, yes: bool) {
        self.ctl.start_in_inbox = yes;
    }

    /// On the first snapshot: open on the inbox when there are agent
    /// sessions, and say how to get around.
    pub(crate) fn first_look(&mut self) {
        let n = self.snapshot.agents.len();
        if n == 0 {
            return;
        }
        if std::mem::take(&mut self.ctl.start_in_inbox) {
            self.ctl.inbox = true;
        }
        let other = if self.ctl.inbox {
            "servers"
        } else {
            "the agent inbox"
        };
        let tip = self
            .ctl
            .setup_tip
            .take()
            .map(|t| format!(" · {t}"))
            .unwrap_or_default();
        self.set_status(format!(
            "{n} agent session{} · a: {other} · Enter: jump · ?: all keys{tip}",
            if n == 1 { "" } else { "s" }
        ));
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
        // a filter shows everything it matches; otherwise parked and ended
        // sessions fold away behind one row each
        let q = self.filter.to_lowercase();
        let searching = !q.is_empty();
        let (active, parked): (Vec<&Agent>, Vec<&Agent>) = agents
            .into_iter()
            .partition(|a| self.inbox_urgency(a, now) < 3);
        let mut out: Vec<Entry> = active.iter().map(|a| Entry::Agent(a.pid)).collect();
        if !parked.is_empty() {
            out.push(Entry::Fold(0));
            if self.ctl.parked_open || searching {
                out.extend(parked.iter().map(|a| Entry::Agent(a.pid)));
            }
        }
        let ended: Vec<Entry> = self
            .snapshot
            .ended
            .iter()
            .filter(|e| {
                !searching
                    || e.cwd.to_string_lossy().to_lowercase().contains(&q)
                    || e.summary
                        .title
                        .as_deref()
                        .is_some_and(|t| t.to_lowercase().contains(&q))
            })
            .map(|e| Entry::Ended(e.id.clone()))
            .collect();
        if !ended.is_empty() {
            out.push(Entry::Fold(1));
            if self.ctl.ended_open || searching {
                out.extend(ended);
            }
        }
        // while you're moving through it, hold the order still: rows that
        // are still there keep their place (and section), new ones go at
        // the end of their own section
        if self.ctl.holding {
            let mut held: Vec<Entry> = self
                .display
                .iter()
                .filter(|e| out.contains(e))
                .cloned()
                .collect();
            for e in out.into_iter().filter(|e| !self.display.contains(e)) {
                let r = self.rank(&e, now);
                let at = held
                    .iter()
                    .rposition(|h| self.rank(h, now) <= r)
                    .map_or(0, |i| i + 1);
                held.insert(at, e);
            }
            // held parked rows stay under their fold, even with none parked now
            let parked = |e: &Entry| e != &Entry::Fold(0) && self.rank(e, now) == 3;
            if !held.contains(&Entry::Fold(0)) {
                if let Some(i) = held.iter().position(parked) {
                    held.insert(i, Entry::Fold(0));
                }
            }
            return held;
        }
        out
    }

    /// The inbox order is to be held: a key was pressed in the last
    /// `INBOX_HOLD`, over a list of inbox rows. Read once per frame, into
    /// `Control::holding`.
    pub(crate) fn browsing_inbox(&self) -> bool {
        self.last_input.elapsed() < INBOX_HOLD
            && !self.display.is_empty()
            && self
                .display
                .iter()
                .all(|e| matches!(e, Entry::Agent(_) | Entry::Ended(_) | Entry::Fold(_)))
    }

    /// The section `a` is shown under: where it was while the order is
    /// held, else where it is now (its urgency, parked and stale as one).
    pub(crate) fn section(&self, a: &Agent, now: u64) -> u8 {
        let held = self.ctl.holding.then(|| self.ctl.sections.get(&a.pid));
        match held.flatten() {
            Some(&s) => s,
            None => self.inbox_urgency(a, now).min(3),
        }
    }

    /// Where a row sorts in the inbox: its section, the parked fold with
    /// (before) the parked rows, ended sessions last.
    fn rank(&self, e: &Entry, now: u64) -> u8 {
        match e {
            Entry::Agent(pid) => self
                .snapshot
                .agents
                .iter()
                .find(|a| a.pid == *pid)
                .map_or(3, |a| self.section(a, now)),
            Entry::Fold(0) => 3,
            _ => 4,
        }
    }

    /// Remember the section each shown session is in, for the hold.
    pub(crate) fn note_sections(&mut self) {
        let now = unix_now();
        let sections: HashMap<u32, u8> = self
            .display
            .iter()
            .filter_map(|e| match e {
                Entry::Agent(pid) => self.snapshot.agents.iter().find(|a| a.pid == *pid),
                _ => None,
            })
            .map(|a| (a.pid, self.section(a, now)))
            .collect();
        self.ctl.sections = sections;
    }
}

fn cant_type(a: &Agent) -> String {
    match &a.host {
        Some(h) => format!(
            "marina can only type into {} sessions — {} is a {}; Enter jumps there",
            crate::hosts::TYPABLE,
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
        app.screen_ready(7, generation, Ok(dialog.clone()));
        assert!(app.screen_of(7).is_none());
        assert!(!app.waiting_on_you(&a));
        // a current read counts
        app.screen_ready(7, app.ctl.screen_gen, Ok(dialog));
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
                    recap: Some("Done. Next: say go.".into()),
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
        // b starts working: still after a (working follows your turn)
        app.apply(make(2, true));
        assert_eq!(
            app.inbox_entries(10_000)[..2],
            [Entry::Agent(1), Entry::Agent(2)]
        );
        // while browsing, even a real reorder waits
        app.display = vec![Entry::Agent(2), Entry::Agent(1)];
        app.last_input = Instant::now();
        app.ctl.holding = app.browsing_inbox();
        assert_eq!(
            app.inbox_entries(10_000)[..2],
            [Entry::Agent(2), Entry::Agent(1)]
        );
    }

    #[test]
    fn while_browsing_rows_keep_their_section_and_new_ones_join_theirs() {
        let mut app = App::new();
        let now = unix_now();
        let make = move |seq: u64, c_working: bool, d: bool| {
            let mut s = Snapshot::sample();
            s.seq = seq;
            let finished = |pid: u32, id: &str, at: u64| {
                let mut a = Agent::sample(pid, id, id);
                a.children.retain(|c| !c.shell);
                a.summary = Some(crate::transcript::Summary {
                    turn_ended: Some(at),
                    updated: Some(at),
                    recap: Some("Done. Next: say go.".into()),
                    ..Default::default()
                });
                a
            };
            let mut c = finished(3, "cccc3333", now - 60);
            if c_working {
                c = Agent::sample(3, "cccc3333", "cccc3333");
            }
            s.agents = vec![
                finished(1, "aaaa1111", now - 600),
                finished(2, "bbbb2222", now - 900),
                c,
            ];
            if d {
                let mut d = finished(4, "dddd4444", now - 1200);
                d.waiting = true;
                s.agents.push(d);
            }
            Arc::new(s)
        };
        app.apply(make(1, true, false));
        app.toggle_inbox();
        app.last_input = Instant::now() - INBOX_HOLD * 2;
        app.reconcile();
        let agents = |app: &App| -> Vec<Entry> {
            app.display
                .iter()
                .filter(|e| matches!(e, Entry::Agent(_)))
                .cloned()
                .collect()
        };
        assert_eq!(
            agents(&app),
            [Entry::Agent(1), Entry::Agent(2), Entry::Agent(3)]
        );

        // browsing: c finishes (it would sort first), d starts waiting on you
        app.last_input = Instant::now();
        app.apply(make(2, false, true));
        app.reconcile();
        assert_eq!(
            agents(&app),
            [
                Entry::Agent(4),
                Entry::Agent(1),
                Entry::Agent(2),
                Entry::Agent(3)
            ],
            "c keeps its place; d goes into needs-you, not to the bottom"
        );
        let c = app.snapshot.agents.iter().find(|a| a.pid == 3).unwrap();
        assert_eq!(app.section(c, unix_now()), 2, "still shown under working");

        // the hold over, everything is where it belongs
        app.last_input = Instant::now() - INBOX_HOLD * 2;
        app.reconcile();
        assert_eq!(
            agents(&app),
            [
                Entry::Agent(4),
                Entry::Agent(3),
                Entry::Agent(1),
                Entry::Agent(2)
            ]
        );

        // leaving and coming back holds nothing over from before, even
        // straight after a key press
        app.last_input = Instant::now();
        app.reconcile();
        assert!(!app.ctl.sections.is_empty());
        app.toggle_inbox();
        app.select_next();
        app.toggle_inbox();
        app.reconcile();
        assert!(!app.ctl.holding);
        assert_eq!(agents(&app)[0], Entry::Agent(4));
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
