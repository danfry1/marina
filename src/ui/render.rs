//! Drawing from `(snapshot + view state)`. The only state this writes back is
//! layout bookkeeping for mouse hit-testing (rects + header hit zones) and the
//! selection reconciliation (`App::reconcile`) that must see the final entry
//! list for this frame.

use std::collections::HashMap;

use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Cell, Clear, HighlightSpacing, Paragraph, Row, Table},
    Frame,
};

use super::format::{centered_rect, fmt_age, fmt_mem, fmt_uptime, tildify, unix_now};
use super::state::{is_agent_service, App, Entry, GroupBy, HistKey, PaneKind, SortMode};
use crate::launcher::{Launcher, LauncherKind};
use crate::model::{Agent, Target, TargetKey, TargetKind};

/// Column widths (PROJECT, COMMAND, PORT, CPU, MEM, VIA, UP, BRANCH).
const COLW: [u16; 8] = [18, 16, 7, 7, 7, 14, 6, 15];
/// How far PROJECT and BRANCH grow into spare width.
const PROJECT_MAX: u16 = 36;
const BRANCH_MAX: u16 = 40;
const BRANCH: usize = 7;
/// The table keeps at least this many lines (3 rows, header, borders)
/// when panes open below it; with fewer rows it counts as hidden.
const TABLE_MIN: u16 = 6;
const TABLE_MIN_ROWS: u16 = 3;

pub fn render(frame: &mut Frame, app: &mut App) {
    app.reconcile();

    let log_open = app.log.is_some();
    let inspect_open = app.inspect;
    // an agent lists its shells/helpers below the usual five lines
    let mut inspect_h = match (inspect_open, app.selected_agent().is_some()) {
        (false, _) => 0,
        (true, true) => 13,
        (true, false) => 10,
    };
    let mut log_h = if log_open { app.log_height } else { 0 };
    // keep the table visible: shrink the log first, then inspect
    let room = frame.area().height.saturating_sub(2); // detail + footer
    let mut over = (inspect_h + log_h + TABLE_MIN).saturating_sub(room);
    let cut = over.min(log_h.saturating_sub(3));
    log_h -= cut;
    over -= cut;
    inspect_h -= over.min(inspect_h.saturating_sub(3));

    let mut constraints: Vec<Constraint> = vec![Constraint::Min(0)];
    if inspect_open {
        constraints.push(Constraint::Length(inspect_h));
    }
    if log_open {
        constraints.push(Constraint::Length(log_h));
    }
    constraints.push(Constraint::Length(1)); // detail
    constraints.push(Constraint::Length(1)); // footer
    let chunks = Layout::vertical(constraints).split(frame.area());

    let mut i = 0;
    let table_area = chunks[i];
    i += 1;
    let inspect_area = inspect_open.then(|| {
        let a = chunks[i];
        i += 1;
        a
    });
    let log_area = log_open.then(|| {
        let a = chunks[i];
        i += 1;
        a
    });
    let detail_area = chunks[i];
    let footer_area = chunks[i + 1];

    // record layout for mouse hit-testing and paging
    app.table_rect = table_area;
    app.log_rect = log_area;
    let rows = table_area.height.saturating_sub(3);
    app.page_rows = if rows < TABLE_MIN_ROWS {
        0
    } else {
        rows as usize
    };

    let agents = match (app.snapshot.agents.len(), app.group_by) {
        (0, _) => String::new(),
        (n, _) => format!(" · {n} agents {}", fmt_mem(agents_summary(app).3)),
    };
    let n = app.snapshot.targets.len();
    let title = format!(
        " {}marina · {n} server{}{agents} · sort:{}{}{}{} ",
        if app.demo { "DEMO · " } else { "" },
        if n == 1 { "" } else { "s" },
        app.sort.label(),
        if app.sort_rev { " ▴" } else { "" },
        match app.group_by {
            GroupBy::Project => "",
            GroupBy::Session => " · by:session",
        },
        // the filter stays named here, whatever the footer is showing
        if app.filter.is_empty() {
            String::new()
        } else {
            format!(" · filter:{}", app.filter)
        }
    );
    let block = Block::bordered().title(title);

    if app.page_rows == 0 && !app.display.is_empty() {
        app.header_hits.clear();
        let hint = Paragraph::new(Line::styled(
            "table hidden — Esc closes panes",
            Style::new().fg(Color::DarkGray),
        ))
        .block(block);
        frame.render_widget(hint, table_area);
    } else if app.display.is_empty() {
        app.header_hits.clear();
        let dim = Style::new().fg(Color::DarkGray);
        let (msg, hint) = if app.snapshot.seq == 0 {
            (Line::from("scanning…"), "")
        } else if let Some(e) = &app.snapshot.error {
            (
                Line::styled(format!("⚠ {e}"), Style::new().fg(Color::Red)),
                "Servers can't be listed until the port scan works.",
            )
        } else if !app.filter.is_empty() {
            (Line::from("No servers match the filter."), "Esc clears it.")
        } else if app.snapshot.targets.is_empty() && app.group_by == GroupBy::Project {
            (
                Line::from("No dev servers detected."),
                "Start a dev server (npm run dev · cargo run · uvicorn · rails s) and it'll appear here.",
            )
        } else {
            (
                Line::from("No dev servers or agent sessions detected."),
                "Start a dev server (npm run dev · cargo run · uvicorn · rails s) and it'll appear here.",
            )
        };
        let hint = Paragraph::new(vec![
            Line::from(""),
            msg.centered(),
            Line::styled(hint, dim).centered(),
        ])
        .block(block);
        frame.render_widget(hint, table_area);
    } else {
        // Drop columns on narrow panes: UP, then BRANCH, then VIA.
        let cols = visible_columns(table_area.width);
        let arrow = |m: SortMode| match (app.sort == m, app.sort_rev) {
            (false, _) => "",
            (true, false) => " ▾",
            (true, true) => " ▴",
        };
        let head = [
            "PROJECT".to_string(),
            "COMMAND".to_string(),
            format!("PORT{}", arrow(SortMode::Port)),
            format!("CPU{}", arrow(SortMode::Cpu)),
            format!("MEM{}", arrow(SortMode::Mem)),
            "VIA".to_string(),
            "UP".to_string(),
            "BRANCH".to_string(),
        ];
        let header = Row::new(cols.iter().map(|&c| Cell::from(head[c].clone())))
            .style(Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD));
        let colw = column_widths(table_area.width, &cols);
        app.project_w = colw[0];
        app.header_hits = header_hits(table_area, &cols, &colw);
        let by_key: HashMap<&TargetKey, &Target> =
            app.snapshot.targets.iter().map(|t| (&t.key, t)).collect();
        let rows: Vec<Row> = app
            .display
            .iter()
            .map(|e| {
                let (cells, style) = entry_row(e, app, &by_key);
                let mut cells: Vec<Option<Cell>> = cells.into_iter().map(Some).collect();
                Row::new(
                    cols.iter()
                        .filter_map(|&c| cells.get_mut(c).and_then(Option::take)),
                )
                .style(style)
            })
            .collect();
        let widths: Vec<Constraint> = colw.iter().map(|&w| Constraint::Length(w)).collect();
        let table = Table::new(rows, widths)
            .header(header)
            .block(block)
            .row_highlight_style(
                // Reset first, so cell colors can't punch holes in the bar.
                Style::new()
                    .fg(Color::Reset)
                    .bg(Color::Reset)
                    .add_modifier(Modifier::REVERSED),
            )
            .highlight_symbol("▌ ")
            .highlight_spacing(HighlightSpacing::Always);
        frame.render_stateful_widget(table, table_area, &mut app.table_state);
    }

    if let Some(area) = inspect_area {
        frame.render_widget(inspect_panel(app), area);
    }

    if let (Some(area), Some(l)) = (log_area, app.log.as_ref()) {
        let inner_h = area.height.saturating_sub(2) as usize;
        let start = match l.scroll {
            Some(s) => s.min(l.lines.len().saturating_sub(1)),
            None => l.lines.len().saturating_sub(inner_h),
        };
        let text: Vec<Line> = l
            .lines
            .iter()
            .skip(start)
            .take(inner_h)
            .map(|s| Line::from(s.clone()))
            .collect();
        let title = match (l.kind, l.scroll.is_some()) {
            (PaneKind::Diff, _) => format!(" diff: {} — [ ] scroll · q close ", l.title),
            (PaneKind::Log, true) => {
                format!(" log: {} — scrolled · ] follow · q close ", l.title)
            }
            (PaneKind::Log, false) => {
                format!(" log: {} — [ ] scroll · +/- size · q close ", l.title)
            }
        };
        let para = Paragraph::new(text).block(Block::bordered().title(title));
        frame.render_widget(para, area);
    }

    frame.render_widget(detail_line(app), detail_area);
    frame.render_widget(footer_line(app), footer_area);

    if app.show_help {
        let lines = help_lines();
        let area = centered_rect(HELP_W, lines.len() as u16 + 2, frame.area());
        frame.render_widget(Clear, area);
        frame.render_widget(
            Paragraph::new(lines).block(Block::bordered().title(" keys ")),
            area,
        );
    }
}

