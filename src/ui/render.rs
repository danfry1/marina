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

use super::format::{centered_rect, fmt_mem, fmt_uptime, tildify, unix_now};
use super::state::{App, Entry, GroupBy, SortMode};
use crate::launcher::{Launcher, LauncherKind};
use crate::model::{Agent, Target, TargetKey, TargetKind};

/// Column widths (PROJECT, COMMAND, PORT, CPU, MEM, VIA, UP, BRANCH).
const COLW: [u16; 8] = [22, 16, 7, 7, 9, 12, 6, 15];

pub fn render(frame: &mut Frame, app: &mut App) {
    app.reconcile();

    let log_open = app.log.is_some();
    let inspect_open = app.inspect;
    let mut constraints: Vec<Constraint> = vec![Constraint::Min(0)];
    if inspect_open {
        // an agent lists its shells/helpers below the usual five lines
        let h = if app.selected_agent().is_some() {
            12
        } else {
            9
        };
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
        (n, _) => {
            let mem: u64 = app.snapshot.agents.iter().map(|a| a.mem_bytes).sum();
            format!(" · {n} agents {}", fmt_mem(mem))
        }
    };
    let title = format!(
        " marina · {} targets{agents} · sort:{}{} ",
        app.snapshot.targets.len(),
        app.sort.label(),
        match app.group_by {
            GroupBy::Project => "",
            GroupBy::Session => " · by:session",
        }
    );
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
        app.header_hits = header_hits(table_area, cols);
        let by_key: HashMap<&TargetKey, &Target> =
            app.snapshot.targets.iter().map(|t| (&t.key, t)).collect();
        let rows: Vec<Row> = app
            .display
            .iter()
            .map(|e| entry_row(e, app, &by_key))
            .collect();
        let widths: Vec<Constraint> = COLW[..cols]
            .iter()
            .map(|&w| Constraint::Length(w))
            .collect();
        let table = Table::new(rows, widths)
            .header(header)
            .block(block)
            .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED))
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
        let area = centered_rect(58, lines.len() as u16 + 2, frame.area());
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
fn header_hits(area: Rect, cols: usize) -> Vec<(u16, u16, SortMode)> {
    let mut hits = Vec::new();
    let mut x = area.x + 1 + 2;
    for (i, &w) in COLW[..cols].iter().enumerate() {
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
    } else {
        Line::styled(
            "  j/k move · / filter · s sort · v by session · i inspect · K kill · ? help · q quit",
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
                    "  {n} agent sessions · {working} working · {stale} idle and open >1d ({}) · {} total · Enter folds/unfolds · marina never stops agents",
                    fmt_mem(stale_mem),
                    fmt_mem(mem)
                ),
                Style::new().fg(Color::Gray),
            )
        }
        Some(Entry::Agent(_)) => match app.selected_agent() {
            Some(a) => {
                let l = a.as_launcher();
                let shells: Vec<&str> = a.shells().map(|c| c.label.as_str()).collect();
                let doing = if !shells.is_empty() {
                    format!("running {}", shells.join(", "))
                } else if a.is_working() {
                    "working".to_string()
                } else if a.is_stale(unix_now()) {
                    format!("idle, open {} — stale", fmt_uptime(a.start_time))
                } else {
                    "idle".to_string()
                };
                let mut spans = vec![
                    Span::styled(format!("  {}", l.describe()), launcher_style(&l)),
                    Span::styled(
                        format!(
                            " · {} · {doing} · pids:{}",
                            a.git_branch.as_deref().unwrap_or("—"),
                            a.pids.len()
                        ),
                        Style::new().fg(Color::Gray),
                    ),
                ];
                if let Some(hint) = l.resume_hint() {
                    spans.push(Span::styled(
                        format!(" · {hint}"),
                        Style::new().fg(Color::DarkGray),
                    ));
                }
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
            Some(a) => agent_inspect(a, key, val),
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
                Cell::from(group_header_text(arrow, p, n)),
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
    }
}

