//! Ratatui view: view state + rendering. View state lives only here and
//! survives snapshot swaps. Targets are grouped by project: a project with
//! several targets gets a collapsible header and a group-kill; a lone target
//! renders as a plain row. Selection is held by value (`Entry`), not index, so
//! the cursor doesn't jump when data refreshes; volatile sorts freeze briefly
//! after a keystroke so rows don't slide out from under it.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ratatui::{
    layout::{Constraint, Layout},
    style::{Color, Modifier, Style},
    text::Line,
    widgets::{Block, Cell, Paragraph, Row, Table, TableState},
    Frame,
};

use crate::model::{Snapshot, Target, TargetKey, TargetKind};

const FREEZE: Duration = Duration::from_secs(2);
const STATUS_TTL: Duration = Duration::from_secs(4);

#[derive(Clone, Copy, PartialEq)]
enum SortMode {
    Port,
    Cpu,
    Mem,
}

impl SortMode {
    fn label(self) -> &'static str {
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
enum Entry {
    Group(String),
    Member(TargetKey),
    Target(TargetKey),
}

struct LogView {
    path: PathBuf,
    lines: VecDeque<String>,
    rx: Receiver<String>,
    stop: Arc<AtomicBool>,
}

impl LogView {
    fn close(self) {
        self.stop.store(true, Ordering::Relaxed); // end the tail thread
    }
}

/// A kill whose SIGKILL escalation can still be cancelled (`u`).
struct PendingKill {
    cancel: Arc<AtomicBool>,
    until: Instant,
    project: String,
}

pub struct App {
    snapshot: Arc<Snapshot>,
    selected: Option<Entry>,
    sort: SortMode,
    display: Vec<Entry>,
    collapsed: HashSet<String>,
    last_input: Instant,
    table_state: TableState,
    status: Option<String>,
    status_at: Instant,
    log: Option<LogView>,
    pending_kill: Option<PendingKill>,
    filter: String,
    filter_active: bool,
}

impl App {
    pub fn new() -> Self {
        App {
            snapshot: Arc::new(Snapshot::empty()),
            selected: None,
            sort: SortMode::Port,
            display: Vec::new(),
            collapsed: HashSet::new(),
            last_input: Instant::now(),
            table_state: TableState::default(),
            status: None,
            status_at: Instant::now(),
            log: None,
            pending_kill: None,
            filter: String::new(),
            filter_active: false,
        }
    }

    pub fn apply(&mut self, snap: Arc<Snapshot>) {
        self.snapshot = snap; // selection is validated against `display` in render
    }

    pub fn set_status(&mut self, s: impl Into<String>) {
        self.status = Some(s.into());
        self.status_at = Instant::now();
    }

    /// Clear a stale status line so it doesn't linger (call each loop tick).
    pub fn expire_status(&mut self) {
        if self.status.is_some() && self.status_at.elapsed() > STATUS_TTL {
            self.status = None;
        }
    }

    pub fn cycle_sort(&mut self) {
        self.sort = self.sort.next();
        self.last_input = Instant::now();
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
        if self.display.is_empty() {
            return;
        }
        let cur = self
            .selected
            .as_ref()
            .and_then(|s| self.display.iter().position(|e| same_entry(e, s)))
            .unwrap_or(0) as isize;
        let next = (cur + delta).rem_euclid(self.display.len() as isize) as usize;
        self.selected = Some(self.display[next].clone());
    }
    pub fn jump_top(&mut self) {
        self.last_input = Instant::now();
        self.selected = self.display.first().cloned();
    }
    pub fn jump_bottom(&mut self) {
        self.last_input = Instant::now();
        self.selected = self.display.last().cloned();
    }

    pub fn toggle_collapse(&mut self) {
        if let Some(Entry::Group(p)) = &self.selected {
            let p = p.clone();
            if !self.collapsed.remove(&p) {
                self.collapsed.insert(p);
            }
        }
    }