/// Clickable header cells: (x_start, x_end, sort mode) for PORT/CPU/MEM.
/// Accounts for the block border (1) and the always-reserved highlight
/// symbol "▌ " (2), plus the default column spacing of 1.
fn header_hits(area: Rect, cols: &[usize], colw: &[u16]) -> Vec<(u16, u16, SortMode)> {
    let mut hits = Vec::new();
    let mut x = area.x + 1 + 2;
    for (&i, &w) in cols.iter().zip(colw) {
        let mode = match i {
            2 => Some(SortMode::Port),
            3 => Some(SortMode::Cpu),
            4 => Some(SortMode::Mem),
            _ => None,
        };
        if let Some(m) = mode {
            hits.push((x, x + w, m));
        }
        x += w + 1; // column_spacing
    }
    hits
}

fn footer_line(app: &App) -> Line<'static> {
    if app.is_filtering() {
        Line::styled(
            format!(
                "  /{}▏  Enter apply · Esc cancel · ↑↓ move · Ctrl-U clear",
                app.filter
            ),
            Style::new().fg(Color::Yellow),
        )
    } else if let Some(c) = &app.confirm {
        Line::styled(
            format!("  {}", c.prompt),
            Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        )
    } else if let Some(s) = &app.status {
        Line::styled(format!("  {s}"), Style::new().fg(Color::Yellow))
    } else if let Some(w) = &app.data_warning {
        Line::styled(
            format!("  ⚠ {w}"),
            Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
        )
    } else {
        Line::styled(
            format!("  {}", key_hints(app)),
            Style::new().fg(Color::DarkGray),
        )
    }
}

/// The keys that do something on the selected row, most useful first.
fn key_hints(app: &App) -> String {
    let close = if app.log.is_some() || app.inspect {
        "q close · "
    } else {
        ""
    };
    let keys = match &app.selected {
        _ if app.held_placeholder().is_some() => "j/k move · ? help".to_string(),
        Some(Entry::Group(g)) => {
            let n = app.visible_members(g).len();
            let fold = if app.collapsed.contains(g) {
                "unfold"
            } else {
                "fold"
            };
            format!("Enter {fold} · X kill {n} · R restart {n} · o open · ? help")
        }
        Some(Entry::Agents) => "Enter fold/unfold · v by session · ? help".into(),
        Some(Entry::Agent(_)) => {
            "Enter jump · d diff · o open its server · i inspect · ? help".into()
        }
        Some(Entry::Member(_) | Entry::Target(_)) => {
            "Enter inspect · o open · y copy · t logs · R restart · X kill · ? help".into()
        }
        None => "/ filter · v by session · ? help · q quit".into(),
    };
    format!("{close}{keys}")
}

