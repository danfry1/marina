//! View state lives only here and survives snapshot swaps. Targets are grouped
//! by project (or, with `v`, by the agent session / launcher that started
//! them): a group with several targets gets a collapsible header and a
//! group-kill; a lone target renders as a plain row. In session view every
//! session gets a header, since the session *is* the information. Session
//! view also lists the agent sessions themselves (`Entry::Agent`) — a live
//! agent with servers heads its group; one without is a row of its own.
//! Agents are never in `selected_targets`, so no verb can reach one.
//! Selection is held by value
//! (`Entry`), not index, so the cursor doesn't jump when data refreshes; if the
//! selected row vanishes the cursor falls to its nearest neighbor (the same
//! list position), never back to the top. Volatile sorts freeze briefly after a
//! keystroke so rows don't slide out from under the cursor.
//!
//! `dirty` gates redraws: the main loop only draws when something changed (or a
//! transient animation — kill countdown, new-row flash — is running), so an
//! idle cockpit does no per-frame work.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use ratatui::widgets::TableState;

use super::format::{fmt_mem, tildify, unix_now};
use crate::model::{Agent, Snapshot, Target, TargetKey};

pub(crate) const FREEZE: Duration = Duration::from_secs(2);
pub(crate) const STATUS_TTL: Duration = Duration::from_secs(4);
/// Orphan notices outlive ordinary status messages — they ask for a decision.
pub(crate) const NOTICE_TTL: Duration = Duration::from_secs(20);
/// An agent working at least this long gets a "finished" notice when it stops.
const AGENT_WORK_MIN: u64 = 30;
/// ...once it has been quiet this long (rides out pauses between tool calls).
const AGENT_QUIET: u64 = 10;
/// How long a newly-appeared row flashes.
pub(crate) const FLASH: Duration = Duration::from_millis(1500);
/// How long after a marina-initiated kill/restart a vanished target is NOT
/// reported as "exited unexpectedly".
const SUPPRESS_TTL: Duration = Duration::from_secs(20);
/// Short-lived targets (a test runner binding a port for seconds) exiting is
/// normal — only report unexpected exits for things that ran at least this long.
const CRASH_MIN_UPTIME: u64 = 60;

#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum SortMode {
    Port,
    Cpu,
    Mem,
}

impl SortMode {
    pub(crate) fn label(self) -> &'static str {
        match self {
            SortMode::Port => "port",
            SortMode::Cpu => "cpu",
            SortMode::Mem => "mem",
        }
    }
    fn next(self) -> Self {
        match self {
            SortMode::Port => SortMode::Cpu,
            SortMode::Cpu => SortMode::Mem,
            SortMode::Mem => SortMode::Port,
        }
    }
    fn is_volatile(self) -> bool {
        !matches!(self, SortMode::Port)
    }
}

/// What group headers collect: a project, or the session that launched it.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum GroupBy {
    Project,
    Session,
}

/// A visible row: a project group header, a member of an (expanded) group, a
/// standalone single-target project, or (session view) an agent session,
/// keyed by pid.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Entry {
    Group(String),
    Member(TargetKey),
    Target(TargetKey),
    Agent(u32),
    /// Project view: the folded `agents` section at the bottom. Its own
    /// variant, so it can never collide with (or kill) a project named so.
    Agents,
}

pub(crate) struct LogView {
    pub(crate) title: String,
    pub(crate) lines: VecDeque<String>,
    pub(crate) rx: Receiver<String>,
    pub(crate) stop: Arc<AtomicBool>,
    /// `Some(i)` = pinned with buffer line `i` at the top; `None` = follow tail.
    pub(crate) scroll: Option<usize>,
}

impl LogView {
    fn close(self) {
        self.stop.store(true, Ordering::Relaxed); // end the tail thread
    }
}

/// A kill whose SIGKILL escalation can still be cancelled (`u`). The affected
/// rows render in a "dying" style until the window closes.
pub(crate) struct PendingKill {
    pub(crate) cancel: Arc<AtomicBool>,
    pub(crate) until: Instant,
    pub(crate) label: String,
    pub(crate) keys: Vec<TargetKey>,
}

pub struct App {
    pub(crate) snapshot: Arc<Snapshot>,
    pub(crate) selected: Option<Entry>,
    /// Where the cursor last sat in `display` — the nearest-neighbor fallback
    /// when the selected row vanishes (e.g. after a kill).
    pub(crate) last_index: usize,
    pub(crate) sort: SortMode,
    pub(crate) display: Vec<Entry>,
    pub(crate) collapsed: HashSet<String>,
    pub(crate) last_input: Instant,
    pub(crate) table_state: TableState,
    pub(crate) status: Option<String>,
    pub(crate) status_at: Instant,
    pub(crate) status_ttl: Duration,
    pub(crate) group_by: GroupBy,
    /// Session-view group label per target, rebuilt on each snapshot.
    pub(crate) session_of: HashMap<TargetKey, String>,
    /// Session-view group label per agent pid, rebuilt alongside.
    pub(crate) agent_session_of: HashMap<u32, String>,
    /// Project view's `agents` section is unfolded.
    pub(crate) agents_open: bool,
    /// Notify when an agent finishes or starts waiting (`[notify] agents`).
    pub notify_agents: bool,
    /// Per agent pid: (started, last seen) working, unix seconds.
    agent_streaks: HashMap<u32, (u64, u64)>,
    /// Agents already reported as waiting on a question.
    agent_blocked: HashSet<u32>,
    /// Messages for a desktop notification, drained by the main loop.
    pub(crate) notifications: Vec<String>,
    pub(crate) log: Option<LogView>,
    pub(crate) log_height: u16,
    /// A background log discovery is running (guards double-`T`).
    pub(crate) log_pending: bool,
    pub(crate) pending_kill: Option<PendingKill>,
    pub(crate) filter: String,
    pub(crate) filter_active: bool,
    pub(crate) show_help: bool,
    pub(crate) inspect: bool,
    /// Recently-appeared targets (row flash).
    pub(crate) appeared: HashMap<TargetKey, Instant>,
    /// Targets marina itself killed/restarted — their exit is expected.
    pub(crate) suppress_exit: HashMap<TargetKey, Instant>,
    /// Persistent data problem (port scan failed, sampler died). Unlike
    /// `status` this does not expire.
    pub(crate) data_warning: Option<String>,
    /// Something changed since the last draw.
    pub(crate) dirty: bool,
    // --- layout rects recorded by render for mouse hit-testing ---
    pub(crate) table_rect: Rect,
    pub(crate) log_rect: Option<Rect>,
    /// Header cells that sort on click: (x_start, x_end, mode).
    pub(crate) header_hits: Vec<(u16, u16, SortMode)>,
    /// Preview, reply box and inbox (`ui::control`).
    pub(crate) ctl: super::control::Control,
    /// PROJECT column width at the last render (it grows with the pane).
    pub(crate) project_w: u16,
    /// Recent cpu/mem per target and per agent session, for the inspect
    /// panel's trend line.
    pub(crate) history: HashMap<HistKey, VecDeque<(Instant, f32, u64)>>,
}

impl App {
    pub fn new() -> Self {
        App {
            snapshot: Arc::new(Snapshot::empty()),
            selected: None,
            last_index: 0,
            sort: SortMode::Port,
            display: Vec::new(),
            collapsed: HashSet::new(),
            last_input: Instant::now(),
            table_state: TableState::default(),
            status: None,
            status_at: Instant::now(),
            status_ttl: STATUS_TTL,
            group_by: GroupBy::Project,
            session_of: HashMap::new(),
            agent_session_of: HashMap::new(),
            agents_open: false,
            notify_agents: true,
            agent_streaks: HashMap::new(),
            agent_blocked: HashSet::new(),
            notifications: Vec::new(),
            log: None,
            log_height: 12,
            log_pending: false,
            pending_kill: None,
            filter: String::new(),
            filter_active: false,
            show_help: false,
            inspect: false,
            appeared: HashMap::new(),
            suppress_exit: HashMap::new(),
            data_warning: None,
            dirty: true,
            table_rect: Rect::default(),
            log_rect: None,
            header_hits: Vec::new(),
            project_w: 22,
            ctl: Default::default(),
            history: HashMap::new(),
        }
    }

    // --- snapshot ingestion -------------------------------------------------