/// Counts for the project view's agents section: (working, stale, cpu, mem).
fn agents_summary(app: &App) -> (usize, usize, f32, u64) {
    let now = unix_now();
    let agents = &app.snapshot.agents;
    (
        agents.iter().filter(|a| a.is_working()).count(),
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
    let busy = if working > 0 {
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
        Cell::from(group_header_text(arrow, "agents", n)),
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
    let stale = a.is_stale(unix_now());
    let l = a.as_launcher();
    let style = if stale {
        Style::new().fg(Color::DarkGray)
    } else {
        launcher_style(&l)
    };
    let project = match app.group_by {
        // under its session header, which already names it
        GroupBy::Session if !app.group_members(&label).is_empty() => Cell::from("  ↳"),
        GroupBy::Session => Cell::from(Span::styled(label, style)),
        // inside the agents section
        GroupBy::Project => Cell::from(Span::styled(format!("  {label}"), style)),
    };
    // what it's doing, not which agent (the label says that)
    let activity = a.activity();
    let command = if a.shells().next().is_some() {
        Span::styled(activity, Style::new().fg(Color::Yellow))
    } else if a.is_working() {
        Span::styled(activity, launcher_style(&l))
    } else {
        Span::styled(activity, Style::new().fg(Color::DarkGray))
    };
    let row = Row::new(vec![
        project,
        Cell::from(command),
        Cell::from(Span::styled("—", Style::new().fg(Color::DarkGray))),
        Cell::from(format!("{:.1}%", a.cpu_pct)),
        Cell::from(fmt_mem(a.mem_bytes)),
        Cell::from(Span::styled(
            a.session
                .as_deref()
                .map(|s| s[..s.len().min(8)].to_string())
                .unwrap_or_else(|| "—".into()),
            Style::new().fg(Color::DarkGray),
        )),
        Cell::from(fmt_uptime(a.start_time)),
        Cell::from(a.git_branch.clone().unwrap_or_else(|| "—".into())),
    ]);
    // idle for days: dim the whole row, it's the one to go close
    if stale {
        row.style(Style::new().fg(Color::DarkGray))
    } else {
        row
    }
}

/// Inspect for an agent session: who/where, how to resume, and what it's
/// running right now — shells first, then helpers (MCP servers, …).
fn agent_inspect(a: &Agent, key: Style, val: Style) -> Vec<Line<'static>> {
    let kv = |k: &str, v: String| {
        Line::from(vec![
            Span::styled(format!(" {k:<6} "), key),
            Span::styled(v, val),
        ])
    };
    let l = a.as_launcher();
    let cmd = if a.argc > 0 {
        format!("{} (+{} args)", a.argv0, a.argc)
    } else {
        a.argv0.clone()
    };
    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                format!(" {} session", a.name),
                launcher_style(&l).add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!(" · {}", a.project), val),
        ]),
        kv("cmd", cmd),
        kv(
            "cwd",
            format!(
                "{} · {}",
                tildify(&a.cwd.display().to_string()),
                a.git_branch.as_deref().unwrap_or("—")
            ),
        ),
        kv(
            "resume",
            l.resume_hint()
                .unwrap_or_else(|| a.session.clone().unwrap_or_else(|| "—".into())),
        ),
        kv(
            "procs",
            format!(
                "{} pids · pid {} · up {} · marina never stops agents",
                a.pids.len(),
                a.pid,
                fmt_uptime(a.start_time)
            ),
        ),
    ];
    if a.children.is_empty() {
        lines.push(kv("doing", "idle — no shells or helpers".into()));
    }
    // room for 5 child lines in the 12-row panel (10 inner, 5 used above)
    let shown = if a.children.len() <= 5 {
        a.children.len()
    } else {
        4
    };
    for c in a.children.iter().take(shown) {
        let k = if c.shell { "shell" } else { "helper" };
        lines.push(kv(
            k,
            format!(
                "{} · pid {} · {:.1}% · up {}",
                c.label,
                c.pid,
                c.cpu_pct,
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
    let project_text = if indent {
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
fn group_header_text(arrow: &str, label: &str, n: usize) -> String {
    let suffix = format!(" ({n})");
    let room = (COLW[0] as usize).saturating_sub(2 + suffix.chars().count());
    let label = if label.chars().count() > room {
        let cut: String = label.chars().take(room.saturating_sub(1)).collect();
        format!("{cut}…")
    } else {
        label.to_string()
    };
    format!("{arrow} {label}{suffix}")
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
    if width >= 105 {
        8
    } else if width >= 89 {
        7
    } else if width >= 82 {
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
    vec![
        Line::from(""),
        row("j / k", "move (mouse: click, wheel)"),
        row("g / G", "top / bottom"),
        row("Enter", "fold / unfold group"),
        row("i", "inspect selection"),
        row("s", "cycle sort (port/cpu/mem) — or click a header"),
        row(
            "/",
            "filter (project · command · port · cwd · branch · via)",
        ),
        row("v", "group by project / agent session (lists agents)"),
        row("K · u", "kill · cancel force-kill"),
        row("R", "restart (output captured to a log)"),
        row("T", "tail logs"),
        row("[ · ]", "scroll logs · +/- resize"),
        row("Y · O", "copy URL · open"),
        row("Esc", "close pane / clear filter"),
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
        let out = render_to_string(&mut app, 120, 28);
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
        assert!(out.contains("41616c1a"), "session id in the VIA column");

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
    fn long_group_labels_keep_their_count_visible() {
        let t = group_header_text("▾", "claude·courts-portfolio-site", 3);
        assert_eq!(t.chars().count(), COLW[0] as usize);
        assert!(t.ends_with("… (3)"), "{t}");
        assert_eq!(
            group_header_text("▾", "client-portal", 2),
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