fn detail_line(app: &App) -> Line<'static> {
    if let Some(h) = app.held_placeholder() {
        return Line::styled(
            format!("  {} · {} · j/k to move on", h.label, h.state),
            Style::new().fg(Color::Gray),
        );
    }
    match &app.selected {
        Some(Entry::Group(p)) => {
            let shown = app.visible_members(p).len();
            let total = app
                .group_members(p)
                .into_iter()
                .filter(|t| app.group_by == GroupBy::Session || !is_agent_service(t))
                .count();
            let (cpu, mem) = group_usage(app, p);
            // In session view, name the session itself, not just its label.
            let who = match (app.group_by, app.group_launcher(p)) {
                (GroupBy::Session, Some(l)) => format!("{} · ", l.describe()),
                _ => String::new(),
            };
            // A filter narrows what the header acts on; the agent itself is
            // never part of a kill.
            let (count, kill) = if shown < total {
                (
                    format!("{shown} of {total} shown"),
                    format!("X kills the {shown} shown"),
                )
            } else if app.group_agents(p).is_empty() {
                (plural(shown, "server"), "X kills all".into())
            } else {
                (plural(shown, "server"), "X stops its servers".into())
            };
            Line::styled(
                format!(
                    "  {who}{p} · {count} · {cpu:.1}% · {} · {kill}",
                    fmt_mem(mem)
                ),
                Style::new().fg(Color::Gray),
            )
        }
        Some(Entry::Agents) => {
            let (working, stale, _, mem) = agents_summary(app);
            let n = app.snapshot.agents.len();
            let stale_mem: u64 = app
                .snapshot
                .agents
                .iter()
                .filter(|a| a.is_stale(unix_now()))
                .map(|a| a.mem_bytes)
                .sum();
            Line::styled(
                format!(
                    "  {n} agent sessions · {working} working · {stale} idle and open >1d ({}) · {} total{} · Enter folds/unfolds",
                    fmt_mem(stale_mem),
                    fmt_mem(mem),
                    match crate::model::repeated_helpers(&app.snapshot.agents).first() {
                        // each session's own copy of the same MCP server adds up
                        Some((l, k, m)) => format!(" · helpers: {l} ×{k} {}", fmt_mem(*m)),
                        None => String::new(),
                    }
                ),
                Style::new().fg(Color::Gray),
            )
        }
        Some(Entry::Agent(_)) => match app.selected_agent() {
            Some(a) => {
                let l = a.as_launcher();
                let now = unix_now();
                let gray = Style::new().fg(Color::Gray);
                let mut spans = Vec::new();
                // the task title (from the host) says more than anything else
                if let Some(t) = a.title() {
                    spans.push(Span::styled(
                        format!("  {t} · "),
                        Style::new().fg(Color::White).add_modifier(Modifier::ITALIC),
                    ));
                } else {
                    spans.push(Span::raw("  "));
                }
                let doing = if a.is_stale(now) {
                    format!(
                        "{}, no input for {}{}",
                        a.activity(now),
                        fmt_age(now.saturating_sub(a.last_input())),
                        if app.closable(a, now) {
                            " — looks finished"
                        } else {
                            " — stale"
                        }
                    )
                } else {
                    let shells: Vec<&str> = a.shells().map(|c| c.label.as_str()).collect();
                    if shells.is_empty() {
                        a.activity(now)
                    } else {
                        format!("running {}", shells.join(", "))
                    }
                };
                spans.push(Span::styled(doing, gray));
                if let Some(g) = &a.git {
                    spans.push(Span::styled(format!(" · {}", g.describe()), gray));
                }
                // before the long pid/dir/session part, so it's never cut off
                spans.push(Span::styled(
                    match &a.host {
                        Some(h) => format!(" · Enter jumps ({})", h.short()),
                        None => " · Enter copies resume".into(),
                    },
                    Style::new().fg(Color::DarkGray),
                ));
                spans.push(Span::styled(
                    format!(" · {}", l.describe()),
                    launcher_style(&l),
                ));
                Line::from(spans)
            }
            None => Line::from(""),
        },
        _ => match app.selected_target() {
            Some(t) => {
                let gray = Style::new().fg(Color::Gray);
                let url = t.url.as_ref().map(|u| u.value.as_str()).unwrap_or("—");
                let branch = t.git_branch.as_deref().unwrap_or("—");
                let ports = if t.ports.len() > 1 {
                    t.ports
                        .iter()
                        .map(|p| format!(":{p}"))
                        .collect::<Vec<_>>()
                        .join(" ")
                } else {
                    String::new()
                };
                let mut spans = vec![Span::styled(
                    format!(
                        "  {} · {} · {url} · {} · {branch} · pids:{}",
                        t.project,
                        t.command_label,
                        tildify(&t.cwd.display().to_string()),
                        t.pids.len()
                    ),
                    gray,
                )];
                if !ports.is_empty() {
                    spans.push(Span::styled(format!(" · {ports}"), gray));
                }
                if let Some(c) = &t.container {
                    spans.push(Span::styled(format!(" · container:{c}"), gray));
                }
                if t.exposed {
                    spans.push(Span::styled(
                        " · LAN-exposed (0.0.0.0)",
                        Style::new().fg(Color::Red),
                    ));
                }
                if let Some(l) = &t.launcher {
                    spans.push(Span::styled(
                        format!(" · via {}", l.describe()),
                        launcher_style(l),
                    ));
                }
                Line::from(spans)
            }
            None => Line::from(""),
        },
    }
}