    /// Swap in a fresh snapshot; diff it against the old one to flash new rows
    /// and to notice targets that vanished *without* marina killing them.
    pub fn apply(&mut self, snap: Arc<Snapshot>) {
        let old = std::mem::replace(&mut self.snapshot, snap);
        self.record_history();
        self.update_seen();
        let now = Instant::now();
        if old.seq > 0 {
            let old_keys: HashSet<&TargetKey> = old.targets.iter().map(|t| &t.key).collect();
            let new_keys: HashSet<&TargetKey> =
                self.snapshot.targets.iter().map(|t| &t.key).collect();
            for t in &self.snapshot.targets {
                if !old_keys.contains(&t.key) {
                    self.appeared.insert(t.key.clone(), now);
                }
            }
            let mut crashed: Option<String> = None;
            for t in &old.targets {
                if new_keys.contains(&t.key) || self.suppress_exit.contains_key(&t.key) {
                    continue;
                }
                let uptime = unix_now().saturating_sub(t.anchor.start_time);
                if t.anchor.start_time > 0 && uptime >= CRASH_MIN_UPTIME {
                    let what = t
                        .ports
                        .first()
                        .map(|p| format!(":{p}"))
                        .unwrap_or_else(|| t.command_label.clone());
                    crashed = Some(format!("⚠ {} ({what}) exited unexpectedly", t.project));
                }
            }
            if let Some(msg) = crashed {
                self.set_status(msg);
            }
            // An agent session that ended and left servers running: say so
            // once, at the transition, and queue a desktop notification.
            for msg in orphan_notices(&old, &self.snapshot) {
                self.notifications.push(msg.clone());
                self.set_notice(format!("⚠ {msg} · /ended to review, K to stop"));
            }
            if self.notify_agents {
                for msg in self.agent_notices(unix_now()) {
                    self.notifications.push(msg.clone());
                    self.set_notice(format!("{msg} · Enter on its row jumps there"));
                }
            }
        } else {
            let n = self
                .snapshot
                .targets
                .iter()
                .filter(|t| t.launcher.as_ref().is_some_and(|l| l.is_orphaned()))
                .count();
            if n > 0 {
                self.set_notice(format!(
                    "⚠ {n} server{} left running by ended agent sessions · /ended to review",
                    if n == 1 { "" } else { "s" }
                ));
            }
        }
        (self.session_of, self.agent_session_of) =
            session_groups(&self.snapshot.targets, &self.snapshot.agents);
        self.data_warning = self.snapshot.error.clone();
        self.dirty = true;
    }

    /// Agents that finished a stretch of work, or started waiting on a
    /// question, since the last snapshot: `claude·api finished after 12m`,
    /// `claude·api needs you`. Each transition is reported once.
    pub(crate) fn agent_notices(&mut self, now: u64) -> Vec<String> {
        let mut out = Vec::new();
        let live: HashSet<u32> = self.snapshot.agents.iter().map(|a| a.pid).collect();
        self.agent_streaks.retain(|pid, _| live.contains(pid));
        self.agent_blocked.retain(|pid| live.contains(pid));
        for a in &self.snapshot.agents {
            let label = self
                .agent_session_of
                .get(&a.pid)
                .cloned()
                .unwrap_or_else(|| a.name.clone());
            if a.is_blocked() {
                if self.agent_blocked.insert(a.pid) {
                    // say what it wants, so the notification is actionable
                    match a.summary.as_ref().and_then(|s| s.pending_tool.as_deref()) {
                        Some(t) => {
                            out.push(format!("{label} needs you: wants to run {}", clip(t, 90)))
                        }
                        None => out.push(format!("{label} needs you")),
                    }
                }
                self.agent_streaks.remove(&a.pid); // the question says it all
                continue;
            }
            self.agent_blocked.remove(&a.pid);
            if a.is_working(now) {
                self.agent_streaks
                    .entry(a.pid)
                    .and_modify(|s| s.1 = now)
                    .or_insert((now, now));
            } else if let Some(&(start, last)) = self.agent_streaks.get(&a.pid) {
                if now.saturating_sub(last) >= AGENT_QUIET {
                    self.agent_streaks.remove(&a.pid);
                    if last.saturating_sub(start) >= AGENT_WORK_MIN {
                        let said = a
                            .summary
                            .as_ref()
                            .and_then(|s| s.last_reply.as_deref())
                            .map(|r| {
                                format!(": {}", clip(&super::agent_view::first_sentence(r), 110))
                            })
                            .unwrap_or_default();
                        out.push(format!(
                            "{label} finished after {}{said}",
                            super::format::fmt_age(last - start)
                        ));
                    }
                }
            }
        }
        out
    }

    /// Servers this agent session started (its live launcher is the agent).
    pub(crate) fn agent_servers(&self, a: &Agent) -> Vec<&Target> {
        self.snapshot
            .targets
            .iter()
            .filter(|t| {
                t.launcher
                    .as_ref()
                    .is_some_and(|l| l.alive && l.name == a.name && l.pid == Some(a.pid))
            })
            .collect()
    }

    /// Stale, no servers, and nothing uncommitted or unpushed: it looks
    /// finished. (Closing never loses files, and the session can be resumed.)
    pub(crate) fn closable(&self, a: &Agent, now: u64) -> bool {
        a.looks_finished(now, self.agent_servers(a).len())
    }

    /// The sampler channel disconnected — data is frozen; say so, loudly.
    pub fn sampler_died(&mut self) {
        let msg = "sampler thread stopped — data frozen (restart marina)";
        if self.data_warning.as_deref() != Some(msg) {
            self.data_warning = Some(msg.into());
            self.dirty = true;
        }
    }

    /// Per-loop upkeep: expire the status line, the kill-undo window, and row
    /// flashes. Sets `dirty` only when something actually changed.
    pub fn tick(&mut self) {
        if self.status.is_some() && self.status_at.elapsed() > self.status_ttl {
            self.status = None;
            self.dirty = true;
        }
        if self
            .pending_kill
            .as_ref()
            .is_some_and(|pk| Instant::now() >= pk.until)
        {
            self.pending_kill = None;
            self.dirty = true;
        }
        let n = self.appeared.len();
        self.appeared.retain(|_, at| at.elapsed() < FLASH);
        if self.appeared.len() != n {
            self.dirty = true;
        }
        self.suppress_exit
            .retain(|_, at| at.elapsed() < SUPPRESS_TTL);
    }

    /// A transient animation (kill countdown / row flash) is on screen — the
    /// main loop keeps drawing while this holds even without new input/data.
    pub fn has_transient(&self) -> bool {
        self.pending_kill.is_some() || !self.appeared.is_empty()
    }

    pub fn set_status(&mut self, s: impl Into<String>) {
        self.status = Some(s.into());
        self.status_at = Instant::now();
        self.status_ttl = STATUS_TTL;
        self.dirty = true;
    }

    /// A status message that stays up long enough to act on.
    pub fn set_notice(&mut self, s: impl Into<String>) {
        self.set_status(s);
        self.status_ttl = NOTICE_TTL;
    }

    /// Notification texts queued since the last call.
    pub fn take_notifications(&mut self) -> Vec<String> {
        std::mem::take(&mut self.notifications)
    }

    // --- grouping (`v`) -----------------------------------------------------

    pub fn toggle_group_by(&mut self) {
        self.group_by = match self.group_by {
            GroupBy::Project => GroupBy::Session,
            GroupBy::Session => GroupBy::Project,
        };
        self.set_status(match self.group_by {
            GroupBy::Project => "grouped by project",
            GroupBy::Session => "grouped by agent session / launcher",
        });
    }

    /// The group a target belongs to in the current view.
    pub(crate) fn group_key<'a>(&'a self, t: &'a Target) -> &'a str {
        match self.group_by {
            GroupBy::Project => &t.project,
            GroupBy::Session => self
                .session_of
                .get(&t.key)
                .map(String::as_str)
                .unwrap_or("unattributed"),
        }
    }

    pub(crate) fn group_members(&self, g: &str) -> Vec<&Target> {
        self.snapshot
            .targets
            .iter()
            .filter(|t| self.group_key(t) == g)
            .collect()
    }

