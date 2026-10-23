//! Drawing for agent control: the inbox (a list of session cards beside the
//! selected session's detail) and the preview pane, which shows the same
//! detail under the project view.
//!
//! The detail is built for "what is going on here?" at a glance: a fixed
//! context header (repo, task, branch and changes, servers, PR, where it
//! runs), then what you asked, Claude's status recap, and its reply —
//! scrollable — or the session's live screen.

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use ratatui::Frame;

use super::control::PreviewMode;
use super::format::{fmt_age, tildify, unix_now};
use super::state::{App, Entry};
use crate::model::Agent;

const KEY: Style = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);
const DIM: Style = Style::new().fg(Color::DarkGray);

/// The inbox's groups, by urgency (unread replies count as new) and how
/// recently it was active.
fn group_of(app: &App, a: &Agent, now: u64) -> &'static str {
    match app.inbox_urgency(a, now) {
        0 => "NEEDS YOU",
        1 => "NEW",
        2 => "WORKING",
        4 => "STALE",
        _ if now.saturating_sub(a.last_active()) < 3600 => "RECENT",
        _ => "EARLIER",
    }
}

/// `●` coloured by state: red needs you, cyan new, yellow working.
fn dot(app: &App, a: &Agent, now: u64) -> Span<'static> {
    let (c, color) = match app.inbox_urgency(a, now) {
        0 => ("●", Color::Red),
        1 => ("●", Color::Cyan),
        2 => ("◐", Color::Yellow),
        4 => ("·", Color::DarkGray),
        _ => ("○", Color::Gray),
    };
    Span::styled(c, Style::new().fg(color).add_modifier(Modifier::BOLD))
}

/// `pixgolf #d62d` — the session label without the agent name, unless it
/// isn't Claude Code (then `opencode·marina`).
fn short_label(app: &App, a: &Agent) -> String {
    let label = app.agent_label(a);
    if a.name == "claude" {
        label
            .strip_prefix("claude·")
            .map(String::from)
            .unwrap_or(label)
    } else {
        label
    }
}

/// One line on what it's doing or last said.
fn gist(app: &App, a: &Agent, now: u64) -> String {
    let s = a.summary.as_ref();
    if app.waiting_on_you(a) {
        return match a.pending_tool() {
            Some(t) => format!("wants to run {t}"),
            None => "is asking you something".into(),
        };
    }
    if a.is_working(now) {
        let shells: Vec<&str> = a.shells().map(|c| c.label.as_str()).collect();
        if !shells.is_empty() {
            return format!("running {}", shells.join(", "));
        }
        if let Some(t) = a.pending_tool() {
            return format!("running {t}");
        }
    }
    s.and_then(|s| s.recap.as_deref())
        .map(first_sentence)
        .or_else(|| s.and_then(|s| s.last_reply.as_deref()).map(first_sentence))
        .unwrap_or_default()
}

pub(crate) fn first_sentence(s: &str) -> String {
    let line = first_line(s);
    match line.find(". ") {
        Some(i) if i > 20 => line[..=i].to_string(),
        _ => line,
    }
}

/// Clip to `w` columns with `…`.
fn clip(s: &str, w: usize) -> String {
    if s.chars().count() <= w {
        s.to_string()
    } else {
        let mut c: String = s.chars().take(w.saturating_sub(1)).collect();
        c.push('…');
        c
    }
}