    fn is_group_selected(&self) -> bool {
        matches!(self.selected, Some(Entry::Group(_)))
    }

    /// The single target under the cursor (a member or standalone) — `None` on a
    /// group header.
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

    // --- kill undo ----------------------------------------------------------

    pub fn note_pending_kill(&mut self, cancel: Arc<AtomicBool>, project: &str) {
        self.pending_kill = Some(PendingKill {
            cancel,
            until: Instant::now() + Duration::from_secs(4),
            project: project.to_string(),
        });
    }
    pub fn undo_kill(&mut self) {
        match self.pending_kill.take() {
            Some(pk) if Instant::now() < pk.until => {
                pk.cancel.store(true, Ordering::SeqCst);
                self.set_status(format!("undo — cancelled SIGKILL for {}", pk.project));
            }
            _ => self.set_status("nothing to undo"),
        }
    }
    pub fn expire_pending(&mut self) {
        if let Some(pk) = &self.pending_kill {
            if Instant::now() >= pk.until {
                self.pending_kill = None;
            }
        }
    }

    // --- filter (`/`) -------------------------------------------------------

    pub fn is_filtering(&self) -> bool {
        self.filter_active
    }
    pub fn start_filter(&mut self) {
        self.filter_active = true;
        self.last_input = Instant::now();
    }
    pub fn filter_push(&mut self, c: char) {
        self.filter.push(c);
        self.last_input = Instant::now();
    }
    pub fn filter_backspace(&mut self) {
        self.filter.pop();
        self.last_input = Instant::now();
    }
    pub fn filter_commit(&mut self) {
        self.filter_active = false;
    }
    pub fn filter_cancel(&mut self) {
        self.filter_active = false;
        self.filter.clear();
    }
    fn matches(&self, t: &Target) -> bool {
        if self.filter.is_empty() {
            return true;
        }
        let q = self.filter.to_lowercase();
        t.project.to_lowercase().contains(&q)
            || t.command_label.to_lowercase().contains(&q)
            || t.ports.iter().any(|p| p.to_string().contains(&q))
    }

    // --- log pane (`T`) -----------------------------------------------------

    pub fn toggle_log(&mut self) {
        if let Some(old) = self.log.take() {
            old.close();
            self.set_status("closed logs");
            return;
        }
        let Some((pids, name, cwd)) = self
            .selected_target()
            .map(|t| (t.pids.clone(), t.project.clone(), t.cwd.clone()))
        else {
            if self.is_group_selected() {
                self.set_status("select a service to tail (not a group)");
            }
            return;
        };
        match crate::logs::discover(&pids, &cwd, &name) {
            Some(path) => {
                let stop = Arc::new(AtomicBool::new(false));
                let rx = crate::logs::tail(path.clone(), Arc::clone(&stop));
                self.set_status(format!(
                    "tailing {name} · {}",
                    tildify(&path.display().to_string())
                ));
                self.log = Some(LogView {
                    path,
                    lines: VecDeque::new(),
                    rx,
                    stop,
                });
            }
            None => self.set_status(format!(
                "no log file found for {name} (logs may go to stdout)"
            )),
        }
    }
    pub fn close_log(&mut self) {
        if let Some(old) = self.log.take() {
            old.close();
            self.set_status("closed logs");
        }
    }
    pub fn pump_log(&mut self) {
        if let Some(l) = &mut self.log {
            while let Ok(line) = l.rx.try_recv() {
                l.lines.push_back(line);
                if l.lines.len() > 2000 {
                    l.lines.pop_front();
                }
            }
        }
    }

    // --- layout -------------------------------------------------------------