/// The `i` panel: everything about the selection that doesn't fit a row.
fn inspect_panel(app: &App) -> Paragraph<'static> {
    let key = Style::new().fg(Color::Cyan);
    let val = Style::new().fg(Color::Gray);
    let kv = |k: &str, v: String| {
        Line::from(vec![
            Span::styled(format!(" {k:<6} "), key),
            Span::styled(v, val),
        ])
    };
    let lines: Vec<Line> = match (&app.selected, app.selected_target()) {
        (Some(Entry::Agent(_)), _) => match app.selected_agent() {
            Some(a) => agent_inspect(a, app, key, val),
            None => vec![Line::from(" nothing selected")],
        },
        (_, Some(t)) => {
            // Never render raw argv — args can hold tokens/passwords (a
            // documented invariant). The program path + arg count is enough.
            let cmd = match t.anchor_argv.first() {
                Some(prog) if t.anchor_argv.len() > 1 => {
                    format!("{prog} (+{} args)", t.anchor_argv.len() - 1)
                }
                Some(prog) => prog.clone(),
                None => t.command_label.clone(),
            };
            let ports = if t.ports.is_empty() {
                "—".into()
            } else {
                t.ports
                    .iter()
                    .map(|p| format!(":{p}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            };
            let mut port_line = vec![
                Span::styled(" ports  ".to_string(), key),
                Span::styled(ports, val),
            ];
            if t.exposed {
                port_line.push(Span::styled(
                    " · LAN-exposed (0.0.0.0)",
                    Style::new().fg(Color::Red),
                ));
            }
            let mut lines = vec![
                Line::from(vec![
                    Span::styled(
                        format!(" {}", t.project),
                        Style::new().add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(format!(" · {}", t.command_label), val),
                ]),
                kv("cmd", cmd),
                Line::from(port_line),
                kv(
                    "url",
                    t.url
                        .as_ref()
                        .map(|u| u.value.clone())
                        .unwrap_or_else(|| "—".into()),
                ),
                kv(
                    "cwd",
                    format!(
                        "{} · {}",
                        tildify(&t.cwd.display().to_string()),
                        t.git_branch.as_deref().unwrap_or("—")
                    ),
                ),
            ];
            lines.push(match &t.launcher {
                Some(l) => {
                    let mut spans = vec![
                        Span::styled(" via    ".to_string(), key),
                        Span::styled(l.describe(), launcher_style(l)),
                    ];
                    if let Some(hint) = l.resume_hint() {
                        spans.push(Span::styled(format!(" · resume: {hint}"), val));
                    }
                    Line::from(spans)
                }
                None => kv("via", "—".into()),
            });
            if t.container.is_none() {
                lines.push(kv("trend", trend(app, &HistKey::Target(t.key.clone()))));
            }
            lines.push(match &t.container {
                Some(c) => kv("dockr", format!("container {c}")),
                None => kv(
                    "procs",
                    format!(
                        "{} pids · anchor {} · up {}",
                        t.pids.len(),
                        t.anchor.pid,
                        fmt_uptime(t.anchor.start_time)
                    ),
                ),
            });
            lines
        }
        (Some(Entry::Group(p)), _) => {
            let mut lines = vec![Line::from(Span::styled(
                format!(" {p} (group)"),
                Style::new().add_modifier(Modifier::BOLD),
            ))];
            if let (GroupBy::Session, Some(l)) = (app.group_by, app.group_launcher(p)) {
                let mut spans = vec![
                    Span::styled(" via    ".to_string(), key),
                    Span::styled(l.describe(), launcher_style(&l)),
                ];
                if let Some(hint) = l.resume_hint() {
                    spans.push(Span::styled(format!(" · resume: {hint}"), val));
                }
                lines.push(Line::from(spans));
            }
            for t in app.visible_members(p).into_iter().take(5) {
                let port = t
                    .ports
                    .first()
                    .map(|p| format!(":{p}"))
                    .unwrap_or_else(|| "—".into());
                lines.push(kv(&port, t.command_label.clone()));
            }
            lines
        }
        _ => vec![Line::from(" nothing selected")],
    };
    Paragraph::new(lines).block(Block::bordered().title(" inspect — i or q closes "))
}

/// A group's cpu/mem: its visible, measurable targets plus (session view)
/// its agent.
fn group_usage(app: &App, g: &str) -> (f32, u64) {
    let members = app.visible_members(g);
    let measured = members.iter().filter(|t| !t.pids.is_empty());
    let agents = app.group_agents(g);
    (
        measured.clone().map(|t| t.cpu_pct).sum::<f32>()
            + agents.iter().map(|a| a.cpu_pct).sum::<f32>(),
        measured.map(|t| t.mem_bytes).sum::<u64>()
            + agents.iter().map(|a| a.mem_bytes).sum::<u64>(),
    )
}

/// `3 servers`, `1 server`.
fn plural(n: usize, what: &str) -> String {
    format!("{n} {what}{}", if n == 1 { "" } else { "s" })
}

/// A row's cells (all eight columns; the caller keeps the visible ones) and
/// its style.
fn entry_row(
    entry: &Entry,
    app: &App,
    by_key: &HashMap<&TargetKey, &Target>,
) -> (Vec<Cell<'static>>, Style) {
    let none = Style::new();
    match entry {
        Entry::Group(p) => {
            let n = app.visible_members(p).len() + app.group_agents(p).len();
            if n == 0 {
                return held_row(app);
            }
            let (cpu, mem) = group_usage(app, p);
            let arrow = if app.collapsed.contains(p) {
                "▸"
            } else {
                "▾"
            };
            let header_style = match (app.group_by, app.group_launcher(p)) {
                (GroupBy::Session, Some(l)) => launcher_style(&l).add_modifier(Modifier::BOLD),
                _ => Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
            };
            (
                vec![
                    Cell::from(group_header_text(arrow, p, n, app.project_w)),
                    Cell::from(""),
                    Cell::from(""),
                    Cell::from(format!("{cpu:.1}%")),
                    Cell::from(fmt_mem(mem)),
                    Cell::from(""),
                    Cell::from(""),
                    Cell::from(""),
                ],
                header_style,
            )
        }
        Entry::Member(k) | Entry::Target(k) => match by_key.get(k) {
            Some(t) => target_row(t, matches!(entry, Entry::Member(_)), app),
            None => held_row(app),
        },
        Entry::Agent(pid) => match app.snapshot.agents.iter().find(|a| a.pid == *pid) {
            Some(a) => agent_row(a, app),
            None => (Vec::new(), none),
        },
        Entry::Agents => agents_section_row(app),
    }
}

/// The placeholder for a row marina just stopped or restarted:
/// `client-portal · next dev   stopped`.
fn held_row(app: &App) -> (Vec<Cell<'static>>, Style) {
    let dim = Style::new().fg(Color::DarkGray);
    match app.held_placeholder() {
        Some(h) => (
            vec![
                Cell::from(ellipsize(&h.label, app.project_w as usize)),
                Cell::from(h.state),
                Cell::from("—"),
            ],
            dim.add_modifier(Modifier::ITALIC),
        ),
        None => (Vec::new(), dim),
    }
}

/// Counts for the project view's agents section: (working, stale, cpu, mem).
fn agents_summary(app: &App) -> (usize, usize, f32, u64) {
    let now = unix_now();
    let agents = &app.snapshot.agents;
    (
        agents.iter().filter(|a| a.is_working(now)).count(),
        agents.iter().filter(|a| a.is_stale(now)).count(),
        agents.iter().map(|a| a.cpu_pct).sum(),
        agents.iter().map(|a| a.mem_bytes).sum(),
    )
}

/// `▸ agents (21)  2 working  …  14 stale` — the folded section header.
fn agents_section_row(app: &App) -> (Vec<Cell<'static>>, Style) {
    let (working, stale, cpu, mem) = agents_summary(app);
    let arrow = if app.agents_open { "▾" } else { "▸" };
    let n = app.snapshot.agents.len();
    let blocked = app
        .snapshot
        .agents
        .iter()
        .filter(|a| a.is_blocked())
        .count();
    let busy = if blocked > 0 {
        Span::styled(
            format!("{blocked} need you"),
            Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
        )
    } else if working > 0 {
        Span::styled(format!("{working} working"), Style::new().fg(Color::Yellow))
    } else {
        Span::styled("all idle", Style::new().fg(Color::DarkGray))
    };
    let stale = if stale > 0 {
        Span::styled(format!("{stale} stale"), Style::new().fg(Color::DarkGray))
    } else {
        Span::raw("")
    };
    (
        vec![
            Cell::from(group_header_text(arrow, "agents", n, app.project_w)),
            Cell::from(busy),
            Cell::from(""),
            Cell::from(format!("{cpu:.1}%")),
            Cell::from(fmt_mem(mem)),
            Cell::from(stale),
            Cell::from(""),
            Cell::from(""),
        ],
        Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD),
    )
}

/// An agent session: under its group header when it has servers, else a row
/// of its own named by its session label. COMMAND says what it's doing; VIA
/// stays "who started it", so where it lives (Enter's target) is in the
/// detail line.
fn agent_row(a: &Agent, app: &App) -> (Vec<Cell<'static>>, Style) {
    let label = app
        .agent_session_of
        .get(&a.pid)
        .cloned()
        .unwrap_or_else(|| a.name.clone());
    let now = unix_now();
    let stale = a.is_stale(now);
    let l = a.as_launcher();
    let style = if stale {
        Style::new().fg(Color::DarkGray)
    } else {
        launcher_style(&l)
    };
    let project = match app.group_by {
        // under its session header: the agent itself, marked apart from
        // the servers beside it
        GroupBy::Session if !app.visible_members(&label).is_empty() => Cell::from(Span::styled(
            format!("  ◆ {}", a.name),
            style.add_modifier(Modifier::BOLD),
        )),
        GroupBy::Session => Cell::from(Span::styled(
            ellipsize(&label, app.project_w as usize),
            style,
        )),
        // inside the agents section
        GroupBy::Project => Cell::from(Span::styled(
            format!(
                "  {}",
                ellipsize(&label, (app.project_w as usize).saturating_sub(2))
            ),
            style,
        )),
    };
    // what it's doing, not which agent (the label says that)
    let activity = a.activity(now);
    let command = if a.shells().next().is_some() {
        Span::styled(activity, Style::new().fg(Color::Yellow))
    } else if a.is_blocked() {
        Span::styled(
            activity,
            Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
        )
    } else if a.is_working(now) {
        Span::styled(activity, launcher_style(&l))
    } else if activity == "done" {
        Span::styled(activity, Style::new().fg(Color::Green))
    } else {
        Span::styled(activity, Style::new().fg(Color::DarkGray))
    };
    let dash = || Cell::from(Span::styled("—", Style::new().fg(Color::DarkGray)));
    let cells = vec![
        project,
        Cell::from(command),
        dash(),
        Cell::from(format!("{:.1}%", a.cpu_pct)),
        Cell::from(fmt_mem(a.mem_bytes)),
        dash(),
        Cell::from(fmt_uptime(a.start_time)),
        branch_cell(a),
    ];
    // idle for days: dim the whole row, it's the one to go close
    if stale {
        (cells, Style::new().fg(Color::DarkGray))
    } else {
        (cells, Style::new())
    }
}

/// `●4 ↑2 main` — what's uncommitted / unpushed (`✓` when clean), then the
/// branch; the badge goes first so a long branch name can't hide it.
fn branch_cell(a: &Agent) -> Cell<'static> {
    let branch = a.git_branch.clone().unwrap_or_else(|| "—".into());
    match &a.git {
        Some(g) => {
            let color = if g.is_clean() {
                Color::Green
            } else {
                Color::Yellow
            };
            Cell::from(Line::from(vec![
                Span::styled(format!("{} ", g.short()), Style::new().fg(color)),
                Span::raw(branch),
            ]))
        }
        None => Cell::from(branch),
    }
}

/// `cpu ▁▁▃▇▅▂▁ 4.1% · peak 38% · mem 540MB +12MB over 2m` over the recorded
/// history, so a spike or a leak shows without watching the row.
fn trend(app: &App, key: &HistKey) -> String {
    let Some(h) = app.history.get(key).filter(|h| !h.is_empty()) else {
        return "—".into();
    };
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let peak = h.iter().map(|s| s.1).fold(0.0_f32, f32::max);
    // a floor so an idle row reads flat, not as noise scaled up to full height
    let scale = peak.max(5.0);
    // the last 60 samples, one bar each
    let spark: String = h
        .iter()
        .skip(h.len().saturating_sub(60))
        .map(|s| BARS[((s.1 / scale) * 7.0).round().clamp(0.0, 7.0) as usize])
        .collect();
    let (last, cpu, mem) = h[h.len() - 1];
    let (start, _, first) = h[0];
    let span = fmt_age(last.duration_since(start).as_secs());
    let drift = match mem as i64 - first as i64 {
        d if d.unsigned_abs() < 1024 * 1024 => format!("steady over {span}"),
        d if d > 0 => format!("+{} over {span}", fmt_mem(d as u64)),
        d => format!("−{} over {span}", fmt_mem(d.unsigned_abs())),
    };
    format!(
        "cpu {spark} {cpu:.1}% · peak {peak:.1}% · mem {} {drift}",
        fmt_mem(mem)
    )
}

/// Where a session lives and what Enter does about it.
fn where_line(a: &Agent) -> String {
    match &a.host {
        Some(h) => format!("{} · Enter jumps there", h.describe()),
        None => "no terminal marina can drive · Enter copies the resume command".into(),
    }
}

/// Inspect for an agent session: who/where, how to resume, and what it's
/// running right now — shells first, then helpers (MCP servers, …).
fn agent_inspect(a: &Agent, app: &App, key: Style, val: Style) -> Vec<Line<'static>> {
    let kv = |k: &str, v: String| {
        Line::from(vec![
            Span::styled(format!(" {k:<6} "), key),
            Span::styled(v, val),
        ])
    };
    let l = a.as_launcher();
    let now = unix_now();
    let cmd = if a.argc > 0 {
        format!("{} (+{} args)", a.argv0, a.argc)
    } else {
        a.argv0.clone()
    };
    let mut head = vec![
        Span::styled(
            format!(" {} session", a.name),
            launcher_style(&l).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" · {}", a.project), val),
    ];
    if let Some(t) = a.title() {
        head.push(Span::styled(
            format!(" — {t}"),
            Style::new().add_modifier(Modifier::ITALIC),
        ));
    }
    let mut cwd = format!(
        "{} · {}",
        tildify(&a.cwd.display().to_string()),
        a.git_branch.as_deref().unwrap_or("—")
    );
    if let Some(g) = &a.git {
        cwd.push_str(&format!(" · {} · d shows the diff", g.describe()));
    }
    let state = match a.tty {
        Some(t) => format!(
            "{} · output {} ago · input {} ago",
            a.activity(now),
            fmt_age(now.saturating_sub(t.output)),
            fmt_age(now.saturating_sub(t.input))
        ),
        None => format!("{} · no terminal (judged by CPU)", a.activity(now)),
    };
    let state = if app.closable(a, now) {
        format!("{state} · looks finished: stale, no servers, clean tree")
    } else {
        state
    };
    let mut lines = vec![
        Line::from(head),
        kv("cwd", cwd),
        kv("trend", trend(app, &HistKey::Agent(a.pid))),
        kv(
            "resume",
            l.resume_hint()
                .unwrap_or_else(|| a.session.clone().unwrap_or_else(|| "—".into())),
        ),
        kv("where", where_line(a)),
        kv("state", state),
        kv(
            "procs",
            format!(
                "{cmd} · {} pids · pid {} · up {} · marina never stops agents",
                a.pids.len(),
                a.pid,
                fmt_uptime(a.start_time)
            ),
        ),
    ];
    if a.children.is_empty() {
        lines.push(kv("doing", "no shells or helpers".into()));
    }
    // room for 4 child lines in the 12-row panel (10 inner, 6 used above)
    let shown = if a.children.len() <= 4 {
        a.children.len()
    } else {
        3
    };
    for c in a.children.iter().take(shown) {
        let k = if c.shell { "shell" } else { "helper" };
        lines.push(kv(
            k,
            format!(
                "{} · pid {} · {:.1}% · {} · up {}",
                c.label,
                c.pid,
                c.cpu_pct,
                fmt_mem(c.mem_bytes),
                fmt_uptime(c.start_time)
            ),
        ));
    }
    if a.children.len() > shown {
        lines.push(kv("", format!("+{} more", a.children.len() - shown)));
    }
    lines
}