/// The inbox list: grouped two-line cards. Records each card's rows for
/// mouse hits, and keeps the selection in view.
pub(crate) fn render_list(frame: &mut Frame, app: &mut App, area: Rect) {
    app.ctl.list_rect = area;
    let now = unix_now();
    let inner_w = area.width.saturating_sub(2) as usize;
    let inner_h = area.height.saturating_sub(2) as usize;
    let selected = app.selected.clone();
    // (line, entry) per row; headings and gaps have none
    let mut rows: Vec<(Line<'static>, Option<Entry>)> = Vec::new();
    let mut group = "";
    let pids: Vec<u32> = app
        .display
        .iter()
        .filter_map(|e| match e {
            Entry::Agent(p) => Some(*p),
            _ => None,
        })
        .collect();
    for pid in pids {
        let Some(a) = app.snapshot.agents.iter().find(|a| a.pid == pid) else {
            continue;
        };
        let g = group_of(app, a, now);
        if g != group {
            if !group.is_empty() {
                rows.push((Line::raw(""), None));
            }
            let n = app
                .snapshot
                .agents
                .iter()
                .filter(|x| {
                    app.display.contains(&Entry::Agent(x.pid)) && group_of(app, x, now) == g
                })
                .count();
            let style = if g == "NEEDS YOU" {
                Style::new().fg(Color::Red).add_modifier(Modifier::BOLD)
            } else {
                KEY
            };
            rows.push((Line::styled(format!(" {g} · {n}"), style), None));
            group = g;
        }
        let sel = selected == Some(Entry::Agent(pid));
        let label = short_label(app, a);
        let ago = match a.last_active() {
            0 => String::new(),
            t => fmt_age(now.saturating_sub(t)),
        };
        let title = a.title().unwrap_or("").to_string();
        // line 1: ● label  title ……… ago
        // ⇄N: N other sessions share its work tree
        let mates = app.tree_mates(a);
        let share = if mates.is_empty() {
            String::new()
        } else {
            format!(" ⇄{}", mates.len())
        };
        let clash = a.is_working(now) && mates.iter().any(|m| m.is_working(now));
        let share_style = Style::new().fg(if clash { Color::Red } else { Color::Yellow });
        let fixed = 4 + label.chars().count() + share.chars().count() + 2 + ago.chars().count() + 1;
        let title = clip(&title, inner_w.saturating_sub(fixed));
        let pad = inner_w.saturating_sub(fixed + title.chars().count());
        let bar = if sel { "▌" } else { " " };
        let label_style = Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD);
        // unread: the title stands out, like an unread email's subject
        let title_style = if app.is_unread(a, now) {
            Style::new().fg(Color::White).add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(Color::Gray)
        };
        let l1 = Line::from(vec![
            Span::styled(bar, Style::new().fg(Color::Magenta)),
            Span::raw(" "),
            dot(app, a, now),
            Span::raw(" "),
            Span::styled(label, label_style),
            Span::styled(share, share_style),
            Span::raw("  "),
            Span::styled(title, title_style),
            Span::raw(" ".repeat(pad)),
            Span::styled(ago, DIM),
            Span::raw(" "),
        ]);
        let g2 = clip(&gist(app, a, now), inner_w.saturating_sub(5));
        let gist_style = if app.waiting_on_you(a) {
            Style::new().fg(Color::Red)
        } else {
            Style::new().fg(Color::Gray)
        };
        let l2 = Line::from(vec![
            Span::styled(bar, Style::new().fg(Color::Magenta)),
            Span::raw("   "),
            Span::styled(g2, gist_style),
        ]);
        let (l1, l2) = if sel {
            let hi = Style::new().bg(Color::Indexed(236));
            (l1.patch_style(hi), l2.patch_style(hi))
        } else {
            (l1, l2)
        };
        rows.push((l1, Some(Entry::Agent(pid))));
        rows.push((l2, Some(Entry::Agent(pid))));
    }

    // sessions that exited lately: dim cards, Enter resumes
    let ended: Vec<&crate::transcript::Ended> = app
        .display
        .iter()
        .filter_map(|e| match e {
            Entry::Ended(id) => app.snapshot.ended.iter().find(|x| &x.id == id),
            _ => None,
        })
        .collect();
    if !ended.is_empty() {
        if !rows.is_empty() {
            rows.push((Line::raw(""), None));
        }
        rows.push((
            Line::styled(format!(" ENDED · {} · Enter resumes", ended.len()), DIM),
            None,
        ));
    }
    for e in ended {
        let entry = Entry::Ended(e.id.clone());
        let sel = selected.as_ref() == Some(&entry);
        let bar = if sel { "▌" } else { " " };
        let label = e
            .cwd
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let ago = fmt_age(now.saturating_sub(e.at));
        let title = e.summary.title.clone().unwrap_or_default();
        let fixed = 4 + label.chars().count() + 2 + ago.chars().count() + 1;
        let title = clip(&title, inner_w.saturating_sub(fixed));
        let pad = inner_w.saturating_sub(fixed + title.chars().count());
        let l1 = Line::from(vec![
            Span::styled(bar, Style::new().fg(Color::Magenta)),
            Span::styled(" ◌ ", DIM),
            Span::styled(label, Style::new().fg(Color::Gray)),
            Span::raw("  "),
            Span::styled(title, DIM),
            Span::raw(" ".repeat(pad)),
            Span::styled(ago, DIM),
            Span::raw(" "),
        ]);
        let said = e
            .summary
            .recap
            .as_deref()
            .or(e.summary.last_reply.as_deref())
            .map(first_sentence)
            .unwrap_or_default();
        let l2 = Line::from(vec![
            Span::styled(bar, Style::new().fg(Color::Magenta)),
            Span::raw("   "),
            Span::styled(clip(&said, inner_w.saturating_sub(5)), DIM),
        ]);
        let (l1, l2) = if sel {
            let hi = Style::new().bg(Color::Indexed(236));
            (l1.patch_style(hi), l2.patch_style(hi))
        } else {
            (l1, l2)
        };
        rows.push((l1, Some(entry.clone())));
        rows.push((l2, Some(entry)));
    }

    // scroll so the selected card is visible
    let sel_row = rows
        .iter()
        .position(|(_, e)| e.is_some() && *e == selected)
        .unwrap_or(0);
    let mut top = app.ctl.list_top.min(rows.len().saturating_sub(1));
    if sel_row < top {
        // include the heading above a group's first card
        top = sel_row.saturating_sub(1);
    } else if sel_row + 2 > top + inner_h {
        top = (sel_row + 2).saturating_sub(inner_h);
    }
    app.ctl.list_top = top;
    app.ctl.hits = rows
        .iter()
        .enumerate()
        .skip(top)
        .take(inner_h)
        .filter_map(|(i, (_, e))| e.clone().map(|e| (area.y + 1 + (i - top) as u16, e)))
        .collect();
    let lines: Vec<Line> = rows
        .into_iter()
        .skip(top)
        .take(inner_h)
        .map(|(l, _)| l)
        .collect();
    let title = {
        let count = |r: u8| {
            app.snapshot
                .agents
                .iter()
                .filter(|a| app.inbox_urgency(a, now) == r)
                .count()
        };
        format!(
            " inbox · {} need you · {} new · {} working · {} sessions ",
            count(0),
            count(1),
            count(2),
            app.snapshot.agents.len()
        )
    };
    frame.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(title)),
        area,
    );
}