    /// Group targets by project, sort groups + members by the active mode, and
    /// flatten to visible entries (collapsing folded groups). Volatile sorts
    /// freeze for `FREEZE` after a keystroke.
    fn compute_entries(&self) -> Vec<Entry> {
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

pub fn render(frame: &mut Frame, app: &mut App) {
    app.display = app.compute_entries();
    // Re-find the selection by underlying key (so a target staying selected
    // survives becoming/leaving a group member), else fall to the first row.
    let idx = app
        .selected
        .as_ref()
        .and_then(|s| app.display.iter().position(|e| same_entry(e, s)))
        .or(if app.display.is_empty() {
            None
        } else {
            Some(0)
        });
    app.selected = idx.map(|i| app.display[i].clone());
    app.table_state.select(idx);

    let log_open = app.log.is_some();
    let constraints: Vec<Constraint> = if log_open {
        vec![
            Constraint::Min(0),
            Constraint::Length(12),
            Constraint::Length(1),
            Constraint::Length(1),
        ]
    } else {
        vec![
            Constraint::Min(0),
            Constraint::Length(1),
            Constraint::Length(1),
        ]
    };
    let chunks = Layout::vertical(constraints).split(frame.area());
    let table_area = chunks[0];
    let (log_area, detail_area, footer_area) = if log_open {
        (Some(chunks[1]), chunks[2], chunks[3])
    } else {
        (None, chunks[1], chunks[2])
    };

    let title = format!(
        " marina · {} targets · sort:{} ",
        app.snapshot.targets.len(),
        app.sort.label()
    );
    let block = Block::bordered().title(title);

    if app.display.is_empty() {
        let dim = Style::new().fg(Color::DarkGray);
        let msg = if app.snapshot.targets.is_empty() {
            "No dev processes detected."
        } else {
            "No targets match the filter."
        };
        let hint = Paragraph::new(vec![
            Line::from(""),
            Line::from(msg).centered(),
            Line::styled(
                "Start a dev server (npm run dev · cargo run · uvicorn · rails s) and it'll appear here.",
                dim,
            )
            .centered(),
        ])
        .block(block);
        frame.render_widget(hint, table_area);
    } else {
        let header = Row::new(["PROJECT", "COMMAND", "PORT", "CPU", "MEM", "UP", "BRANCH"])
            .style(Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD));
        let by_key: HashMap<&TargetKey, &Target> =
            app.snapshot.targets.iter().map(|t| (&t.key, t)).collect();
        let rows: Vec<Row> = app
            .display
            .iter()
            .map(|e| entry_row(e, &app.snapshot.targets, &by_key, &app.collapsed))
            .collect();
        let widths = [
            Constraint::Length(22),
            Constraint::Length(16),
            Constraint::Length(7),
            Constraint::Length(7),
            Constraint::Length(9),
            Constraint::Length(6),
            Constraint::Length(15),
        ];
        let table = Table::new(rows, widths)
            .header(header)
            .block(block)
            .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED))
            .highlight_symbol("▌ ");
        frame.render_stateful_widget(table, table_area, &mut app.table_state);
    }

    let detail = detail_line(app);
    frame.render_widget(detail, detail_area);

    if let (Some(area), Some(l)) = (log_area, app.log.as_ref()) {
        let inner_h = area.height.saturating_sub(2) as usize;
        let start = l.lines.len().saturating_sub(inner_h);
        let text: Vec<Line> = l
            .lines
            .iter()
            .skip(start)
            .map(|s| Line::from(s.clone()))
            .collect();
        let title = format!(
            " logs: {} — Esc to close ",
            tildify(&l.path.display().to_string())
        );
        let para = Paragraph::new(text).block(Block::bordered().title(title));
        frame.render_widget(para, area);
    }

    let footer = if app.is_filtering() {
        Line::styled(
            format!("  /{}▏  (Enter: apply · Esc: clear)", app.filter),
            Style::new().fg(Color::Yellow),
        )
    } else if let Some(s) = &app.status {
        Line::styled(format!("  {s}"), Style::new().fg(Color::Yellow))
    } else if !app.filter.is_empty() {
        Line::styled(
            format!("  [filter: {}] · / edit · Esc clear · q quit", app.filter),
            Style::new().fg(Color::DarkGray),
        )
    } else {
        Line::styled(
            "  j/k move · Enter fold · g/G · / filter · s sort · K kill · R restart · T tail · Y copy · O open · q",
            Style::new().fg(Color::DarkGray),
        )
    };
    frame.render_widget(footer, footer_area);
}