    /// The agent sessions heading group `g` (session view only).
    pub(crate) fn group_agents(&self, g: &str) -> Vec<&Agent> {
        if self.group_by != GroupBy::Session {
            return Vec::new();
        }
        self.snapshot
            .agents
            .iter()
            .filter(|a| self.agent_session_of.get(&a.pid).map(String::as_str) == Some(g))
            .collect()
    }

    /// Who group `g` belongs to: its live agent, else its first member's
    /// launcher. Styles and describes session headers.
    pub(crate) fn group_launcher(&self, g: &str) -> Option<crate::launcher::Launcher> {
        self.group_agents(g)
            .first()
            .map(|a| a.as_launcher())
            .or_else(|| {
                self.group_members(g)
                    .first()
                    .and_then(|t| t.launcher.clone())
            })
    }

    // --- help overlay (`?`) -------------------------------------------------

    pub fn help_open(&self) -> bool {
        self.show_help
    }
    pub fn toggle_help(&mut self) {
        self.show_help = !self.show_help;
        self.dirty = true;
    }
    pub fn close_help(&mut self) {
        self.show_help = false;
        self.dirty = true;
    }

    // --- sort ---------------------------------------------------------------

    pub fn cycle_sort(&mut self) {
        self.set_sort(self.sort.next());
    }

    pub(crate) fn set_sort(&mut self, m: SortMode) {
        self.sort = m;
        // An explicit sort change means "reorder now" — push the navigation
        // freeze into the past so the new order applies immediately.
        self.last_input = self
            .last_input
            .checked_sub(FREEZE)
            .unwrap_or(self.last_input);
        self.dirty = true;
    }

    // --- selection ----------------------------------------------------------

    pub fn select_next(&mut self) {
        self.move_selection(1);
    }
    pub fn select_prev(&mut self) {
        self.move_selection(-1);
    }
    fn move_selection(&mut self, delta: isize) {
        self.last_input = Instant::now();
        self.dirty = true;
        if self.display.is_empty() {
            return;
        }
        let cur = self
            .selected
            .as_ref()
            .and_then(|s| self.display.iter().position(|e| same_entry(e, s)))
            .unwrap_or(0) as isize;
        let next = (cur + delta).rem_euclid(self.display.len() as isize) as usize;
        self.last_index = next;
        self.selected = Some(self.display[next].clone());
    }
    pub fn jump_top(&mut self) {
        self.last_input = Instant::now();
        self.last_index = 0;
        self.selected = self.display.first().cloned();
        self.dirty = true;
    }
    pub fn jump_bottom(&mut self) {
        self.last_input = Instant::now();
        self.last_index = self.display.len().saturating_sub(1);
        self.selected = self.display.last().cloned();
        self.dirty = true;
    }

    pub fn toggle_collapse(&mut self) {
        if self.selected == Some(Entry::Agents) {
            self.agents_open = !self.agents_open;
            self.dirty = true;
            return;
        }
        if let Some(Entry::Group(p)) = &self.selected {
            let p = p.clone();
            if !self.collapsed.remove(&p) {
                self.collapsed.insert(p);
            }
            self.dirty = true;
        }
    }

    pub fn toggle_inspect(&mut self) {
        self.inspect = !self.inspect;
        self.dirty = true;
    }

    /// The single target under the cursor (a member or standalone) — `None` on
    /// a group header.
    pub fn selected_target(&self) -> Option<&Target> {
        match &self.selected {
            Some(Entry::Member(k)) | Some(Entry::Target(k)) => {
                self.snapshot.targets.iter().find(|t| &t.key == k)
            }
            _ => None,
        }
    }

    /// The agent session under the cursor, if the cursor is on one.
    pub fn selected_agent(&self) -> Option<&Agent> {
        match &self.selected {
            Some(Entry::Agent(pid)) => self.snapshot.agents.iter().find(|a| a.pid == *pid),
            _ => None,
        }
    }

    /// All targets the selection acts on: a whole group, or one target.
    /// Never an agent session — on an agent row this is empty.
    pub fn selected_targets(&self) -> Vec<&Target> {
        match &self.selected {
            Some(Entry::Group(p)) => self.group_members(p),
            Some(Entry::Member(k)) | Some(Entry::Target(k)) => self
                .snapshot
                .targets
                .iter()
                .filter(|t| &t.key == k)
                .collect(),
            Some(Entry::Agent(_)) | Some(Entry::Agents) | None => Vec::new(),
        }
    }

    pub fn selection_label(&self) -> String {
        match &self.selected {
            Some(Entry::Group(p)) => p.clone(),
            Some(Entry::Member(k)) | Some(Entry::Target(k)) => self
                .snapshot
                .targets
                .iter()
                .find(|t| &t.key == k)
                .map(|t| t.project.clone())
                .unwrap_or_default(),
            Some(Entry::Agent(_)) => self
                .selected_agent()
                .map(|a| format!("{} session", a.name))
                .unwrap_or_default(),
            Some(Entry::Agents) => "agent sessions".into(),
            None => String::new(),
        }
    }

    /// Recompute visible entries and re-anchor the selection: by key when the
    /// row still exists, else the nearest neighbor (same list position), never
    /// a jump back to the top. Called by render before drawing.
    pub(crate) fn reconcile(&mut self) {
        self.display = self.compute_entries();
        let idx = self
            .selected
            .as_ref()
            .and_then(|s| self.display.iter().position(|e| same_entry(e, s)));
        let idx = match idx {
            Some(i) => Some(i),
            None if self.display.is_empty() => None,
            None => Some(self.last_index.min(self.display.len() - 1)),
        };
        if let Some(i) = idx {
            self.last_index = i;
        }
        self.selected = idx.map(|i| self.display[i].clone());
        self.table_state.select(idx);
    }

    // --- kill / restart bookkeeping -----------------------------------------

    pub fn note_pending_kill(
        &mut self,
        cancel: Arc<AtomicBool>,
        label: &str,
        keys: Vec<TargetKey>,
    ) {
        self.mark_marina_action(&keys);
        self.pending_kill = Some(PendingKill {
            cancel,
            until: Instant::now() + crate::verbs::GRACE,
            label: label.to_string(),
            keys,
        });
        self.dirty = true;
    }

    /// These targets are being killed/restarted *by marina* — their vanishing
    /// must not be reported as an unexpected exit.
    pub fn mark_marina_action(&mut self, keys: &[TargetKey]) {
        let now = Instant::now();
        for k in keys {
            self.suppress_exit.insert(k.clone(), now);
        }
    }

    pub fn undo_kill(&mut self) {
        match self.pending_kill.take() {
            Some(pk) if Instant::now() < pk.until => {
                pk.cancel.store(true, Ordering::SeqCst);
                self.set_status(format!(
                    "cancelled force-kill for {} — note: SIGTERM was already sent",
                    pk.label
                ));
            }
            _ => self.set_status("nothing to undo"),
        }
        self.dirty = true;
    }

    /// Is this row inside an active kill grace window? (Rendered as dying.)
    pub(crate) fn is_dying(&self, key: &TargetKey) -> bool {
        self.pending_kill
            .as_ref()
            .is_some_and(|pk| pk.keys.contains(key))
    }

    /// Did this row appear within the flash window?
    pub(crate) fn is_flashing(&self, key: &TargetKey) -> bool {
        self.appeared
            .get(key)
            .is_some_and(|at| at.elapsed() < FLASH)
    }

    // --- filter (`/`) -------------------------------------------------------

    pub fn is_filtering(&self) -> bool {
        self.filter_active
    }
    pub fn start_filter(&mut self) {
        self.filter_active = true;
        self.last_input = Instant::now();
        self.dirty = true;
    }
    pub fn filter_push(&mut self, c: char) {
        self.filter.push(c);
        self.last_input = Instant::now();
        self.dirty = true;
    }
    pub fn filter_backspace(&mut self) {
        self.filter.pop();
        self.last_input = Instant::now();
        self.dirty = true;
    }
    pub fn filter_commit(&mut self) {
        self.filter_active = false;
        self.dirty = true;
    }
    pub fn filter_cancel(&mut self) {
        self.filter_active = false;
        self.filter.clear();
        self.dirty = true;
    }
    fn matches(&self, t: &Target) -> bool {
        if self.filter.is_empty() {
            return true;
        }
        let q = self.filter.to_lowercase();
        t.project.to_lowercase().contains(&q)
            || t.command_label.to_lowercase().contains(&q)
            || t.ports.iter().any(|p| p.to_string().contains(&q))
            || t.cwd.to_string_lossy().to_lowercase().contains(&q)
            || t.git_branch
                .as_deref()
                .is_some_and(|b| b.to_lowercase().contains(&q))
            // `/claude`, `/agent`, `/ended` (orphans), `/detached`
            || t.launcher.as_ref().is_some_and(|l| {
                l.short().to_lowercase().contains(&q) || l.kind.as_str().contains(&q)
            })
    }

