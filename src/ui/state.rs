//! View state lives only here and survives snapshot swaps. Targets are grouped
//! by project: a project with several targets gets a collapsible header and a
//! group-kill; a lone target renders as a plain row. Selection is held by value
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

use super::format::unix_now;
use crate::model::{Snapshot, Target, TargetKey};

pub(crate) const FREEZE: Duration = Duration::from_secs(2);
pub(crate) const STATUS_TTL: Duration = Duration::from_secs(4);
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

/// A visible row: a project group header, a member of an (expanded) group, or a
/// standalone single-target project.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Entry {
    Group(String),
    Member(TargetKey),
    Target(TargetKey),
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
        }
    }

    // --- snapshot ingestion -------------------------------------------------

    /// Swap in a fresh snapshot; diff it against the old one to flash new rows
    /// and to notice targets that vanished *without* marina killing them.
    pub fn apply(&mut self, snap: Arc<Snapshot>) {
        let old = std::mem::replace(&mut self.snapshot, snap);
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
        }
        self.data_warning = self.snapshot.error.clone();
        self.dirty = true;
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
        if self.status.is_some() && self.status_at.elapsed() > STATUS_TTL {
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
        self.dirty = true;
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

    /// All targets the selection acts on: a whole group, or one target.
    pub fn selected_targets(&self) -> Vec<&Target> {
        match &self.selected {
            Some(Entry::Group(p)) => self
                .snapshot
                .targets
                .iter()
                .filter(|t| &t.project == p)
                .collect(),
            Some(Entry::Member(k)) | Some(Entry::Target(k)) => self
                .snapshot
                .targets
                .iter()
                .filter(|t| &t.key == k)
                .collect(),
            None => Vec::new(),
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
    }

    /// Esc in normal mode: close the most intrusive thing first —
    /// log pane, then inspect panel, then a committed filter.
    pub fn escape(&mut self) {
        if self.log.is_some() {
            self.close_log();
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
                            if matches!(e, Entry::Group(_)) {
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
                } else {
                    self.select_prev();
                }
            }
            MouseEventKind::ScrollDown => {
                if self.log_rect.is_some_and(|r| rect_contains(r, x, y)) {
                    self.log_scroll_down(3);
                } else {
                    self.select_next();
                }
            }
            _ => {}
        }
    }

    // --- layout -------------------------------------------------------------

    /// Group targets by project, sort groups + members by the active mode, and
    /// flatten to visible entries (collapsing folded groups). Volatile sorts
    /// freeze for `FREEZE` after a keystroke.
    pub(crate) fn compute_entries(&self) -> Vec<Entry> {
        let targets = &self.snapshot.targets;
        let visible: Vec<usize> = (0..targets.len())
            .filter(|&i| self.matches(&targets[i]))
            .collect();
        if visible.is_empty() {
            return Vec::new();
        }

        let mut order: Vec<String> = Vec::new();
        let mut groups: HashMap<String, Vec<usize>> = HashMap::new();
        for &i in &visible {
            let p = &targets[i].project;
            if !groups.contains_key(p) {
                order.push(p.clone());
            }
            groups.entry(p.clone()).or_default().push(i);
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
                let mut m = groups.remove(&p).unwrap();
                sort_members(&mut m);
                (p, m)
            })
            .collect();

        match self.sort {
            SortMode::Port => grouped.sort_by(|a, b| {
                let mp = |m: &[usize]| {
                    m.iter()
                        .filter_map(|&i| targets[i].ports.iter().min().copied())
                        .min()
                };
                match (mp(&a.1), mp(&b.1)) {
                    (Some(x), Some(y)) => x.cmp(&y),
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    (None, None) => a.0.cmp(&b.0),
                }
            }),
            SortMode::Cpu => grouped.sort_by(|a, b| {
                let s = |m: &[usize]| m.iter().map(|&i| targets[i].cpu_pct).sum::<f32>();
                s(&b.1)
                    .partial_cmp(&s(&a.1))
                    .unwrap_or(std::cmp::Ordering::Equal)
            }),
            SortMode::Mem => grouped.sort_by(|a, b| {
                let s = |m: &[usize]| m.iter().map(|&i| targets[i].mem_bytes).sum::<u64>();
                s(&b.1).cmp(&s(&a.1))
            }),
        }

        let mut entries = Vec::new();
        for (project, members) in &grouped {
            if members.len() == 1 {
                entries.push(Entry::Target(targets[members[0]].key.clone()));
            } else {
                entries.push(Entry::Group(project.clone()));
                if !self.collapsed.contains(project) {
                    for &i in members {
                        entries.push(Entry::Member(targets[i].key.clone()));
                    }
                }
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
        Entry::Group(_) => None,
    }
}

/// Two entries are "the same selection" if they're the same target (ignoring
/// Member/Target variant) or the same group.
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