/// The selected session's detail, in `area`: fixed context header, then
/// the conversation (or live screen), scrolled by `ctl.detail_scroll`.
pub(crate) fn render_detail(frame: &mut Frame, app: &mut App, area: Rect) {
    app.ctl.detail_rect = area;
    let inner_w = area.width.saturating_sub(2) as usize;
    let inner_h = area.height.saturating_sub(2) as usize;
    if let Some(e) = app.selected_ended().cloned() {
        render_ended(frame, app, area, &e);
        return;
    }
    let Some(a) = app.selected_agent() else {
        frame.render_widget(
            Paragraph::new(Line::styled(" select a session", DIM)).block(Block::bordered()),
            area,
        );
        return;
    };
    let mode = app.ctl.preview.unwrap_or(PreviewMode::Reply);
    let head = header_lines(app, a, inner_w);
    let body = match mode {
        PreviewMode::Reply => conversation_lines(a, inner_w),
        PreviewMode::Screen => screen_lines(app, a),
    };
    let room = inner_h.saturating_sub(head.len());
    // the screen sticks to its bottom (the prompt); prose starts at the top
    let max = body.len().saturating_sub(room);
    let offset = app.ctl.detail_scroll.min(max);
    let scroll = match mode {
        PreviewMode::Screen => max.saturating_sub(offset),
        PreviewMode::Reply => offset,
    };
    let more_below = scroll < max;
    let mut lines = head;
    lines.extend(body.into_iter().skip(scroll).take(room));
    let interactive = a.host.as_ref().is_some_and(|h| h.kind.interactive());
    let mut hints = vec!["Enter jump"];
    if interactive {
        hints.push("r reply");
        if !app.ctl.quick.is_empty() {
            hints.push("1-9 quick reply");
        }
        if app.waiting_on_you(a) {
            hints.push("y/n answer");
        }
        hints.push(match mode {
            PreviewMode::Reply => "p screen",
            PreviewMode::Screen => "p conversation",
        });
    }
    if a.git.is_some_and(|g| !g.is_clean()) {
        hints.push("D diff");
    }
    if max > 0 {
        hints.push("[ ] scroll");
    }
    let bottom = format!(
        " {}{} ",
        hints.join(" · "),
        if more_below { " · ↓ more" } else { "" }
    );
    let block = Block::bordered().title_bottom(Line::styled(bottom, DIM));
    frame.render_widget(Paragraph::new(lines).block(block), area);
    // keep the stored offset in range, so scrolling back responds at once
    app.ctl.detail_scroll = offset;
}