fn target_row(t: &Target, indent: bool, app: &App) -> (Vec<Cell<'static>>, Style) {
    let dying = app.is_dying(&t.key);
    let flashing = app.is_flashing(&t.key);
    let port = match t.ports.first() {
        Some(p) => format!(":{p}"),
        None => "—".into(),
    };
    let service = crate::launcher::agent_service(&t.anchor_argv);
    let project_text = if let Some(agent) = service.filter(|_| !indent) {
        // beside the agents section: whose server it is
        format!("{agent} server")
    } else if indent && app.group_by == GroupBy::Session {
        // a session spans projects: say whose server each one is
        format!(
            "  {}",
            ellipsize(&t.project, (app.project_w as usize).saturating_sub(2))
        )
    } else if indent {
        "  ↳".to_string()
    } else {
        match t.kind {
            TargetKind::Watched => format!("{} ·watch", t.project),
            TargetKind::Listener => t.project.clone(),
        }
    };
    let project = if flashing {
        Cell::from(Span::styled(
            project_text,
            Style::new().fg(Color::Green).add_modifier(Modifier::BOLD),
        ))
    } else {
        Cell::from(project_text)
    };
    // No pids → nothing measurable (container stats live in the VM).
    let (cpu, mem) = if t.pids.is_empty() {
        ("—".to_string(), "—".to_string())
    } else {
        (format!("{:.1}%", t.cpu_pct), fmt_mem(t.mem_bytes))
    };
    // Infra (databases/caches) gets a subtle category tag rather than a dimmed
    // row — categorize it without making it look dead. Containers likewise.
    let tag = infra_tag(&t.command_label).or(t.container.is_some().then_some("docker"));
    let command = match tag {
        _ if dying => Cell::from("stopping…"),
        Some(tag) => Cell::from(Line::from(vec![
            Span::raw(t.command_label.clone()),
            Span::styled(format!(" ·{tag}"), Style::new().fg(Color::DarkGray)),
        ])),
        None => Cell::from(t.command_label.clone()),
    };
    // A LAN-exposed port is security-relevant — make it visible at a glance.
    let port_cell = if t.exposed {
        Cell::from(Span::styled(
            format!("{port}!"),
            Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
        ))
    } else {
        Cell::from(port)
    };
    let cells = vec![
        project,
        command,
        port_cell,
        Cell::from(cpu),
        Cell::from(mem),
        match &t.launcher {
            Some(l) => Cell::from(Span::styled(l.short(), launcher_style(l))),
            None => Cell::from(Span::styled("—", Style::new().fg(Color::DarkGray))),
        },
        Cell::from(fmt_uptime(t.anchor.start_time)),
        Cell::from(t.git_branch.clone().unwrap_or_else(|| "—".into())),
    ];
    if dying {
        (
            cells,
            Style::new().fg(Color::Red).add_modifier(Modifier::DIM),
        )
    } else {
        (cells, Style::new())
    }
}