    pub(crate) fn matches_agent(&self, a: &Agent) -> bool {
        if self.filter.is_empty() {
            return true;
        }
        let q = self.filter.to_lowercase();
        // `/agent` lists every session; `/claude`, `/cargo` (a shell), a dir, …
        "agent".contains(&q)
            || a.name.to_lowercase().contains(&q)
            || a.project.to_lowercase().contains(&q)
            || a.cwd.to_string_lossy().to_lowercase().contains(&q)
            || a.git_branch
                .as_deref()
                .is_some_and(|b| b.to_lowercase().contains(&q))
            || a.session.as_deref().is_some_and(|s| s.starts_with(&q))
            || a.children
                .iter()
                .any(|c| c.label.to_lowercase().contains(&q))
    }

    /// Esc in normal mode: close the most intrusive thing first —
    /// log pane, then inspect panel, then a committed filter.
    pub fn escape(&mut self) {
        if self.log.is_some() {
            self.close_log();
        } else if self.ctl.preview.is_some() {
            self.ctl.preview = None;
            self.dirty = true;
        } else if self.inspect {
            self.inspect = false;
            self.dirty = true;
        } else if !self.filter.is_empty() {
            self.filter.clear();
            self.set_status("filter cleared");
        }
    }

    // --- log pane (`T`) -----------------------------------------------------

    pub fn log_open(&self) -> bool {
        self.log.is_some()
    }

    pub fn open_log(&mut self, title: String, rx: Receiver<String>, stop: Arc<AtomicBool>) {
        if let Some(old) = self.log.take() {
            old.close();
        }
        self.set_status(format!("tailing {title}"));
        self.log = Some(LogView {
            title,
            lines: VecDeque::new(),
            rx,
            stop,
            scroll: None,
        });
        self.dirty = true;
    }

    /// Read from the top instead of following the tail (a diff, not a log).
    pub fn log_from_top(&mut self) {
        if let Some(l) = &mut self.log {
            l.scroll = Some(0);
            self.dirty = true;
        }
    }

    pub fn close_log(&mut self) {
        if let Some(old) = self.log.take() {
            old.close();
            self.set_status("closed logs");
        }
    }

    pub fn pump_log(&mut self) {
        let mut got = false;
        if let Some(l) = &mut self.log {
            while let Ok(line) = l.rx.try_recv() {
                l.lines.push_back(line);
                if l.lines.len() > 2000 {
                    l.lines.pop_front();
                    // keep a pinned view anchored to the same content
                    if let Some(s) = l.scroll.as_mut() {
                        *s = s.saturating_sub(1);
                    }
                }
                got = true;
            }
        }
        if got {
            self.dirty = true;
        }
    }

    fn log_visible_lines(&self) -> usize {
        self.log_height.saturating_sub(2) as usize
    }

    pub fn log_scroll_up(&mut self, n: usize) {
        let h = self.log_visible_lines();
        if let Some(l) = &mut self.log {
            let bottom = l.lines.len().saturating_sub(h);
            let cur = l.scroll.unwrap_or(bottom);
            l.scroll = Some(cur.saturating_sub(n));
            self.dirty = true;
        }
    }

    pub fn log_scroll_down(&mut self, n: usize) {
        let h = self.log_visible_lines();
        if let Some(l) = &mut self.log {
            let bottom = l.lines.len().saturating_sub(h);
            let cur = l.scroll.unwrap_or(bottom);
            let new = cur + n;
            l.scroll = if new >= bottom { None } else { Some(new) };
            self.dirty = true;
        }
    }

    pub fn log_grow(&mut self) {
        if self.log.is_some() {
            self.log_height = (self.log_height + 3).min(30);
            self.dirty = true;
        }
    }
    pub fn log_shrink(&mut self) {
        if self.log.is_some() {
            self.log_height = self.log_height.saturating_sub(3).max(5);
            self.dirty = true;
        }
    }

    // --- mouse --------------------------------------------------------------