/// An exited session: where it was, what it last said, and how to resume.
fn render_ended(frame: &mut Frame, app: &mut App, area: Rect, e: &crate::transcript::Ended) {
    let w = area.width.saturating_sub(2) as usize;
    let inner_h = area.height.saturating_sub(2) as usize;
    let now = unix_now();
    let name = e
        .cwd
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut head = vec![Line::from(vec![
        Span::styled(" ◌ ", DIM),
        Span::styled(
            name,
            Style::new().fg(Color::Gray).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(
                " · ended {} ago · session {}",
                fmt_age(now.saturating_sub(e.at)),
                crate::launcher::short_id(&e.id, 8)
            ),
            DIM,
        ),
    ])];
    if let Some(t) = &e.summary.title {
        head.push(Line::styled(
            format!(" {}", clip(t, w.saturating_sub(1))),
            Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
        ));
    }
    head.push(Line::from(vec![
        Span::styled(" repo   ", KEY),
        Span::raw(tildify(&e.cwd.display().to_string())),
    ]));
    if let Some(pr) = &e.summary.pr_url {
        head.push(Line::from(vec![
            Span::styled(" pr     ", KEY),
            Span::raw(pr.clone()),
        ]));
    }
    head.push(Line::styled(
        format!(" {}", "─".repeat(w.saturating_sub(2))),
        DIM,
    ));
    let body = summary_lines(&e.summary, "claude", w);
    let room = inner_h.saturating_sub(head.len());
    let max = body.len().saturating_sub(room);
    let offset = app.ctl.detail_scroll.min(max);
    head.extend(body.into_iter().skip(offset).take(room));
    let hint = match crate::launch::here() {
        Some(p) => format!(" Enter resumes it in {} · [ ] scroll ", p.name()),
        None => " Enter shows how to resume it · [ ] scroll ".into(),
    };
    frame.render_widget(
        Paragraph::new(head).block(Block::bordered().title_bottom(Line::styled(hint, DIM))),
        area,
    );
    app.ctl.detail_scroll = offset;
}