/// A category tag for recognized infrastructure, so datastores/caches read as
/// "supporting service" without dimming the row (which looks like "dead").
fn infra_tag(label: &str) -> Option<&'static str> {
    match label {
        "postgres" | "mysql" | "mongodb" => Some("db"),
        "redis" | "memcached" => Some("cache"),
        _ => None,
    }
}

/// `▾ label (n)` sized to the PROJECT column: a long label is cut with `…`
/// so the member count always stays visible.
fn group_header_text(arrow: &str, label: &str, n: usize, width: u16) -> String {
    let suffix = format!(" ({n})");
    let room = (width as usize).saturating_sub(2 + suffix.chars().count());
    format!("{arrow} {}{suffix}", ellipsize(label, room))
}

/// `s` cut to `room` columns, ending in `…` when it doesn't fit.
fn ellipsize(s: &str, room: usize) -> String {
    if s.chars().count() <= room {
        return s.to_string();
    }
    let cut: String = s.chars().take(room.saturating_sub(1)).collect();
    format!("{cut}…")
}

/// Column widths for a table `width` wide showing columns `cols`: the
/// minimums in `COLW`, with spare room going to PROJECT (long session
/// labels) and then BRANCH, so a wide terminal doesn't cut names short.
fn column_widths(width: u16, cols: &[usize]) -> Vec<u16> {
    let mut w: Vec<u16> = cols.iter().map(|&c| COLW[c]).collect();
    let mut spare = width.saturating_sub(table_width(cols));
    let grow = spare.min(PROJECT_MAX - COLW[0]);
    w[0] += grow;
    spare -= grow;
    if let Some(b) = cols.iter().position(|&c| c == BRANCH) {
        w[b] += spare.min(BRANCH_MAX - COLW[BRANCH]);
    }
    w
}

/// The width columns `cols` need at their minimums: borders (2), the
/// highlight gutter (2), and one space between columns.
fn table_width(cols: &[usize]) -> u16 {
    4 + cols.iter().map(|&c| COLW[c]).sum::<u16>() + cols.len().saturating_sub(1) as u16
}

/// Agents stand out (an orphan — its session ended — in yellow); editors and
/// terminals are ordinary; detached is dim.
fn launcher_style(l: &Launcher) -> Style {
    match l.kind {
        _ if l.is_orphaned() => Style::new().fg(Color::Yellow),
        LauncherKind::Agent => Style::new().fg(Color::Magenta),
        LauncherKind::Editor | LauncherKind::Terminal => Style::new().fg(Color::Gray),
        LauncherKind::Detached => Style::new().fg(Color::DarkGray),
    }
}

/// The columns that fit, by index: all of them, or without UP, then
/// BRANCH, then VIA — so who started a server is the last thing to go.
fn visible_columns(width: u16) -> Vec<usize> {
    const SETS: [&[usize]; 4] = [
        &[0, 1, 2, 3, 4, 5, 6, 7],
        &[0, 1, 2, 3, 4, 5, 7],
        &[0, 1, 2, 3, 4, 5],
        &[0, 1, 2, 3, 4],
    ];
    SETS.iter()
        .find(|cols| table_width(cols) <= width)
        .unwrap_or(&SETS[3])
        .to_vec()
}

/// The help overlay's width: it fits a 60-column terminal.
const HELP_W: u16 = 58;