    /// Click to select (click a selected group header again to fold it), click
    /// a PORT/CPU/MEM header cell to sort, wheel to move (or scroll the log
    /// pane when hovering it).
    pub fn on_mouse(&mut self, ev: MouseEvent) {
        let (x, y) = (ev.column, ev.row);
        match ev.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if self.show_help {
                    self.close_help();
                    return;
                }
                if self.ctl.inbox {
                    // a card selects; the table geometry below doesn't apply
                    if let Some(pid) = self.card_at(y) {
                        self.selected = Some(Entry::Agent(pid));
                        self.last_input = Instant::now();
                        self.dirty = true;
                    }
                    return;
                }
                let t = self.table_rect;
                if !rect_contains(t, x, y) {
                    return;
                }
                if y == t.y.saturating_add(1) {
                    // header row: sortable column?
                    if let Some(m) = self
                        .header_hits
                        .iter()
                        .find(|&&(a, b, _)| x >= a && x < b)
                        .map(|&(_, _, m)| m)
                    {
                        self.set_sort(m);
                    }
                } else if y >= t.y + 2 && y + 1 < t.y + t.height {
                    let idx = (y - t.y - 2) as usize + self.table_state.offset();
                    if idx < self.display.len() {
                        let e = self.display[idx].clone();
                        let already = self.selected.as_ref().is_some_and(|s| same_entry(s, &e));
                        if already {
                            if matches!(e, Entry::Group(_) | Entry::Agents) {
                                self.toggle_collapse();
                            }
                        } else {
                            self.last_input = Instant::now();
                            self.last_index = idx;
                            self.selected = Some(e);
                        }
                        self.dirty = true;
                    }
                }
            }
            MouseEventKind::ScrollUp => {
                if self.log_rect.is_some_and(|r| rect_contains(r, x, y)) {
                    self.log_scroll_up(3);
                } else if self.detail_visible() && rect_contains(self.ctl.detail_rect, x, y) {
                    self.detail_scroll_by(-3);
                } else {
                    self.select_prev();
                }
            }
            MouseEventKind::ScrollDown => {
                if self.log_rect.is_some_and(|r| rect_contains(r, x, y)) {
                    self.log_scroll_down(3);
                } else if self.detail_visible() && rect_contains(self.ctl.detail_rect, x, y) {
                    self.detail_scroll_by(3);
                } else {
                    self.select_next();
                }
            }
            _ => {}
        }
    }

    /// Append this snapshot's cpu/mem to each row's history; forget rows
    /// that are gone.
    fn record_history(&mut self) {
        let snap = Arc::clone(&self.snapshot);
        let now: Vec<(HistKey, f32, u64)> = snap
            .targets
            .iter()
            .filter(|t| !t.pids.is_empty())
            .map(|t| (HistKey::Target(t.key.clone()), t.cpu_pct, t.mem_bytes))
            .chain(
                snap.agents
                    .iter()
                    .map(|a| (HistKey::Agent(a.pid), a.cpu_pct, a.mem_bytes)),
            )
            .collect();
        let live: HashSet<&HistKey> = now.iter().map(|(k, _, _)| k).collect();
        self.history.retain(|k, _| live.contains(k));
        let at = Instant::now();
        for (k, cpu, mem) in &now {
            let h = self.history.entry(k.clone()).or_default();
            if h.len() == HISTORY {
                h.pop_front();
            }
            h.push_back((at, *cpu, *mem));
        }
    }

    // --- layout -------------------------------------------------------------

    /// Group targets by project, sort groups + members by the active mode, and
    /// flatten to visible entries (collapsing folded groups). Volatile sorts
    /// freeze for `FREEZE` after a keystroke.
    pub(crate) fn compute_entries(&self) -> Vec<Entry> {
        if self.ctl.inbox {
            return self.inbox_entries(unix_now());
        }
        let targets = &self.snapshot.targets;
        let agents = &self.snapshot.agents;
        let mut visible: Vec<usize> = (0..targets.len())
            .filter(|&i| self.matches(&targets[i]))
            .collect();
        // Project view: agents' background services sit beside the agents.
        let services: Vec<usize> = match self.group_by {
            GroupBy::Project => visible
                .iter()
                .copied()
                .filter(|&i| is_agent_service(&targets[i]))
                .collect(),
            GroupBy::Session => Vec::new(),
        };
        visible.retain(|i| !services.contains(i));
        let visible_agents: Vec<usize> = (0..agents.len())
            .filter(|&i| self.matches_agent(&agents[i]))
            .collect();
        if visible.is_empty() && visible_agents.is_empty() && services.is_empty() {
            return Vec::new();
        }
        // working, then idle, then stale; alphabetical within each
        let now = unix_now();
        let rank = |a: &Agent| {
            if a.is_working(now) {
                0
            } else if a.is_stale(now) {
                2
            } else {
                1
            }
        };
        let agent_label = |i: usize| {
            self.agent_session_of
                .get(&agents[i].pid)
                .map(String::as_str)
                .unwrap_or("")
        };
        // Project view: agents fold into one section at the bottom.
        let (visible_agents, section): (Vec<usize>, Vec<usize>) = match self.group_by {
            GroupBy::Session => (visible_agents, Vec::new()),
            GroupBy::Project => (Vec::new(), visible_agents),
        };

        let mut order: Vec<String> = Vec::new();
        let mut groups: HashMap<String, Vec<usize>> = HashMap::new();
        for &i in &visible {
            let p = self.group_key(&targets[i]);
            if !groups.contains_key(p) {
                order.push(p.to_string());
            }
            groups.entry(p.to_string()).or_default().push(i);
        }
        let mut group_agents: HashMap<String, Vec<usize>> = HashMap::new();
        for &i in &visible_agents {
            let Some(g) = self.agent_session_of.get(&agents[i].pid) else {
                continue;
            };
            if !groups.contains_key(g) && !group_agents.contains_key(g) {
                order.push(g.clone());
            }
            group_agents.entry(g.clone()).or_default().push(i);
        }

        let sort_members = |m: &mut Vec<usize>| match self.sort {
            SortMode::Port => m.sort_by(|&a, &b| ord_canonical(&targets[a], &targets[b])),
            SortMode::Cpu => m.sort_by(|&a, &b| {
                targets[b]
                    .cpu_pct
                    .partial_cmp(&targets[a].cpu_pct)
                    .unwrap_or(std::cmp::Ordering::Equal)
            }),
            SortMode::Mem => m.sort_by(|&a, &b| targets[b].mem_bytes.cmp(&targets[a].mem_bytes)),
        };
        let mut grouped: Vec<(String, Vec<usize>)> = order
            .into_iter()
            .map(|p| {
                let mut m = groups.remove(&p).unwrap_or_default();
                sort_members(&mut m);
                (p, m)
            })
            .collect();
        let agents_of = |g: &str| group_agents.get(g).map(Vec::as_slice).unwrap_or_default();
        let agent_cpu = |g: &str| agents_of(g).iter().map(|&i| agents[i].cpu_pct).sum::<f32>();
        let agent_mem = |g: &str| {
            agents_of(g)
                .iter()
                .map(|&i| agents[i].mem_bytes)
                .sum::<u64>()
        };

        match self.sort {
            SortMode::Port => grouped.sort_by(|a, b| {
                let mp = |m: &[usize]| {
                    m.iter()
                        .filter_map(|&i| targets[i].ports.iter().min().copied())
                        .min()
                };
                // port-less groups: live agents before idle and stale ones
                let r = |g: &str| agents_of(g).iter().map(|&i| rank(&agents[i])).min();
                match (mp(&a.1), mp(&b.1)) {
                    (Some(x), Some(y)) => x.cmp(&y),
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    (None, None) => r(&a.0).cmp(&r(&b.0)).then_with(|| a.0.cmp(&b.0)),
                }
            }),
            SortMode::Cpu => grouped.sort_by(|a, b| {
                let s = |g: &(String, Vec<usize>)| {
                    g.1.iter().map(|&i| targets[i].cpu_pct).sum::<f32>() + agent_cpu(&g.0)
                };
                s(b).partial_cmp(&s(a)).unwrap_or(std::cmp::Ordering::Equal)
            }),
            SortMode::Mem => grouped.sort_by(|a, b| {
                let s = |g: &(String, Vec<usize>)| {
                    g.1.iter().map(|&i| targets[i].mem_bytes).sum::<u64>() + agent_mem(&g.0)
                };
                s(b).cmp(&s(a))
            }),
        }

        let mut entries = Vec::new();
        for (project, members) in &grouped {
            let heads = agents_of(project);
            if members.is_empty() {
                // an agent session with no servers: just its row(s)
                entries.extend(heads.iter().map(|&i| Entry::Agent(agents[i].pid)));
            } else if members.len() == 1 && self.group_by == GroupBy::Project {
                entries.push(Entry::Target(targets[members[0]].key.clone()));
            } else {
                entries.push(Entry::Group(project.clone()));
                if !self.collapsed.contains(project) {
                    entries.extend(heads.iter().map(|&i| Entry::Agent(agents[i].pid)));
                    for &i in members {
                        entries.push(Entry::Member(targets[i].key.clone()));
                    }
                }
            }
        }
        // always visible, right above the (folded) sessions they serve
        entries.extend(
            services
                .iter()
                .map(|&i| Entry::Target(targets[i].key.clone())),
        );
        if !section.is_empty() {
            entries.push(Entry::Agents);
            // a filter that hits an agent opens the section
            if self.agents_open || !self.filter.is_empty() {
                let mut section = section;
                section.sort_by(|&a, &b| {
                    rank(&agents[a])
                        .cmp(&rank(&agents[b]))
                        .then_with(|| agent_label(a).cmp(agent_label(b)))
                });
                entries.extend(section.iter().map(|&i| Entry::Agent(agents[i].pid)));
            }
        }

        let frozen = self.sort.is_volatile()
            && self.last_input.elapsed() < FREEZE
            && !self.display.is_empty();
        if !frozen {
            return entries;
        }
        let desired: HashSet<&Entry> = entries.iter().collect();
        let mut order: Vec<Entry> = self
            .display
            .iter()
            .filter(|e| desired.contains(e))
            .cloned()
            .collect();
        for e in entries {
            if !order.contains(&e) {
                order.push(e);
            }
        }
        order
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

fn rect_contains(r: Rect, x: u16, y: u16) -> bool {
    x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height
}

/// The target key behind an entry (`None` for a group header).
fn key_of(e: &Entry) -> Option<&TargetKey> {
    match e {
        Entry::Member(k) | Entry::Target(k) => Some(k),
        Entry::Group(_) | Entry::Agent(_) | Entry::Agents => None,
    }
}

/// Session-view group label per target and per agent pid: `claude·fal-app`,
/// `claude·ended`, `tmux`, `detached`, `unattributed`. A live agent and the
/// servers it launched share a label. Distinct sessions that would share a
/// label get a short id suffix (`claude·fal-app #219a`).
pub(crate) fn session_groups(
    targets: &[Target],
    agents: &[Agent],
) -> (HashMap<TargetKey, String>, HashMap<u32, String>) {
    use crate::launcher::{Launcher, LauncherKind};
    let agent_launchers: Vec<Launcher> = agents.iter().map(Agent::as_launcher).collect();
    let base_and_id = |l: Option<&Launcher>| -> (String, String) {
        let Some(l) = l else {
            return ("unattributed".into(), String::new());
        };
        let id = l
            .session
            .clone()
            .or_else(|| l.pid.map(|p| p.to_string()))
            .unwrap_or_default();
        let base = match l.kind {
            LauncherKind::Detached => "detached".into(),
            LauncherKind::Agent if l.is_orphaned() => format!("{}·ended", l.name),
            LauncherKind::Agent => match l.cwd.as_ref().and_then(|c| c.file_name()) {
                Some(dir) => format!("{}·{}", l.name, dir.to_string_lossy()),
                None => l.name.clone(),
            },
            LauncherKind::Editor | LauncherKind::Terminal => l.name.clone(),
        };
        (base, id)
    };
    let mut ids: HashMap<String, HashSet<String>> = HashMap::new();
    let all = targets
        .iter()
        .map(|t| t.launcher.as_ref())
        .chain(agent_launchers.iter().map(Some));
    for l in all {
        let (base, id) = base_and_id(l);
        ids.entry(base).or_default().insert(id);
    }
    let label = |l: Option<&Launcher>| {
        let (base, id) = base_and_id(l);
        if ids[&base].len() > 1 && !id.is_empty() {
            format!("{base} #{}", crate::launcher::short_id(&id, 4))
        } else {
            base
        }
    };
    let by_target = targets
        .iter()
        .map(|t| (t.key.clone(), label(t.launcher.as_ref())))
        .collect();
    let by_agent = agents
        .iter()
        .zip(&agent_launchers)
        .map(|(a, l)| (a.pid, label(Some(l))))
        .collect();
    (by_target, by_agent)
}

/// One message per agent session that ended between `old` and `new` while its
/// servers kept running: `claude session in ~/dev/app ended — left 2 servers
/// running (480MB)`. Only the alive→orphaned transition counts, so each
/// session is reported once.
pub(crate) fn orphan_notices(old: &Snapshot, new: &Snapshot) -> Vec<String> {
    use crate::launcher::LauncherKind;
    let before: HashMap<&TargetKey, &Target> = old.targets.iter().map(|t| (&t.key, t)).collect();
    // session identity -> (agent name, cwd while alive, targets left behind)
    let mut ended: Vec<(String, String, Option<String>, Vec<&Target>)> = Vec::new();
    for t in &new.targets {
        let Some(l) = t.launcher.as_ref().filter(|l| l.is_orphaned()) else {
            continue;
        };
        let Some(was) = before
            .get(&t.key)
            .and_then(|o| o.launcher.as_ref())
            .filter(|ol| ol.kind == LauncherKind::Agent && ol.alive)
        else {
            continue;
        };
        let id = l
            .session
            .clone()
            .or_else(|| l.pid.map(|p| p.to_string()))
            .unwrap_or_default();
        match ended.iter_mut().find(|e| e.0 == id) {
            Some(e) => e.3.push(t),
            None => ended.push((
                id,
                l.name.clone(),
                was.cwd.as_ref().map(|c| tildify(&c.display().to_string())),
                vec![t],
            )),
        }
    }
    ended
        .into_iter()
        .map(|(_, name, cwd, ts)| {
            let n = ts.len();
            let mem: u64 = ts.iter().map(|t| t.mem_bytes).sum();
            let where_ = cwd.map(|c| format!(" in {c}")).unwrap_or_default();
            format!(
                "{name} session{where_} ended — left {n} server{} running ({})",
                if n == 1 { "" } else { "s" },
                fmt_mem(mem)
            )
        })
        .collect()
}

/// Two entries are "the same selection" if they're the same target (ignoring
/// Member/Target variant), the same group, or the same agent.
pub(crate) fn same_entry(a: &Entry, b: &Entry) -> bool {
    match (key_of(a), key_of(b)) {
        (Some(x), Some(y)) => x == y,
        _ => a == b,
    }
}

/// Canonical order: listeners by port asc, watched after, by project.
fn ord_canonical(a: &Target, b: &Target) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a.ports.first(), b.ports.first()) {
        (Some(x), Some(y)) => x.cmp(y),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => a.project.cmp(&b.project),
    }
}

