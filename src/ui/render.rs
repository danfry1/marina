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
use super::state::{App, Entry, GroupBy, HistKey, SortMode};
use crate::launcher::{Launcher, LauncherKind};
use crate::model::{Agent, Target, TargetKey, TargetKind};

/// Column widths (PROJECT, COMMAND, PORT, CPU, MEM, VIA, UP, BRANCH).
const COLW: [u16; 8] = [22, 16, 7, 7, 9, 14, 6, 15];
/// How far PROJECT and BRANCH grow into spare width.
const PROJECT_MAX: u16 = 36;
const BRANCH_MAX: u16 = 40;

pub fn render(frame: &mut Frame, app: &mut App) {
    app.reconcile();

    let log_open = app.log.is_some();
    let inspect_open = app.inspect;
    let mut constraints: Vec<Constraint> = vec![Constraint::Min(0)];
    if inspect_open {
        // an agent lists its shells/helpers below the usual five lines
        let h = if app.selected_agent().is_some() {
            13
        } else {
            10
        };
        constraints.push(Constraint::Length(h));
    }
    let preview_open = app.ctl.preview.is_some();
    if preview_open {
        // most of the screen: it's where you read what the agent said
        let h = (frame.area().height * 45 / 100).max(10);
        constraints.push(Constraint::Length(h));
    }
    if log_open {
        constraints.push(Constraint::Length(app.log_height));
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
    let preview_area = preview_open.then(|| {
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

    // record layout for mouse hit-testing
    app.table_rect = table_area;
    app.log_rect = log_area;

    let agents = match (app.snapshot.agents.len(), app.group_by) {
        (0, _) => String::new(),
        (n, _) => format!(" · {n} agents {}", fmt_mem(agents_summary(app).3)),
    };
    let title = if app.inbox_open() {
        let now = unix_now();
        let count = |r: u8| {
            app.snapshot
                .agents
                .iter()
                .filter(|a| a.urgency(now) == r)
                .count()
        };
        format!(
            " marina · inbox · {} need you · {} done · {} working · {} sessions ",
            count(0),
            count(1),
            count(2),
            app.snapshot.agents.len()
        )
    } else {
        format!(
            " inbox (a) · ▸ SERVERS │ {} targets{agents} · sort:{}{} ",
            app.snapshot.targets.len(),
            app.sort.label(),
            match app.group_by {
                GroupBy::Project => "",
                GroupBy::Session => " · by:session",
            }
        )
    };
    let block = Block::bordered().title(title);

    if app.display.is_empty() {
        app.header_hits.clear();
        let dim = Style::new().fg(Color::DarkGray);
        let msg = if app.snapshot.targets.is_empty() && app.group_by == GroupBy::Project {
            "No dev processes detected."
        } else if app.snapshot.targets.is_empty() && app.snapshot.agents.is_empty() {
            "No dev processes or agent sessions detected."
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
    } else if app.inbox_open() {
        app.header_hits.clear();
        app.sync_detail();
        // a mail client: cards on the left, the selected session on the
        // right; stacked when the terminal is narrow
        let (list, detail) = if table_area.width >= 120 {
            // the list carries the asks now: give it half
            let w = (table_area.width / 2).clamp(60, 110);
            let c =
                Layout::horizontal([Constraint::Length(w), Constraint::Min(40)]).split(table_area);
            (c[0], c[1])
        } else {
            let c = Layout::vertical([Constraint::Percentage(45), Constraint::Min(8)])
                .split(table_area);
            (c[0], c[1])
        };
        super::agent_view::render_list(frame, app, list);
        super::agent_view::render_detail(frame, app, detail);
    } else {
        // Drop the rightmost columns (BRANCH, then UP, then VIA) on narrow panes.
        let cols = visible_columns(table_area.width);
        let arrow = |m: SortMode| if app.sort == m { " ▾" } else { "" };
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
        let header = Row::new(
            head[..cols]
                .iter()
                .cloned()
                .map(Cell::from)
                .collect::<Vec<_>>(),
        )
        .style(Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD));
        let colw = column_widths(table_area.width, cols);
        app.project_w = colw[0];
        app.header_hits = header_hits(table_area, &colw);
        let by_key: HashMap<&TargetKey, &Target> =
            app.snapshot.targets.iter().map(|t| (&t.key, t)).collect();
        let rows: Vec<Row> = app
            .display
            .iter()
            .map(|e| entry_row(e, app, &by_key))
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

    if let Some(area) = preview_area {
        app.sync_detail();
        super::agent_view::render_detail(frame, app, area);
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
        let hint = if l.scroll.is_some() {
            "scrolled · ] follow · Esc close"
        } else {
            "[ ] scroll · +/- size · Esc close"
        };
        let title = format!(" logs: {} — {hint} ", l.title);
        let para = Paragraph::new(text).block(Block::bordered().title(title));
        frame.render_widget(para, area);
    }

    frame.render_widget(detail_line(app), detail_area);
    frame.render_widget(footer_line(app), footer_area);

    if app.show_help {
        let lines = help_lines();
        let full = frame.area();
        // two columns (agents | servers + everywhere) when it wouldn't fit
        // tall, and there's the width
        let split = lines.len() as u16 + 2 > full.height && full.width >= 150;
        if split {
            let at = lines
                .iter()
                .position(|l| l.to_string().trim() == "SERVERS")
                .unwrap_or(lines.len() / 2);
            let (left, right) = lines.split_at(at);
            let mut right = right.to_vec();
            right.insert(0, Line::from(""));
            let h = left.len().max(right.len()) as u16 + 2;
            let area = centered_rect(146, h, full);
            frame.render_widget(Clear, area);
            let block = Block::bordered().title(" keys ");
            let inner = block.inner(area);
            frame.render_widget(block, area);
            let cols = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
                .split(inner);
            frame.render_widget(Paragraph::new(left.to_vec()), cols[0]);
            frame.render_widget(Paragraph::new(right), cols[1]);
        } else {
            let area = centered_rect(76, lines.len() as u16 + 2, full);
            frame.render_widget(Clear, area);
            frame.render_widget(
                Paragraph::new(lines).block(Block::bordered().title(" keys ")),
                area,
            );
        }
    }
}

/// Clickable header cells: (x_start, x_end, sort mode) for PORT/CPU/MEM.
/// Accounts for the block border (1) and the always-reserved highlight
/// symbol "▌ " (2), plus the default column spacing of 1.
fn header_hits(area: Rect, colw: &[u16]) -> Vec<(u16, u16, SortMode)> {
    let mut hits = Vec::new();
    let mut x = area.x + 1 + 2;
    for (i, &w) in colw.iter().enumerate() {
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
    if let Some(d) = app.new_draft() {
        let place = crate::launch::here()
            .map(|p| p.name())
            .unwrap_or("(no terminal marina can open)");
        let wt = if d.worktree { "on" } else { "off" };
        return Line::from(vec![
            Span::styled(
                format!(
                    "  new {} in {} › ",
                    d.agent,
                    tildify(&d.dir.display().to_string())
                ),
                Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("{}▏", d.prompt), Style::new().fg(Color::White)),
            Span::styled(
                format!("  first prompt, optional · Tab own worktree: {wt} · Enter opens {place} · Esc cancel"),
                Style::new().fg(Color::DarkGray),
            ),
        ]);
    }
    if let Some((who, text)) = app.reply_draft() {
        return Line::from(vec![
            Span::styled(
                format!("  reply to {who} › "),
                Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("{text}▏"), Style::new().fg(Color::White)),
            Span::styled(
                "  Enter send · Esc discard",
                Style::new().fg(Color::DarkGray),
            ),
        ]);
    }
    if app.is_filtering() {
        Line::styled(
            format!("  /{}▏  (Enter: apply · Esc: clear)", app.filter),
            Style::new().fg(Color::Yellow),
        )
    } else if let Some(s) = &app.status {
        Line::styled(format!("  {s}"), Style::new().fg(Color::Yellow))
    } else if let Some(w) = &app.data_warning {
        Line::styled(
            format!("  ⚠ {w}"),
            Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
        )
    } else if !app.filter.is_empty() {
        Line::styled(
            format!("  [filter: {}] · / edit · Esc clear · q quit", app.filter),
            Style::new().fg(Color::DarkGray),
        )
    } else if app.inbox_open() {
        Line::styled(
            "  inbox · Enter jump · p preview · r reply · y/n answer · D diff · a back · ? help · q quit",
            Style::new().fg(Color::DarkGray),
        )
    } else if app.selected_agent().is_some() {
        Line::styled(
            "  Enter jump · p preview · r reply · y/n answer · a inbox · i inspect · ? help · q quit",
            Style::new().fg(Color::DarkGray),
        )
    } else {
        Line::styled(
            "  j/k move · / filter · s sort · v by session · a inbox · i inspect · K kill · ? help · q quit",
            Style::new().fg(Color::DarkGray),
        )
    }
}

fn detail_line(app: &App) -> Line<'static> {
    match &app.selected {
        Some(Entry::Group(p)) => {
            let members: Vec<&Target> = app.group_members(p);
            let (cpu, mem) = group_usage(app, p);
            // In session view, name the session itself, not just its label.
            let who = match (app.group_by, app.group_launcher(p)) {
                (GroupBy::Session, Some(l)) => format!("{} · ", l.describe()),
                _ => String::new(),
            };
            // The agent itself is never part of a kill.
            let kill = if app.group_agents(p).is_empty() {
                "K kills all"
            } else {
                "K stops its servers"
            };
            Line::styled(
                format!(
                    "  {who}{} · {} services · {:.1}% · {} · {kill}",
                    p,
                    members.len(),
                    cpu,
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
            for t in app.group_members(p).into_iter().take(5) {
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
    Paragraph::new(lines).block(Block::bordered().title(" inspect — i / Esc to close "))
}

/// A group's cpu/mem: its measurable targets plus (session view) its agent.
fn group_usage(app: &App, g: &str) -> (f32, u64) {
    let members = app.group_members(g);
    let measured = members.iter().filter(|t| !t.pids.is_empty());
    let agents = app.group_agents(g);
    (
        measured.clone().map(|t| t.cpu_pct).sum::<f32>()
            + agents.iter().map(|a| a.cpu_pct).sum::<f32>(),
        measured.map(|t| t.mem_bytes).sum::<u64>()
            + agents.iter().map(|a| a.mem_bytes).sum::<u64>(),
    )
}

fn entry_row(entry: &Entry, app: &App, by_key: &HashMap<&TargetKey, &Target>) -> Row<'static> {
    match entry {
        Entry::Group(p) => {
            let n = app.group_members(p).len() + app.group_agents(p).len();
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
            Row::new(vec![
                Cell::from(group_header_text(arrow, p, n, app.project_w)),
                Cell::from(""),
                Cell::from(""),
                Cell::from(format!("{cpu:.1}%")),
                Cell::from(fmt_mem(mem)),
                Cell::from(""),
                Cell::from(""),
                Cell::from(""),
            ])
            .style(header_style)
        }
        Entry::Member(k) => match by_key.get(k) {
            Some(t) => target_row(t, true, app),
            None => Row::new(Vec::<Cell>::new()),
        },
        Entry::Target(k) => match by_key.get(k) {
            Some(t) => target_row(t, false, app),
            None => Row::new(Vec::<Cell>::new()),
        },
        Entry::Agent(pid) => match app.snapshot.agents.iter().find(|a| a.pid == *pid) {
            Some(a) => agent_row(a, app),
            None => Row::new(Vec::<Cell>::new()),
        },
        Entry::Agents => agents_section_row(app),
        // only the inbox lists these, and draws its own rows
        Entry::Ended(_) | Entry::Fold(_) => Row::new(Vec::<Cell>::new()),
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
fn agents_section_row(app: &App) -> Row<'static> {
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
    Row::new(vec![
        Cell::from(group_header_text(arrow, "agents", n, app.project_w)),
        Cell::from(busy),
        Cell::from(""),
        Cell::from(format!("{cpu:.1}%")),
        Cell::from(fmt_mem(mem)),
        Cell::from(stale),
        Cell::from(""),
        Cell::from(""),
    ])
    .style(Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD))
}

/// An agent session: under its group header when it has servers, else a row
/// of its own named by its session label. COMMAND counts its live shells.
fn agent_row(a: &Agent, app: &App) -> Row<'static> {
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
        // under its session header, which already names it
        GroupBy::Session if !app.group_members(&label).is_empty() => Cell::from("  ↳"),
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
    let row = Row::new(vec![
        project,
        Cell::from(command),
        Cell::from(Span::styled("—", Style::new().fg(Color::DarkGray))),
        Cell::from(format!("{:.1}%", a.cpu_pct)),
        Cell::from(fmt_mem(a.mem_bytes)),
        // where it lives, i.e. where Enter goes (the session id is in the
        // detail line and `i`)
        Cell::from(match &a.host {
            Some(h) => Span::raw(h.short()),
            None => Span::styled("—", Style::new().fg(Color::DarkGray)),
        }),
        Cell::from(fmt_uptime(a.start_time)),
        branch_cell(a),
    ]);
    // idle for days: dim the whole row, it's the one to go close
    if stale {
        row.style(Style::new().fg(Color::DarkGray))
    } else {
        row
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
        cwd.push_str(&format!(" · {} · D shows the diff", g.describe()));
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

fn target_row(t: &Target, indent: bool, app: &App) -> Row<'static> {
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
    let row = Row::new(vec![
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
    ]);
    if dying {
        row.style(
            Style::new()
                .fg(Color::Red)
                .add_modifier(Modifier::DIM | Modifier::CROSSED_OUT),
        )
    } else {
        row
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

/// How many columns fit: full set, or drop BRANCH (then UP) on narrow panes.
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

/// Column widths for a table `width` wide showing `cols` columns: the
/// minimums in `COLW`, with spare room going to PROJECT (long session
/// labels) and then BRANCH, so a wide terminal doesn't cut names short.
fn column_widths(width: u16, cols: usize) -> Vec<u16> {
    let mut w = COLW[..cols].to_vec();
    // borders (2), highlight symbol (2), one space between columns
    let used = 4 + w.iter().sum::<u16>() + cols.saturating_sub(1) as u16;
    let mut spare = width.saturating_sub(used);
    let grow = spare.min(PROJECT_MAX - COLW[0]);
    w[0] += grow;
    spare -= grow;
    if cols == COLW.len() {
        w[cols - 1] += spare.min(BRANCH_MAX - COLW[cols - 1]);
    }
    w
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

/// Columns that fit: each needs its width + 1 spacing, plus borders and the
/// highlight gutter (4).
fn visible_columns(width: u16) -> usize {
    if width >= 107 {
        8
    } else if width >= 91 {
        7
    } else if width >= 84 {
        6
    } else {
        5
    }
}

fn help_lines() -> Vec<Line<'static>> {
    let key = Style::new().fg(Color::Cyan);
    let dim = Style::new().fg(Color::DarkGray);
    let row = |k: &str, d: &str| {
        Line::from(vec![
            Span::styled(format!("  {k:<11}"), key),
            Span::styled(d.to_string(), dim),
        ])
    };
    let head = |t: &str| {
        Line::from(Span::styled(
            format!(" {t}"),
            Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD),
        ))
    };
    vec![
        Line::from(""),
        head("AGENTS"),
        row(
            "a",
            "inbox ↔ servers: every agent session, what needs you first",
        ),
        row("Enter", "jump to the session · on an ended one: resume it"),
        row("p", "preview its conversation · again: its live screen"),
        row("y · n", "approve / reject what it's asking"),
        row(
            "r · 1–9",
            "reply · quick reply ([agents] quick); Enter sends",
        ),
        row("N", "new session here (Tab: in its own git worktree)"),
        row("X", "end the session (press twice; sends /exit)"),
        row("D", "uncommitted diff of its work tree"),
        Line::from(""),
        head("SERVERS"),
        row("Enter", "fold / unfold a group"),
        row("K · u", "kill · cancel the force-kill"),
        row("R · T", "restart (output to a log) · tail logs"),
        row("Y · O", "copy URL · open in the browser"),
        row("s", "cycle sort (port/cpu/mem) — or click a header"),
        row("v", "group by project / by agent session"),
        Line::from(""),
        head("EVERYWHERE"),
        row("j k · g G", "move · top / bottom (mouse: click, wheel)"),
        row(
            "/",
            "filter (project · command · port · cwd · branch · via)",
        ),
        row("i", "inspect the selection"),
        row("[ ]", "scroll the log / detail pane · +/- resize the log"),
        row("Esc", "close a pane / clear the filter"),
        row("? · q", "this help · quit (also Ctrl+C)"),
        Line::from(""),
        Line::from(Span::styled(
            format!(
                "  marina v{} · config: {}",
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
    use crate::model::Snapshot;
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
        assert_eq!(column_widths(107, 8), COLW.to_vec(), "no spare room");
        let w = column_widths(140, 8);
        assert_eq!((w[0], w[7]), (36, 34));
        let w = column_widths(300, 8);
        assert_eq!((w[0], w[7]), (PROJECT_MAX, BRANCH_MAX));
        assert_eq!(column_widths(91, 7)[0], 22);
        assert_eq!(column_widths(97, 7)[0], 28, "narrow panes grow PROJECT too");
    }

    #[test]
    fn visible_columns_drop_from_the_right_when_narrow() {
        assert_eq!(visible_columns(120), 8);
        assert_eq!(visible_columns(95), 7);
        assert_eq!(visible_columns(85), 6);
        assert_eq!(visible_columns(60), 5);
    }

    #[test]
    fn help_overlay_lists_keys_version_and_config() {
        let mut app = app_with_sample();
        app.toggle_help();
        let short = render_to_string(&mut app, 160, 30);
        assert!(
            short.contains(env!("CARGO_PKG_VERSION")) && short.contains("SERVERS"),
            "two columns on a short, wide terminal: {short}"
        );
        let out = render_to_string(&mut app, 120, 40);
        assert!(out.contains("keys"));
        assert!(out.contains("fold / unfold"));
        assert!(out.contains("tail logs"));
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
        assert!(out.contains("herdr·ghostty"), "where it lives, in VIA");

        app.toggle_inspect();
        let out = render_to_string(&mut app, 160, 30);
        assert!(out.contains("herdr pane w2S:p1 in Ghostty · Enter jumps there"));
        assert!(out.contains("D shows the diff"));
    }

    #[test]
    fn long_group_labels_keep_their_count_visible() {
        let t = group_header_text("▾", "claude·courts-portfolio-site", 3, COLW[0]);
        assert_eq!(t.chars().count(), COLW[0] as usize);
        assert!(t.ends_with("… (3)"), "{t}");
        assert_eq!(
            group_header_text("▾", "client-portal", 2, COLW[0]),
            "▾ client-portal (2)"
        );
    }

    #[test]
    fn narrow_pane_drops_the_branch_column() {
        let mut app = app_with_sample();
        assert!(render_to_string(&mut app, 120, 20).contains("BRANCH"));
        assert!(!render_to_string(&mut app, 64, 20).contains("BRANCH"));
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
        assert!(out.contains("logs: test.log"));
        assert!(out.contains("[ ] scroll"));
    }
}