fn detail_line(app: &App) -> Line<'static> {
    match &app.selected {
        Some(Entry::Group(p)) => {
            let members: Vec<&Target> = app
                .snapshot
                .targets
                .iter()
                .filter(|t| &t.project == p)
                .collect();
            let cpu: f32 = members
                .iter()
                .filter(|t| !t.pids.is_empty())
                .map(|t| t.cpu_pct)
                .sum();
            let mem: u64 = members
                .iter()
                .filter(|t| !t.pids.is_empty())
                .map(|t| t.mem_bytes)
                .sum();
            Line::styled(
                format!(
                    "  {} · {} services · {:.1}% · {} · K kills all",
                    p,
                    members.len(),
                    cpu,
                    fmt_mem(mem)
                ),
                Style::new().fg(Color::Gray),
            )
        }
        _ => match app.selected_target() {
            Some(t) => {
                let url = t.url.as_ref().map(|u| u.value.as_str()).unwrap_or("—");
                let branch = t.git_branch.as_deref().unwrap_or("—");
                Line::styled(
                    format!(
                        "  {} · {} · {} · {} · {} · pids:{}",
                        t.project,
                        t.command_label,
                        url,
                        tildify(&t.cwd.display().to_string()),
                        branch,
                        t.pids.len()
                    ),
                    Style::new().fg(Color::Gray),
                )
            }
            None => Line::from(""),
        },
    }
}

fn entry_row(
    entry: &Entry,
    targets: &[Target],
    by_key: &HashMap<&TargetKey, &Target>,
    collapsed: &HashSet<String>,
) -> Row<'static> {
    match entry {
        Entry::Group(p) => {
            let members: Vec<&Target> = targets.iter().filter(|t| &t.project == p).collect();
            let cpu: f32 = members
                .iter()
                .filter(|t| !t.pids.is_empty())
                .map(|t| t.cpu_pct)
                .sum();
            let mem: u64 = members
                .iter()
                .filter(|t| !t.pids.is_empty())
                .map(|t| t.mem_bytes)
                .sum();
            let arrow = if collapsed.contains(p) { "▸" } else { "▾" };
            Row::new(vec![
                Cell::from(format!("{arrow} {p} ({})", members.len())),
                Cell::from(""),
                Cell::from(""),
                Cell::from(format!("{cpu:.1}%")),
                Cell::from(fmt_mem(mem)),
                Cell::from(""),
                Cell::from(""),
            ])
            .style(Style::new().fg(Color::White).add_modifier(Modifier::BOLD))
        }
        Entry::Member(k) => match by_key.get(k) {
            Some(t) => target_row(t, true),
            None => Row::new(Vec::<Cell>::new()),
        },
        Entry::Target(k) => match by_key.get(k) {
            Some(t) => target_row(t, false),
            None => Row::new(Vec::<Cell>::new()),
        },
    }
}