/// Who, what, where: always visible above the conversation.
fn header_lines(app: &App, a: &Agent, w: usize) -> Vec<Line<'static>> {
    let now = unix_now();
    let mut out = Vec::new();
    let state = if app.waiting_on_you(a) {
        Span::styled(
            "needs you",
            Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
        )
    } else if a.is_working(now) {
        Span::styled(a.activity(now), Style::new().fg(Color::Yellow))
    } else if a.summary.is_some() && a.last_active() > 0 {
        // the same clock as its card: when it last said something
        Span::styled(
            format!("idle {}", fmt_age(now.saturating_sub(a.last_active()))),
            DIM,
        )
    } else {
        Span::styled(a.activity(now), DIM)
    };
    let label = short_label(app, a);
    let place = a
        .host
        .as_ref()
        .map(|h| h.short())
        .unwrap_or_else(|| "no terminal".into());
    let context = a
        .summary
        .as_ref()
        .and_then(|s| s.context_tokens)
        .map(|n| format!(" · {}k context", n / 1000))
        .unwrap_or_default();
    out.push(Line::from(vec![
        Span::raw(" "),
        dot(app, a, now),
        Span::raw(" "),
        Span::styled(
            label,
            Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" · ", DIM),
        state,
        Span::styled(format!(" · {place}{context}"), DIM),
    ]));
    if let Some(t) = a.title() {
        out.push(Line::styled(
            format!(" {}", clip(t, w.saturating_sub(1))),
            Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
        ));
    }
    // repo, branch and what's changed
    let branch = a.git_branch.clone().unwrap_or_else(|| "—".into());
    let mut repo = vec![
        Span::styled(" repo   ", KEY),
        Span::raw(tildify(&a.cwd.display().to_string())),
        Span::styled(format!(" · {branch}"), Style::new().fg(Color::Gray)),
    ];
    if let Some(g) = a.git {
        let style = if g.is_clean() {
            Style::new().fg(Color::Green)
        } else {
            Style::new().fg(Color::Yellow)
        };
        repo.push(Span::styled(format!(" · {}", g.describe()), style));
    }
    out.push(Line::from(repo));
    // other sessions in the same work tree can overwrite its edits
    let mates = app.tree_mates(a);
    if !mates.is_empty() {
        let names: Vec<String> = mates.iter().map(|m| short_label(app, m)).collect();
        let clash = a.is_working(now) && mates.iter().any(|m| m.is_working(now));
        let (lead, color) = if clash {
            ("⚠ working alongside", Color::Red)
        } else {
            ("⇄ shares this tree with", Color::Yellow)
        };
        out.push(Line::styled(
            clip(
                &format!(
                    " {lead} {} — a git worktree each keeps their edits apart",
                    names.join(", ")
                ),
                w,
            ),
            Style::new().fg(color),
        ));
    }
    // what it's running and where you can see it
    let servers: Vec<String> = app
        .agent_servers(a)
        .iter()
        .map(|t| match t.ports.first() {
            Some(p) => format!(":{p} {}", t.command_label),
            None => t.command_label.clone(),
        })
        .collect();
    let mut running: Vec<String> = a.shells().map(|c| format!("$ {}", c.label)).collect();
    running.extend(servers);
    if !running.is_empty() {
        out.push(Line::from(vec![
            Span::styled(" runs   ", KEY),
            Span::raw(clip(&running.join(" · "), w.saturating_sub(8))),
        ]));
    }
    if let Some(pr) = a.summary.as_ref().and_then(|s| s.pr_url.as_deref()) {
        out.push(Line::from(vec![
            Span::styled(" pr     ", KEY),
            Span::raw(pr.to_string()),
        ]));
    }
    if app.waiting_on_you(a) {
        let what = a
            .pending_tool()
            .map(|t| format!(" to run {t}"))
            .unwrap_or_default();
        out.push(Line::styled(
            format!(" ⚠ waiting on you{what} — y approve · n reject · r reply"),
            Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
        ));
    }
    // the canned replies on 1-9, where you'd reach for them
    if a.host.as_ref().is_some_and(|h| h.kind.interactive()) && !app.ctl.quick.is_empty() {
        out.push(Line::from(vec![
            Span::styled(" quick  ", KEY),
            Span::styled(clip(&app.quick_hint(), w.saturating_sub(8)), DIM),
        ]));
    }
    out.push(Line::styled(
        format!(" {}", "─".repeat(w.saturating_sub(2))),
        DIM,
    ));
    out
}

/// You asked / Status / Claude: the transcript summary as sections.
fn conversation_lines(a: &Agent, w: usize) -> Vec<Line<'static>> {
    let Some(s) = &a.summary else {
        let interactive = a.host.as_ref().is_some_and(|h| h.kind.interactive());
        return vec![Line::styled(
            if interactive {
                " no transcript for this session — p shows its screen"
            } else {
                " no transcript for this session (marina reads Claude Code's)"
            },
            DIM,
        )];
    };
    summary_lines(s, &a.name, w)
}

/// You asked / Status / <agent> said, from a transcript summary.
fn summary_lines(s: &crate::transcript::Summary, agent: &str, w: usize) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let section = |out: &mut Vec<Line<'static>>, title: &str| {
        if !out.is_empty() {
            out.push(Line::raw(""));
        }
        out.push(Line::styled(format!(" {title}"), KEY));
    };
    if let Some(p) = &s.last_prompt {
        section(&mut out, "YOU ASKED");
        for l in wrap(p, w.saturating_sub(3)).into_iter().take(6) {
            out.push(Line::styled(
                format!("   {l}"),
                Style::new().fg(Color::Gray),
            ));
        }
    }
    if let Some(r) = &s.recap {
        section(&mut out, "STATUS");
        for l in wrap(&plain(r), w.saturating_sub(3)) {
            out.push(Line::styled(
                format!("   {l}"),
                Style::new().add_modifier(Modifier::ITALIC),
            ));
        }
    }
    section(&mut out, &format!("{} SAID", agent.to_uppercase()));
    let reply = s.last_reply.as_deref().unwrap_or("(nothing yet)");
    for l in wrap(reply, w.saturating_sub(3)) {
        let mut spans = vec![Span::raw("   ")];
        spans.extend(md_spans(&l));
        out.push(Line::from(spans));
    }
    out
}