fn help_lines() -> Vec<Line<'static>> {
    let key = Style::new().fg(Color::Cyan);
    let dim = Style::new().fg(Color::DarkGray);
    let row = |k: &str, d: &str| {
        Line::from(vec![
            Span::styled(format!(" {k:<13}"), key),
            Span::styled(d.to_string(), dim),
        ])
    };
    vec![
        Line::from(""),
        row("j k ↑ ↓", "move (mouse: click, wheel)"),
        row("g G Home End", "top · bottom"),
        row("^d ^u PgDn", "half a page · a page"),
        row("h l ← →", "fold · unfold · h: up to the header"),
        row("Enter", "fold · inspect a server · jump to"),
        row("", "an agent's pane (click again: same)"),
        row("i", "inspect"),
        row("t · d", "log · uncommitted diff, in a pane"),
        row("[ ] + -", "scroll · resize the pane"),
        row("y · o", "copy URL · open it (on a session or"),
        row("", "group: its servers in turn)"),
        row("X", "kill (again: force-kill)"),
        row("R", "restart, output captured to a log"),
        row("", "X and R on a group ask first"),
        row("/", "filter: name, command, port, dir,"),
        row("", "branch, who started it (/claude, /ended)"),
        row("s · S", "sort port/cpu/mem · reverse"),
        row("v", "group by project ↔ who started them"),
        row("Esc", "close pane · inspect · clear filter"),
        row("q · ^C", "close pane, else quit · quit"),
        row("Shift-drag", "select text (Option-drag: iTerm2)"),
        Line::from(""),
        Line::from(Span::styled(
            format!(
                " marina v{} · config: {}",
                env!("CARGO_PKG_VERSION"),
                crate::config::config_path()
                    .map(|p| tildify(&p.display().to_string()))
                    .unwrap_or_else(|| "—".into())
            ),
            dim,
        )),
        Line::from(""),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Snapshot, TargetKey};
    use ratatui::{backend::TestBackend, Terminal};
    use std::sync::Arc;

    fn app_with_sample() -> App {
        let mut app = App::new();
        let mut snap = Snapshot::sample();
        snap.seq = 1;
        app.apply(Arc::new(snap)); // client-portal has 2 targets
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
    fn render_empty_shows_hint() {
        let out = render_to_string(&mut App::new(), 100, 12);
        assert!(
            out.contains("scanning…"),
            "before the first snapshot: {out}"
        );
        let mut app = App::new();
        let mut snap = Snapshot::empty();
        snap.seq = 1;
        app.apply(Arc::new(snap));
        assert!(render_to_string(&mut app, 100, 12).contains("No dev servers detected"));
    }

    #[test]
    fn a_filter_with_no_matches_says_how_to_clear_it() {
        let mut app = app_with_sample();
        app.filter = "zz".into();
        let out = render_to_string(&mut app, 100, 12);
        assert!(out.contains("No servers match the filter") && out.contains("Esc clears it"));
        assert!(
            out.contains("filter:zz"),
            "the title names the filter: {out}"
        );
    }

    #[test]
    fn a_failed_scan_shows_in_the_body() {
        let mut app = App::new();
        let mut snap = Snapshot::empty();
        snap.seq = 1;
        snap.error = Some("port scan failed: boom".into());
        app.apply(Arc::new(snap));
        let out = render_to_string(&mut app, 100, 12);
        assert!(out.contains("⚠ port scan failed: boom"), "{out}");
    }

    #[test]
    fn panes_never_squeeze_the_table_out_on_a_normal_terminal() {
        let mut app = app_with_sample();
        app.inspect = true;
        let (_tx, rx) = std::sync::mpsc::channel();
        app.open_log(
            "x".into(),
            rx,
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
        );
        let _ = render_to_string(&mut app, 80, 24);
        assert!(app.page_rows >= 3, "{}", app.page_rows);
        let out = render_to_string(&mut app, 80, 12);
        assert_eq!(app.page_rows, 0);
        assert!(out.contains("table hidden"), "{out}");
    }

    #[test]
    fn the_footer_offers_the_keys_for_the_row() {
        let mut app = app_with_sample();
        app.selected = Some(Entry::Group("client-portal".into()));
        let out = render_to_string(&mut app, 120, 20);
        assert!(out.contains("X kill 2 · R restart 2"), "{out}");
        app.selected = Some(Entry::Target(TargetKey::Port(8000)));
        let out = render_to_string(&mut app, 120, 20);
        assert!(out.contains("Enter inspect · o open"), "{out}");
        app.ask(super::super::state::Verb::Kill, "kill 2 servers?".into());
        assert!(render_to_string(&mut app, 120, 20).contains("kill 2 servers?"));
    }

    #[test]
    fn render_sample_shows_group_header_members_and_chrome() {
        let out = render_to_string(&mut app_with_sample(), 120, 20);
        assert!(out.contains("client-portal")); // group header
        assert!(out.contains("next dev") && out.contains("postgres")); // members
        assert!(out.contains("billing-api") && out.contains(":3000")); // a single + a port
        assert!(out.contains("PROJECT") && out.contains("COMMAND")); // header row
        assert!(out.contains("? help")); // footer keymap
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
    fn lan_exposed_port_gets_a_marker() {
        // billing-api (:8000) is exposed in the sample data
        let out = render_to_string(&mut app_with_sample(), 120, 20);
        assert!(
            out.contains(":8000!"),
            "exposed port must carry the ! badge"
        );
        assert!(!out.contains(":3000!")); // loopback ports don't
    }

    #[test]
    fn infra_commands_get_a_category_tag() {
        assert_eq!(infra_tag("postgres"), Some("db"));
        assert_eq!(infra_tag("redis"), Some("cache"));
        assert_eq!(infra_tag("vite"), None);
    }

    #[test]
    fn spare_width_goes_to_project_then_branch() {
        let all: Vec<usize> = (0..8).collect();
        assert_eq!(column_widths(101, &all), COLW.to_vec(), "no spare room");
        let w = column_widths(140, &all);
        assert_eq!((w[0], w[7]), (36, 36));
        let w = column_widths(300, &all);
        assert_eq!((w[0], w[7]), (PROJECT_MAX, BRANCH_MAX));
        let no_up = [0, 1, 2, 3, 4, 5, 7];
        assert_eq!(column_widths(94, &no_up)[0], 18);
        assert_eq!(
            column_widths(100, &no_up)[0],
            24,
            "narrow panes grow PROJECT too"
        );
    }

    #[test]
    fn narrow_panes_drop_up_then_branch_then_via() {
        assert_eq!(visible_columns(120).len(), 8);
        assert!(!visible_columns(96).contains(&6), "UP goes first");
        assert!(visible_columns(96).contains(&BRANCH));
        assert_eq!(
            visible_columns(80),
            vec![0, 1, 2, 3, 4, 5],
            "VIA fits 80 columns"
        );
        assert_eq!(visible_columns(60).len(), 5);
    }

    #[test]
    fn help_overlay_lists_keys_version_and_config() {
        let mut app = app_with_sample();
        app.toggle_help();
        let out = render_to_string(&mut app, 120, 28);
        assert!(out.contains("keys"));
        assert!(out.contains("force-kill"));
        assert!(out.contains("Shift-drag"));
        assert!(out.contains(env!("CARGO_PKG_VERSION")));
        assert!(out.contains("config:"));
    }

    #[test]
    fn active_sort_marks_its_column_header() {
        let mut app = app_with_sample();
        app.set_sort(SortMode::Mem);
        assert!(render_to_string(&mut app, 120, 20).contains("MEM ▾"));
    }

    #[test]
    fn via_column_and_inspect_show_the_launching_agent_session() {
        use crate::launcher::{Launcher, LauncherKind};
        use crate::model::TargetKey;
        let mut snap = Snapshot::sample();
        snap.seq = 1;
        let claude = Launcher {
            kind: LauncherKind::Agent,
            name: "claude".into(),
            pid: Some(35115),
            alive: true,
            session: Some("219af9f5-aaaa-4bbb-8ccc-dddddddddddd".into()),
            cwd: None,
            start_time: 0,
        };
        snap.targets[1].launcher = Some(claude.clone()); // billing-api :8000
        snap.targets[2].launcher = Some(Launcher {
            alive: false,
            ..claude
        }); // worker :5555 — its session ended
        let mut app = App::new();
        app.apply(Arc::new(snap));
        let out = render_to_string(&mut app, 120, 24);
        assert!(out.contains("VIA"));
        assert!(out.contains("claude·ended"), "orphan is marked in the row");

        app.selected = Some(Entry::Target(TargetKey::Port(8000)));
        app.toggle_inspect();
        let out = render_to_string(&mut app, 160, 30);
        assert!(out.contains("claude (pid 35115, session 219af9f5)"));
        assert!(out.contains("claude --resume 219af9f5-aaaa-4bbb-8ccc-dddddddddddd"));

        // `/ended` filters down to orphans
        app.filter = "ended".into();
        let out = render_to_string(&mut app, 120, 24);
        assert!(out.contains("worker") && !out.contains("billing-api"));
    }

    #[test]
    fn session_view_renders_agent_rows_and_inspect() {
        use crate::model::Agent;
        let mut snap = Snapshot::sample();
        snap.seq = 1;
        snap.agents = vec![Agent::sample(4242, "41616c1a-c2d3-4845", "marina")];
        let mut app = App::new();
        app.apply(Arc::new(snap));
        // project view: a folded section with the totals
        let out = render_to_string(&mut app, 120, 20);
        assert!(out.contains("1 agents 500MB"), "{out}");
        assert!(
            out.contains("▸ agents (1)") && out.contains("1 working"),
            "{out}"
        );
        assert!(!out.contains("claude·marina"), "folded by default");

        app.toggle_group_by();
        let out = render_to_string(&mut app, 120, 20);
        assert!(out.contains("claude·marina"), "idle agent row is labelled");
        assert!(out.contains("$ cargo test"), "COMMAND says what it's doing");
        assert!(
            !out.contains("41616c1a"),
            "VIA says where it lives, not the id"
        );

        app.selected = Some(Entry::Agent(4242));
        app.toggle_inspect();
        let out = render_to_string(&mut app, 140, 30);
        assert!(out.contains("claude session"));
        assert!(out.contains("claude --resume 41616c1a-c2d3-4845"));
        assert!(out.contains("shell  cargo test"));
        assert!(out.contains("helper npm exec chrome-devtools-mcp"));
        assert!(
            out.contains("running cargo test"),
            "detail line says what it's doing"
        );
    }

    #[test]
    fn agent_rows_show_host_title_git_and_blocked_state() {
        use crate::git::GitStatus;
        use crate::hosts::{HostInfo, HostKind, HostStatus};
        use crate::model::Agent;
        let mut snap = Snapshot::sample();
        snap.seq = 1;
        let mut a = Agent::sample(4242, "41616c1a", "marina");
        a.children.clear();
        a.host = Some(HostInfo {
            kind: HostKind::Herdr,
            target: "w2S:p1".into(),
            status: Some(HostStatus::Blocked),
            title: Some("Agent visibility".into()),
            session: None,
            app_pid: None,
            outer: Some(HostKind::Ghostty),
        });
        a.git = Some(GitStatus {
            changed: 3,
            untracked: 1,
            insertions: 40,
            deletions: 2,
            ahead: Some(2),
            behind: Some(0),
        });
        snap.agents = vec![a];
        let mut app = App::new();
        app.apply(Arc::new(snap));
        let out = render_to_string(&mut app, 120, 20);
        assert!(
            out.contains("1 need you"),
            "section header leads with it: {out}"
        );

        app.toggle_group_by();
        app.selected = Some(Entry::Agent(4242));
        let out = render_to_string(&mut app, 160, 20);
        assert!(out.contains("needs you"));
        assert!(
            out.contains("●4 ↑2 main"),
            "git badge before the branch: {out}"
        );
        assert!(
            out.contains("Agent visibility"),
            "task title in the detail line"
        );
        assert!(out.contains("3 files +40 −2 · 1 untracked · 2 unpushed"));
        assert!(out.contains("Enter jumps (herdr·ghostty)"), "{out}");
        assert!(
            !out.contains("VIA") || out.contains("—"),
            "VIA is who started it"
        );

        app.toggle_inspect();
        let out = render_to_string(&mut app, 160, 30);
        assert!(out.contains("herdr pane w2S:p1 in Ghostty · Enter jumps there"));
        assert!(out.contains("d shows the diff"));
    }

    #[test]
    fn long_group_labels_keep_their_count_visible() {
        let t = group_header_text("▾", "claude·courts-portfolio-site", 3, COLW[0]);
        assert_eq!(t.chars().count(), COLW[0] as usize);
        assert!(t.ends_with("… (3)"), "{t}");
        assert_eq!(
            group_header_text("▾", "client-portal", 2, 22),
            "▾ client-portal (2)"
        );
    }

    #[test]
    fn narrow_pane_drops_the_branch_column() {
        let mut app = app_with_sample();
        assert!(render_to_string(&mut app, 120, 20).contains("BRANCH"));
        let out = render_to_string(&mut app, 80, 20);
        assert!(!out.contains("BRANCH") && out.contains("VIA"), "{out}");
    }

    #[test]
    fn trend_line_shows_spark_peak_and_memory_drift() {
        let mut app = app_with_sample();
        let key = HistKey::Agent(1);
        assert_eq!(trend(&app, &key), "—");
        let mb = 1024 * 1024;
        let t0 = std::time::Instant::now();
        let at = |s: u64| t0 + std::time::Duration::from_secs(s);
        app.history.insert(
            key.clone(),
            [
                (at(0), 0.0, 100 * mb),
                (at(1), 10.0, 100 * mb),
                (at(3), 40.0, 150 * mb),
                (at(4), 2.0, 160 * mb),
            ]
            .into_iter()
            .collect(),
        );
        let t = trend(&app, &key);
        assert!(t.starts_with("cpu ▁▃█▁ 2.0% · peak 40.0%"), "{t}");
        assert!(t.ends_with("mem 160MB +60MB over 4s"), "{t}");
    }

    #[test]
    fn history_follows_rows_and_forgets_gone_ones() {
        let snap = |seq: u64, n: usize| {
            let mut s = Snapshot::sample();
            s.seq = seq;
            for (i, t) in s.targets.iter_mut().enumerate() {
                t.pids = vec![100 + i as u32]; // measurable
            }
            s.targets.truncate(n);
            s
        };
        let mut app = App::new();
        app.apply(Arc::new(snap(1, 9)));
        assert!(app.history.len() > 1);
        let snap = snap(2, 1);
        let kept = HistKey::Target(snap.targets[0].key.clone());
        app.apply(Arc::new(snap));
        assert_eq!(app.history.len(), 1);
        assert_eq!(app.history[&kept].len(), 2, "one sample per snapshot");
    }

    #[test]
    fn inspect_panel_shows_selection_details() {
        use crate::model::TargetKey;
        let mut app = app_with_sample();
        let _ = render_to_string(&mut app, 120, 24); // populate display
        app.selected = Some(Entry::Target(TargetKey::Port(8000))); // billing-api
        app.toggle_inspect();
        let out = render_to_string(&mut app, 120, 24);
        assert!(out.contains("inspect"));
        assert!(out.contains("cwd"));
        assert!(out.contains("billing-api"));

        // a group selection gets the member summary instead
        app.selected = Some(Entry::Group("client-portal".into()));
        let out = render_to_string(&mut app, 120, 24);
        assert!(out.contains("(group)"));
    }

    #[test]
    fn render_records_mouse_hit_zones() {
        let mut app = app_with_sample();
        let _ = render_to_string(&mut app, 120, 20);
        assert_eq!(app.header_hits.len(), 3); // PORT, CPU, MEM
        assert!(app.table_rect.width > 0);
    }

    #[test]
    fn data_warning_is_shown_persistently() {
        let mut app = app_with_sample();
        app.data_warning = Some("port scan failed: boom".into());
        let out = render_to_string(&mut app, 120, 20);
        assert!(out.contains("port scan failed"));
    }

    #[test]
    fn log_pane_renders_with_scroll_hint() {
        use std::sync::atomic::AtomicBool;
        let (_tx, rx) = std::sync::mpsc::channel();
        let mut app = app_with_sample();
        app.open_log("test.log".into(), rx, Arc::new(AtomicBool::new(false)));
        let out = render_to_string(&mut app, 120, 30);
        assert!(out.contains("log: test.log"));
        assert!(out.contains("[ ] scroll"));
    }
}
