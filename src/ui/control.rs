//! Agent control: the preview pane (`p`), replying / approving / rejecting
//! from marina (`r` / `y` / `n`), and the inbox view (`a`).
//!
//! marina only ever types into a session on an explicit keystroke on that
//! session's row, and only through a multiplexer that can address the pane
//! (herdr, tmux) — never by synthesising keystrokes into a GUI terminal.

use std::time::{Duration, Instant};

use ratatui::layout::Rect;

use super::state::{App, Entry};
use crate::hosts::{self, HostInfo, Send};
use crate::model::Agent;

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
    /// Reply being typed: (agent pid, text).
    pub reply: Option<(u32, String)>,
    /// The inbox view replaces the project/session table.
    pub inbox: bool,
    /// Inbox list: first visible row, and (screen row, agent pid) per
    /// visible card row, for mouse hits.
    pub list_top: usize,
    pub hits: Vec<(u16, u32)>,
    /// Detail pane: where it was drawn, its scroll, and whose it was.
    pub detail_rect: Rect,
    pub detail_scroll: usize,
    pub detail_pid: Option<u32>,
}

/// What the main loop should do after a control key.
pub enum Action {
    None,
    /// Send this to the agent's host on a background thread.
    Send(HostInfo, Send, bool),
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
    pub fn screen_due(&mut self) -> Option<(u32, HostInfo)> {
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
        Some((pid, host))
    }

    pub fn screen_ready(&mut self, pid: u32, rows: Option<Vec<String>>) {
        self.ctl.screen_pending = false;
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

    /// Is `a` at an approval / question dialog right now? Its host says so
    /// (herdr), or its screen shows one.
    pub(crate) fn waiting_on_you(&self, a: &Agent) -> bool {
        a.is_blocked() || self.screen_of(a.pid).is_some_and(hosts::shows_dialog)
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
        }
    }

    /// Mouse: the inbox card at screen row `y`, if any.
    pub(crate) fn card_at(&self, y: u16) -> Option<u32> {
        self.ctl.hits.iter().find(|(r, _)| *r == y).map(|(_, p)| *p)
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
        self.ctl.screen = None; // re-read after it reacts
        Action::Send(host, what, true)
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
        let waiting = self.waiting_on_you(a);
        self.ctl.screen = None;
        Action::Send(host, Send::Reply(text), waiting)
    }

    /// The reply being typed and who it's for.
    pub(crate) fn reply_draft(&self) -> Option<(String, &str)> {
        let (pid, text) = self.ctl.reply.as_ref()?;
        let a = self.snapshot.agents.iter().find(|a| a.pid == *pid)?;
        Some((self.agent_label(a), text.as_str()))
    }

    pub(crate) fn agent_label(&self, a: &Agent) -> String {
        self.agent_session_of
            .get(&a.pid)
            .cloned()
            .unwrap_or_else(|| a.name.clone())
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
        agents.sort_by_key(|a| (a.urgency(now), std::cmp::Reverse(a.last_active())));
        agents.into_iter().map(|a| Entry::Agent(a.pid)).collect()
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