fn screen_lines(app: &App, a: &Agent) -> Vec<Line<'static>> {
    let interactive = a.host.as_ref().is_some_and(|h| h.kind.interactive());
    match app.screen_of(a.pid) {
        Some(rows) if !rows.is_empty() => rows.iter().map(|r| Line::raw(r.clone())).collect(),
        Some(_) => vec![Line::styled(" (the screen is empty)", DIM)],
        None if interactive => vec![Line::styled(" reading the screen…", DIM)],
        None => vec![Line::styled(
            format!(
                " marina can read the screen of herdr and tmux panes only — this is a {}",
                a.host
                    .as_ref()
                    .map(|h| h.describe())
                    .unwrap_or_else(|| "session with no terminal".into())
            ),
            DIM,
        )],
    }
}

fn first_line(s: &str) -> String {
    plain(
        s.lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or(""),
    )
}

/// Markdown emphasis and code marks dropped: for one-line summaries.
pub(crate) fn plain(s: &str) -> String {
    s.replace("**", "")
        .replace('`', "")
        .trim_start_matches('#')
        .trim()
        .to_string()
}

/// One line of an agent's markdown as styled spans: `**bold**`, `` `code` ``,
/// and `#` headings; everything else as is.
pub(crate) fn md_spans(line: &str) -> Vec<Span<'static>> {
    let heading = line.trim_start().starts_with('#');
    let line = if heading {
        line.trim_start().trim_start_matches('#').trim_start()
    } else {
        line
    };
    let base = if heading {
        Style::new().add_modifier(Modifier::BOLD)
    } else {
        Style::new()
    };
    let mut spans = Vec::new();
    let (mut bold, mut code) = (false, false);
    let mut cur = String::new();
    let mut rest = line;
    let flush = |cur: &mut String, spans: &mut Vec<Span<'static>>, bold: bool, code: bool| {
        if cur.is_empty() {
            return;
        }
        let mut st = base;
        if bold {
            st = st.add_modifier(Modifier::BOLD);
        }
        if code {
            st = st.fg(Color::Cyan);
        }
        spans.push(Span::styled(std::mem::take(cur), st));
    };
    while let Some(c) = rest.chars().next() {
        if !code && rest.starts_with("**") {
            flush(&mut cur, &mut spans, bold, code);
            bold = !bold;
            rest = &rest[2..];
        } else if c == '`' {
            flush(&mut cur, &mut spans, bold, code);
            code = !code;
            rest = &rest[1..];
        } else {
            cur.push(c);
            rest = &rest[c.len_utf8()..];
        }
    }
    flush(&mut cur, &mut spans, bold, code);
    spans
}

/// Word-wrap `text` to `width` columns (by chars; good enough for prose).
pub(crate) fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(10);
    let mut out = Vec::new();
    for para in text.lines() {
        let mut line = String::new();
        for word in para.split_whitespace() {
            let need = line.chars().count() + usize::from(!line.is_empty()) + word.chars().count();
            if need > width && !line.is_empty() {
                out.push(std::mem::take(&mut line));
            }
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
        }
        // keep indentation-free blank lines between paragraphs
        out.push(line);
    }
    while out.last().is_some_and(|l| l.is_empty()) {
        out.pop();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_prose_and_keeps_paragraph_breaks() {
        let w = wrap("one two three four five six\n\nseven", 12);
        assert_eq!(w, vec!["one two", "three four", "five six", "", "seven"]);
        assert_eq!(wrap("", 20), Vec::<String>::new());
        assert_eq!(first_line("\n  **hello** `x`\nworld"), "hello x");
    }

    #[test]
    fn markdown_marks_become_styles() {
        let s = md_spans("a **bold** and `code`");
        let text: Vec<&str> = s.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, vec!["a ", "bold", " and ", "code"]);
        assert!(s[1].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(s[3].style.fg, Some(Color::Cyan));
        let h = md_spans("## Plan");
        assert_eq!(h[0].content, "Plan");
        assert!(h[0].style.add_modifier.contains(Modifier::BOLD));
    }
}