fn target_row(t: &Target, indent: bool) -> Row<'static> {
    let port = match t.ports.first() {
        Some(p) => format!(":{p}"),
        None => "—".into(),
    };
    let project = if indent {
        "  ↳".to_string()
    } else {
        match t.kind {
            TargetKind::Watched => format!("{} ·watch", t.project),
            TargetKind::Listener => t.project.clone(),
        }
    };
    let (cpu, mem) = if t.pids.is_empty() {
        ("—".to_string(), "—".to_string())
    } else {
        (format!("{:.1}%", t.cpu_pct), fmt_mem(t.mem_bytes))
    };
    let row = Row::new(vec![
        Cell::from(project),
        Cell::from(t.command_label.clone()),
        Cell::from(port),
        Cell::from(cpu),
        Cell::from(mem),
        Cell::from(fmt_uptime(t.anchor.start_time)),
        Cell::from(t.git_branch.clone().unwrap_or_else(|| "—".into())),
    ]);
    if is_infra(&t.command_label) {
        row.style(Style::new().fg(Color::DarkGray))
    } else {
        row
    }
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
fn same_entry(a: &Entry, b: &Entry) -> bool {
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

fn is_infra(label: &str) -> bool {
    matches!(
        label,
        "postgres" | "redis" | "mysql" | "mongodb" | "memcached"
    )
}

fn fmt_uptime(start: u64) -> String {
    if start == 0 {
        return "—".into();
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let s = now.saturating_sub(start);
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else if s < 86_400 {
        format!("{}h", s / 3600)
    } else {
        format!("{}d", s / 86_400)
    }
}

fn fmt_mem(bytes: u64) -> String {
    const MB: u64 = 1024 * 1024;
    const GB: u64 = 1024 * MB;
    if bytes >= GB {
        format!("{:.1}GB", bytes as f64 / GB as f64)
    } else {
        format!("{}MB", bytes / MB)
    }
}

fn tildify(path: &str) -> String {
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && path.starts_with(&home) => {
            format!("~{}", &path[home.len()..])
        }
        _ => path.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    fn app_with_sample() -> App {
        let mut app = App::new();
        app.apply(Arc::new(Snapshot::sample())); // client-portal has 2 targets
        app
    }

    /// Render the app to an off-screen buffer and flatten it to text.
    fn render_to_string(app: &mut App, w: u16, h: u16) -> String {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| render(f, app)).unwrap();
        let buf = term.backend().buffer();
        let mut s = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                if let Some(cell) = buf.cell((x, y)) {
                    s.push_str(cell.symbol());
                }
            }
            s.push('\n');
        }
        s
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
    fn render_empty_shows_hint() {
        let out = render_to_string(&mut App::new(), 100, 12);
        assert!(out.contains("No dev processes detected"));
    }

    #[test]
    fn render_sample_shows_group_header_members_and_chrome() {
        let out = render_to_string(&mut app_with_sample(), 120, 20);
        assert!(out.contains("client-portal")); // group header
        assert!(out.contains("next dev") && out.contains("postgres")); // members
        assert!(out.contains("billing-api") && out.contains(":3000")); // a single + a port
        assert!(out.contains("PROJECT") && out.contains("COMMAND")); // header row
        assert!(out.contains("j/k move")); // footer keymap
        assert!(out.contains('▾')); // expanded-group arrow
    }

    #[test]
    fn render_collapsed_group_hides_members() {
        let mut app = app_with_sample();
        app.collapsed.insert("client-portal".into());
        let out = render_to_string(&mut app, 120, 20);
        assert!(out.contains('▸')); // collapsed arrow
        assert!(!out.contains("next dev")); // member hidden
    }

    #[test]
    fn render_filter_shows_only_matches() {
        let mut app = app_with_sample();
        app.filter = "billing".into();
        let out = render_to_string(&mut app, 120, 20);
        assert!(out.contains("billing-api"));
        assert!(!out.contains("next dev"));
    }

    #[test]
    fn move_selection_wraps_around() {
        let mut app = app_with_sample();
        let _ = render_to_string(&mut app, 120, 20); // populate display
        assert!(app.display.len() > 1);
        app.selected = Some(app.display[0].clone());
        app.select_prev(); // wraps to last
        assert_eq!(app.selected.as_ref(), app.display.last());
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
    fn undo_cancels_a_pending_kill() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let mut app = App::new();
        let flag = Arc::new(AtomicBool::new(false));
        app.note_pending_kill(Arc::clone(&flag), "web");
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
        app.expire_status();
        assert!(app.status.is_none());
    }
}