/// `s` cut to `n` chars with `…`.
fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n - 1).collect::<String>())
    }
}

/// Samples of history kept per row: about two minutes at the 1s tick.
pub(crate) const HISTORY: usize = 120;

/// Whose history: a target, or an agent session by pid.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum HistKey {
    Target(TargetKey),
    Agent(u32),
}

/// A target that is an agent's background service (`opencode serve`), which
/// the project view lists beside the agents section rather than as a project.
pub(crate) fn is_agent_service(t: &Target) -> bool {
    crate::launcher::agent_service(&t.anchor_argv).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn app_with_sample() -> App {
        let mut app = App::new();
        let mut snap = Snapshot::sample();
        snap.seq = 1; // a "real" snapshot, not the initial empty one
        app.apply(Arc::new(snap));
        app
    }

    use crate::launcher::{Launcher, LauncherKind};
    use std::path::PathBuf;

    fn agent(session: &str, cwd: &str, alive: bool) -> Option<Launcher> {
        Some(Launcher {
            kind: LauncherKind::Agent,
            name: "claude".into(),
            pid: Some(100),
            alive,
            session: Some(session.into()),
            cwd: alive.then(|| PathBuf::from(cwd)),
            start_time: 0,
        })
    }

    /// sample(): [0] client-portal :3000, [1] billing-api :8000,
    /// [2] worker :5555, [3] client-portal :5432, [4] design-system watcher.
    fn sessions_snapshot(seq: u64, alive: bool) -> Snapshot {
        let mut snap = Snapshot::sample();
        snap.seq = seq;
        snap.targets[0].launcher = agent("aaaa1111", "/Users/me/dev/portal", alive);
        snap.targets[1].launcher = agent("aaaa1111", "/Users/me/dev/portal", alive);
        snap.targets[2].launcher = agent("bbbb2222", "/Users/me/dev/worker", true);
        snap.targets[3].launcher = Some(Launcher {
            kind: LauncherKind::Terminal,
            name: "tmux".into(),
            pid: Some(7),
            alive: true,
            session: None,
            cwd: None,
            start_time: 0,
        });
        snap
    }

    #[test]
    fn session_groups_label_by_agent_and_dir_and_disambiguate() {
        let snap = sessions_snapshot(1, true);
        let (g, _) = session_groups(&snap.targets, &[]);
        assert_eq!(g[&TargetKey::Port(3000)], "claude·portal");
        assert_eq!(g[&TargetKey::Port(8000)], "claude·portal"); // same session
        assert_eq!(g[&TargetKey::Port(5555)], "claude·worker");
        assert_eq!(g[&TargetKey::Port(5432)], "tmux");
        assert_eq!(g[&snap.targets[4].key], "unattributed");

        // two different sessions in same-named dirs get a short id suffix
        let mut snap = sessions_snapshot(1, true);
        snap.targets[2].launcher = agent("cccc3333", "/Users/me/other/portal", true);
        let (g, _) = session_groups(&snap.targets, &[]);
        assert_eq!(g[&TargetKey::Port(3000)], "claude·portal #aaaa");
        assert_eq!(g[&TargetKey::Port(5555)], "claude·portal #cccc");
    }

    #[test]
    fn session_view_groups_across_projects_and_kills_the_session() {
        let mut app = App::new();
        app.apply(Arc::new(sessions_snapshot(1, true)));
        app.toggle_group_by();
        let entries = app.compute_entries();
        // one session spanning two projects -> one header with both
        assert!(entries
            .iter()
            .any(|e| matches!(e, Entry::Group(g) if g == "claude·portal")));
        // a single-target session still gets a header in session view
        assert!(entries
            .iter()
            .any(|e| matches!(e, Entry::Group(g) if g == "claude·worker")));
        app.selected = Some(Entry::Group("claude·portal".into()));
        let mut ports: Vec<u16> = app
            .selected_targets()
            .iter()
            .flat_map(|t| t.ports.clone())
            .collect();
        ports.sort_unstable();
        assert_eq!(ports, vec![3000, 8000], "K acts on exactly the session");
        // back to project view
        app.toggle_group_by();
        assert!(app
            .compute_entries()
            .iter()
            .any(|e| matches!(e, Entry::Group(p) if p == "client-portal")));
    }

    #[test]
    fn ending_a_session_notifies_once_with_its_servers() {
        let mut app = App::new();
        app.apply(Arc::new(sessions_snapshot(1, true)));
        assert!(app.take_notifications().is_empty());
        app.apply(Arc::new(sessions_snapshot(2, false))); // session aaaa1111 ended
        let n = app.take_notifications();
        assert_eq!(n.len(), 1, "one notice per session, not per server: {n:?}");
        assert!(
            n[0].starts_with(
                "claude session in /Users/me/dev/portal ended — left 2 servers running"
            ),
            "{}",
            n[0]
        );
        assert!(app
            .status
            .as_deref()
            .unwrap_or("")
            .contains("/ended to review"));
        assert_eq!(app.status_ttl, NOTICE_TTL);
        // still orphaned next tick: no repeat
        app.apply(Arc::new(sessions_snapshot(3, false)));
        assert!(app.take_notifications().is_empty());
    }

    #[test]
    fn orphans_already_present_at_startup_get_a_notice_but_no_desktop_ping() {
        let mut app = App::new();
        app.apply(Arc::new(sessions_snapshot(1, false)));
        assert!(app.take_notifications().is_empty());
        assert!(app
            .status
            .as_deref()
            .unwrap_or("")
            .contains("2 servers left running by ended agent sessions"));
    }

    #[test]
    fn agent_servers_sit_beside_the_agents_section() {
        let mut snap = Snapshot::sample();
        snap.seq = 1;
        let mut svc = snap.targets[0].clone();
        svc.key = TargetKey::Port(49374);
        svc.ports = vec![49374];
        svc.project = "opencode".into();
        svc.anchor_argv = vec![
            "/opt/homebrew/bin/opencode".into(),
            "serve".into(),
            "--service".into(),
        ];
        snap.targets.push(svc);
        let mut app = App::new();
        app.apply(Arc::new(snap));
        let e = app.compute_entries();
        // shown even with the section folded, after the projects
        assert_eq!(
            e.last(),
            Some(&Entry::Target(TargetKey::Port(49374))),
            "{e:?}"
        );
        assert!(!e.contains(&Entry::Agents), "no sessions, no section");

        // with the section open, it's listed once, not again inside it
        let mut snap = Snapshot::sample();
        snap.seq = 2;
        snap.targets
            .push(app.snapshot.targets.last().unwrap().clone());
        snap.agents = vec![Agent::sample(7, "aaaa1111", "marina")];
        app.apply(Arc::new(snap));
        app.agents_open = true;
        let e = app.compute_entries();
        let svc = |x: &&Entry| matches!(x, Entry::Target(k) | Entry::Member(k) if *k == TargetKey::Port(49374));
        assert_eq!(e.iter().filter(svc).count(), 1, "{e:?}");
        assert_eq!(e.last(), Some(&Entry::Agent(7)));
    }

    #[test]
    fn session_view_lists_agents_and_verbs_never_reach_them() {
        let mut snap = sessions_snapshot(1, true);
        // pid 100 is the live claude behind session aaaa1111 (:3000, :8000);
        // pid 300 is idle with no servers
        snap.agents = vec![
            Agent::sample(100, "aaaa1111", "portal"),
            Agent::sample(300, "dddd4444", "notes"),
        ];
        let mut app = App::new();
        app.apply(Arc::new(snap));
        // project view: one folded section at the bottom
        let e = app.compute_entries();
        assert_eq!(e.last(), Some(&Entry::Agents));
        assert!(!e.iter().any(|e| matches!(e, Entry::Agent(_))));
        app.selected = Some(Entry::Agents);
        assert!(
            app.selected_targets().is_empty(),
            "K on the section: nothing"
        );
        app.toggle_collapse();
        let e = app.compute_entries();
        let pos = e.iter().position(|x| x == &Entry::Agents).unwrap();
        assert_eq!(e[pos + 1..].len(), 2, "unfolded: both agents follow");
        app.toggle_collapse();
        assert!(!app
            .compute_entries()
            .iter()
            .any(|e| matches!(e, Entry::Agent(_))));

        app.toggle_group_by();
        let entries = app.compute_entries();
        let at = |e: &Entry| entries.iter().position(|x| x == e).unwrap();
        // the agent heads its own group, right under the header
        assert_eq!(
            at(&Entry::Agent(100)),
            at(&Entry::Group("claude·portal".into())) + 1
        );
        // an idle agent is a row of its own, no header
        assert!(entries.contains(&Entry::Agent(300)));
        assert!(!entries
            .iter()
            .any(|e| matches!(e, Entry::Group(g) if g == "claude·notes")));
        assert_eq!(app.group_agents("claude·portal").len(), 1);

        // K on the header: its servers, never the agent
        app.selected = Some(Entry::Group("claude·portal".into()));
        let pids: Vec<u32> = app
            .selected_targets()
            .iter()
            .flat_map(|t| t.ports.clone())
            .map(u32::from)
            .collect();
        assert!(!pids.is_empty() && !pids.contains(&100));
        // on the agent row: nothing at all
        app.selected = Some(Entry::Agent(100));
        assert!(app.selected_targets().is_empty());
        assert_eq!(app.selected_agent().map(|a| a.pid), Some(100));
        assert_eq!(app.selection_label(), "claude session");

        // `/agent` lists every session; a shell's command finds its agent
        app.filter = "agent".into();
        assert!(app.compute_entries().contains(&Entry::Agent(300)));
        app.filter = "cargo test".into();
        let e = app.compute_entries();
        assert!(e.contains(&Entry::Agent(300)) && e.contains(&Entry::Agent(100)));
    }

    #[test]
    fn agents_finishing_or_blocking_are_announced_once() {
        use crate::hosts::{HostInfo, HostKind, HostStatus};
        let snap_with = |seq: u64, working: bool, status: Option<HostStatus>| {
            let mut snap = Snapshot::sample();
            snap.seq = seq;
            let mut a = Agent::sample(100, "s1", "api");
            if !working {
                a.children.clear(); // no shell; the host decides
            }
            a.host = Some(HostInfo {
                kind: HostKind::Herdr,
                target: "w1:p1".into(),
                status,
                title: None,
                session: None,
                app_pid: None,
                outer: None,
            });
            snap.agents = vec![a];
            Arc::new(snap)
        };
        let mut app = App::new();
        app.apply(snap_with(1, true, Some(HostStatus::Working)));
        let t0 = 1_000_000;
        // working for two minutes...
        app.snapshot = snap_with(2, true, Some(HostStatus::Working));
        assert!(app.agent_notices(t0).is_empty());
        assert!(app.agent_notices(t0 + 120).is_empty());
        // ...then quiet: nothing until the quiet period passes
        app.snapshot = snap_with(3, false, Some(HostStatus::Idle));
        assert!(
            app.agent_notices(t0 + 125).is_empty(),
            "a pause, not done yet"
        );
        let n = app.agent_notices(t0 + 135);
        assert_eq!(n, vec!["claude·api finished after 2m".to_string()]);
        assert!(app.agent_notices(t0 + 200).is_empty(), "once");

        // a short burst isn't worth a ping
        app.snapshot = snap_with(4, true, Some(HostStatus::Working));
        app.agent_notices(t0 + 300);
        app.snapshot = snap_with(5, false, Some(HostStatus::Idle));
        assert!(app.agent_notices(t0 + 320).is_empty());

        // blocked: right away, once
        app.snapshot = snap_with(6, false, Some(HostStatus::Blocked));
        assert_eq!(
            app.agent_notices(t0 + 400),
            vec!["claude·api needs you".to_string()]
        );
        assert!(app.agent_notices(t0 + 410).is_empty());
    }

    #[test]
    fn multi_target_project_gets_a_group_single_does_not() {
        let entries = app_with_sample().compute_entries();
        // client-portal (next dev + postgres) -> a group header
        assert!(entries
            .iter()
            .any(|e| matches!(e, Entry::Group(p) if p == "client-portal")));
        // billing-api has one target -> a standalone row, no header
        assert!(!entries
            .iter()
            .any(|e| matches!(e, Entry::Group(p) if p == "billing-api")));
        assert!(
            entries
                .iter()
                .filter(|e| matches!(e, Entry::Member(_)))
                .count()
                >= 2
        );
    }

    #[test]
    fn collapsing_a_group_hides_its_members() {
        let mut app = app_with_sample();
        app.collapsed.insert("client-portal".into());
        let entries = app.compute_entries();
        assert!(entries
            .iter()
            .any(|e| matches!(e, Entry::Group(p) if p == "client-portal")));
        assert_eq!(
            entries
                .iter()
                .filter(|e| matches!(e, Entry::Member(_)))
                .count(),
            0
        );
    }

    #[test]
    fn group_selection_targets_all_members() {
        let mut app = app_with_sample();
        app.selected = Some(Entry::Group("client-portal".into()));
        assert_eq!(app.selected_targets().len(), 2);
        assert_eq!(app.selection_label(), "client-portal");
    }

    #[test]
    fn move_selection_wraps_around() {
        let mut app = app_with_sample();
        app.reconcile();
        assert!(app.display.len() > 1);
        app.selected = Some(app.display[0].clone());
        app.select_prev(); // wraps to last
        assert_eq!(app.selected.as_ref(), app.display.last());
    }

    #[test]
    fn vanished_selection_falls_to_nearest_neighbor_not_top() {
        let mut app = app_with_sample();
        app.reconcile();
        let last = app.display.len() - 1;
        app.jump_bottom();
        // the selected (last) row vanishes: keep a snapshot without it
        let victim = app.display[last].clone();
        let mut snap = Snapshot::sample();
        snap.seq = 2;
        snap.targets.retain(|t| match &victim {
            Entry::Target(k) | Entry::Member(k) => &t.key != k,
            Entry::Group(p) => &t.project != p,
            Entry::Agent(_) | Entry::Agents => true,
        });
        app.apply(Arc::new(snap));
        app.reconcile();
        // cursor stays at the end of the (shorter) list — not index 0
        assert_eq!(app.table_state.selected(), Some(app.display.len() - 1));
        assert!(app.display.len() > 1, "test needs remaining rows");
    }

    #[test]
    fn cpu_sort_orders_busiest_first() {
        let mut app = app_with_sample();
        app.sort = SortMode::Cpu;
        let entries = app.compute_entries();
        // client-portal's aggregate cpu (3.4 + 0.2) is highest -> its header first
        assert!(matches!(&entries[0], Entry::Group(p) if p == "client-portal"));
    }

    #[test]
    fn selection_matches_target_and_member_by_key() {
        let k = TargetKey::Port(3000);
        assert!(same_entry(
            &Entry::Target(k.clone()),
            &Entry::Member(k.clone())
        ));
        assert!(!same_entry(
            &Entry::Group("a".into()),
            &Entry::Group("b".into())
        ));
    }

    #[test]
    fn sort_change_bypasses_the_navigation_freeze() {
        let mut app = app_with_sample();
        app.last_input = Instant::now(); // pretend we just navigated (freeze active)
        app.cycle_sort();
        // the new sort must apply immediately, not after the freeze window
        assert!(app.last_input.elapsed() >= FREEZE);
    }

    #[test]
    fn escape_clears_a_committed_filter() {
        let mut app = app_with_sample();
        app.filter = "billing".into(); // committed (not in input mode)
        app.escape();
        assert!(app.filter.is_empty(), "Esc must clear a committed filter");
        // and with a log pane open, Esc closes that first
    }

    #[test]
    fn escape_closes_panes_before_clearing_filter() {
        let mut app = app_with_sample();
        app.inspect = true;
        app.filter = "x".into();
        app.escape();
        assert!(!app.inspect);
        assert_eq!(app.filter, "x"); // untouched on the first Esc
        app.escape();
        assert!(app.filter.is_empty());
    }

    #[test]
    fn filter_also_matches_cwd_and_branch() {
        let app = app_with_sample();
        let t = &app.snapshot.targets[0].clone();
        let mut a = app;
        a.filter = "users/dev".into(); // cwd substring (case-insensitive)
        assert!(a.matches(t));
        a.filter = "main".into(); // branch
        assert!(a.matches(t));
        a.filter = "nope-xyz".into();
        assert!(!a.matches(t));
    }

    #[test]
    fn undo_cancels_a_pending_kill() {
        let mut app = App::new();
        let flag = Arc::new(AtomicBool::new(false));
        app.note_pending_kill(Arc::clone(&flag), "web", vec![TargetKey::Port(1)]);
        assert!(app.is_dying(&TargetKey::Port(1)));
        app.undo_kill();
        assert!(flag.load(Ordering::SeqCst)); // escalation cancelled
        app.undo_kill(); // nothing left
        assert_eq!(app.status.as_deref(), Some("nothing to undo"));
    }

    #[test]
    fn status_expires_after_ttl() {
        let mut app = App::new();
        app.set_status("hi");
        assert!(app.status.is_some());
        app.status_at = Instant::now() - STATUS_TTL - Duration::from_secs(1);
        app.tick();
        assert!(app.status.is_none());
    }

    #[test]
    fn unexpected_exit_is_reported_but_marina_kills_are_not() {
        let mut app = App::new();
        let old_uptime = unix_now() - 3600; // ran for an hour
        let mut first = Snapshot::sample();
        first.seq = 1;
        for t in &mut first.targets {
            t.anchor.start_time = old_uptime; // old enough to count as a crash
        }
        app.apply(Arc::new(first));
        // next snapshot: billing-api (:8000) is gone
        let mut gone = Snapshot::sample();
        gone.seq = 2;
        for t in &mut gone.targets {
            t.anchor.start_time = old_uptime;
        }
        gone.targets.retain(|t| t.project != "billing-api");
        app.apply(Arc::new(gone));
        assert!(
            app.status.as_deref().unwrap_or("").contains("billing-api"),
            "an unexpected exit must be reported; got {:?}",
            app.status
        );

        // now client-portal vanishes, but marina killed it -> no report
        app.status = None;
        let keys: Vec<TargetKey> = app
            .snapshot
            .targets
            .iter()
            .filter(|t| t.project == "client-portal")
            .map(|t| t.key.clone())
            .collect();
        app.mark_marina_action(&keys);
        let mut gone2 = Snapshot::sample();
        gone2.seq = 3;
        for t in &mut gone2.targets {
            t.anchor.start_time = old_uptime;
        }
        gone2
            .targets
            .retain(|t| t.project != "billing-api" && t.project != "client-portal");
        app.apply(Arc::new(gone2));
        assert!(
            app.status.is_none(),
            "a marina-initiated kill must not be reported as a crash; got {:?}",
            app.status
        );
    }

    #[test]
    fn new_rows_flash_and_expire() {
        let mut app = app_with_sample();
        // add a target in the next snapshot
        let mut snap = Snapshot::sample();
        snap.seq = 2;
        let mut extra = snap.targets[0].clone();
        extra.key = TargetKey::Port(9999);
        extra.ports = vec![9999];
        extra.project = "newcomer".into();
        snap.targets.push(extra);
        app.apply(Arc::new(snap));
        assert!(app.is_flashing(&TargetKey::Port(9999)));
        assert!(app.has_transient());
        // age it out
        *app.appeared.get_mut(&TargetKey::Port(9999)).unwrap() =
            Instant::now() - FLASH - Duration::from_millis(1);
        app.tick();
        assert!(!app.is_flashing(&TargetKey::Port(9999)));
    }

    #[test]
    fn header_click_sorts_and_wheel_moves() {
        use ratatui::crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        let mut app = app_with_sample();
        app.reconcile();
        app.table_rect = Rect::new(0, 0, 100, 20);
        app.header_hits = vec![(50, 57, SortMode::Cpu)];
        app.on_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 52,
            row: 1,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.sort, SortMode::Cpu);
        // wheel over the table moves the selection
        let before = app.selected.clone();
        app.on_mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 10,
            row: 5,
            modifiers: KeyModifiers::NONE,
        });
        assert_ne!(app.selected, before);
    }

    #[test]
    fn click_selects_a_row() {
        use ratatui::crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        let mut app = app_with_sample();
        app.reconcile();
        app.table_rect = Rect::new(0, 0, 100, 20);
        // row 0 of the list renders at y = table.y + 2
        app.on_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: 2,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.selected.as_ref(), app.display.first());
    }

    #[test]
    fn log_scroll_pins_and_resumes_follow() {
        let (_tx, rx) = std::sync::mpsc::channel();
        let mut app = App::new();
        app.open_log("test".into(), rx, Arc::new(AtomicBool::new(false)));
        {
            let l = app.log.as_mut().unwrap();
            for i in 0..100 {
                l.lines.push_back(format!("line {i}"));
            }
        }
        app.log_scroll_up(5);
        let pinned = app.log.as_ref().unwrap().scroll;
        assert!(pinned.is_some());
        app.log_scroll_down(500); // way past the bottom -> resume follow
        assert!(app.log.as_ref().unwrap().scroll.is_none());
    }

    #[test]
    fn sampler_death_sets_a_persistent_warning() {
        let mut app = App::new();
        app.sampler_died();
        assert!(app.data_warning.as_deref().unwrap().contains("sampler"));
        app.dirty = false;
        app.sampler_died(); // idempotent — no redraw churn
        assert!(!app.dirty);
    }
}
