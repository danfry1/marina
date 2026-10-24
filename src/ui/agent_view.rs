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

/// The inbox's groups (see `App::inbox_urgency`).
fn group_of(app: &App, a: &Agent, now: u64) -> &'static str {
    match app.inbox_urgency(a, now) {
        0 => "NEEDS YOU",
        1 => "YOUR TURN",
        2 => "WORKING",
        _ => "PARKED",
    }
}

/// The state dot: red needs you, cyan your turn, yellow working, grey parked.
fn dot(app: &App, a: &Agent, now: u64) -> Span<'static> {
    let (c, color) = match app.inbox_urgency(a, now) {
        0 => ("●", Color::Red),
        1 => ("●", Color::Cyan),
        2 => ("◐", Color::Yellow),
        _ => ("○", Color::DarkGray),
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

/// The one line a card says: what it wants from you, what it's running,
/// or (parked) what it's about.
fn row_text(app: &App, a: &Agent, now: u64) -> String {
    let s = a.summary.as_ref();
    match app.inbox_urgency(a, now) {
        0 => match a.pending_tool() {
            Some(t) => format!("approve: {t}"),
            None => match s.and_then(|s| s.ask()) {
                Some(q) => format!("answer: {q}"),
                None => "is asking you something — p to see it".into(),
            },
        },
        2 => {
            let shells: Vec<&str> = a.shells().map(|c| c.label.as_str()).collect();
            if !shells.is_empty() {
                format!("$ {}", shells.join(", "))
            } else if let Some(t) = a.pending_tool() {
                t
            } else {
                a.title()
                    .map(String::from)
                    .unwrap_or_else(|| "working".into())
            }
        }
        1 => s
            .and_then(|s| s.ask())
            .or_else(|| s.and_then(|s| s.did()))
            .or_else(|| a.title().map(String::from))
            .unwrap_or_default(),
        _ => a
            .title()
            .map(String::from)
            .or_else(|| s.and_then(|s| s.did()))
            .unwrap_or_default(),
    }
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

/// The inbox list: one line per session under what-to-do headings, parked
/// and ended sessions folded behind a row each. Records each row for mouse
/// hits, and keeps the selection in view.
pub(crate) fn render_list(frame: &mut Frame, app: &mut App, area: Rect) {
    app.ctl.list_rect = area;
    let now = unix_now();
    let inner_w = area.width.saturating_sub(2) as usize;
    let inner_h = area.height.saturating_sub(2) as usize;
    let selected = app.selected.clone();
    let hi = Style::new().bg(Color::Indexed(236));
    let mut rows: Vec<(Line<'static>, Option<Entry>)> = Vec::new();
    let mut group = "";
    // one session per line: ▌ ● label  text ……… age
    let line = |sel: bool,
                dot: Span<'static>,
                label: String,
                label_style: Style,
                text: String,
                text_style: Style,
                age: String| {
        let fixed = 4 + label.chars().count() + 2 + age.chars().count() + 1;
        // a space before the age, always
        let text = clip(&text, inner_w.saturating_sub(fixed + 1));
        let pad = inner_w.saturating_sub(fixed + text.chars().count());
        let l = Line::from(vec![
            Span::styled(if sel { "▌" } else { " " }, Style::new().fg(Color::Magenta)),
            Span::raw(" "),
            dot,
            Span::raw(" "),
            Span::styled(label, label_style),
            Span::raw("  "),
            Span::styled(text, text_style),
            Span::raw(" ".repeat(pad)),
            Span::styled(age, DIM),
            Span::raw(" "),
        ]);
        if sel {
            l.patch_style(hi)
        } else {
            l
        }
    };
    let entries = app.display.clone();
    for e in &entries {
        match e {
            Entry::Agent(pid) => {
                let Some(a) = app.snapshot.agents.iter().find(|a| a.pid == *pid) else {
                    continue;
                };
                let g = group_of(app, a, now);
                if g != group && g != "PARKED" {
                    if !group.is_empty() {
                        rows.push((Line::raw(""), None));
                    }
                    let n = entries
                        .iter()
                        .filter_map(|x| match x {
                            Entry::Agent(p) => app.snapshot.agents.iter().find(|a| a.pid == *p),
                            _ => None,
                        })
                        .filter(|x| group_of(app, x, now) == g)
                        .count();
                    let style = if g == "NEEDS YOU" {
                        Style::new().fg(Color::Red).add_modifier(Modifier::BOLD)
                    } else {
                        KEY
                    };
                    rows.push((Line::styled(format!(" {g} · {n}"), style), None));
                    group = g;
                }
                let urgency = app.inbox_urgency(a, now);
                let age = if a.is_working(now) {
                    "now".to_string()
                } else {
                    match a.settled_at() {
                        0 => String::new(),
                        t => fmt_age(now.saturating_sub(t)),
                    }
                };
                let mates = app.tree_mates(a);
                let mut label = short_label(app, a);
                if !mates.is_empty() {
                    label.push_str(&format!(" ⇄{}", mates.len()));
                }
                let label_style = if urgency == 3 {
                    Style::new().fg(Color::Gray)
                } else {
                    Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD)
                };
                let text_style = match urgency {
                    0 => Style::new().fg(Color::Red),
                    1 if app.is_unread(a, now) => {
                        Style::new().fg(Color::White).add_modifier(Modifier::BOLD)
                    }
                    1 => Style::new().fg(Color::White),
                    2 => Style::new().fg(Color::Yellow),
                    _ => DIM,
                };
                let sel = selected.as_ref() == Some(e);
                rows.push((
                    line(
                        sel,
                        dot(app, a, now),
                        label,
                        label_style,
                        row_text(app, a, now),
                        text_style,
                        age,
                    ),
                    Some(e.clone()),
                ));
            }
            Entry::Fold(which) => {
                let (name, n, open) = if *which == 0 {
                    let n = app
                        .snapshot
                        .agents
                        .iter()
                        .filter(|a| app.matches_agent(a) && app.inbox_urgency(a, now) == 3)
                        .count();
                    ("PARKED", n, app.ctl.parked_open)
                } else {
                    ("ENDED", app.snapshot.ended.len(), app.ctl.ended_open)
                };
                if !rows.is_empty() {
                    rows.push((Line::raw(""), None));
                }
                let what = match (*which, open) {
                    (0, true) => "Enter hides · X ends them all".to_string(),
                    (_, true) => "Enter hides".to_string(),
                    (0, false) => "finished, nothing asked · Enter shows · X ends them".to_string(),
                    _ => "Enter shows · Enter on one resumes it".to_string(),
                };
                let sel = selected.as_ref() == Some(e);
                let l = Line::from(vec![
                    Span::styled(if sel { "▌" } else { " " }, Style::new().fg(Color::Magenta)),
                    Span::styled(
                        format!("{} {name} · {n}", if open { "▾" } else { "▸" }),
                        DIM.add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(format!("  {what}"), DIM),
                ]);
                rows.push((if sel { l.patch_style(hi) } else { l }, Some(e.clone())));
                group = "PARKED";
            }
            Entry::Ended(id) => {
                let Some(x) = app.snapshot.ended.iter().find(|x| &x.id == id) else {
                    continue;
                };
                let label = x
                    .cwd
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let text = x
                    .summary
                    .title
                    .clone()
                    .or_else(|| x.summary.did())
                    .unwrap_or_default();
                let sel = selected.as_ref() == Some(e);
                rows.push((
                    line(
                        sel,
                        Span::styled("◌", DIM),
                        label,
                        Style::new().fg(Color::Gray),
                        text,
                        DIM,
                        fmt_age(now.saturating_sub(x.at)),
                    ),
                    Some(e.clone()),
                ));
            }
            _ => {}
        }
    }

    // scroll so the selected row is visible
    let sel_row = rows
        .iter()
        .position(|(_, e)| e.is_some() && *e == selected)
        .unwrap_or(0);
    let mut top = app.ctl.list_top.min(rows.len().saturating_sub(1));
    if sel_row < top {
        top = sel_row.saturating_sub(1); // with its heading
    } else if sel_row + 1 > top + inner_h {
        top = (sel_row + 1).saturating_sub(inner_h);
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
        let mut parts: Vec<String> = Vec::new();
        for (n, what) in [
            (count(0), "need you"),
            (count(1), "your turn"),
            (count(2), "working"),
        ] {
            if n > 0 {
                parts.push(format!("{n} {what}"));
            }
        }
        if parts.is_empty() {
            parts.push("nothing needs you".into());
        }
        clip(
            &format!(" ▸ INBOX · servers (a) │ {} ", parts.join(" · ")),
            inner_w,
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
    let body = summary_lines(&e.summary, "claude", w, true);
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

/// Who and where, then what's needed of you, what it did, and one line of
/// context — always visible above the full conversation.
fn header_lines(app: &App, a: &Agent, w: usize) -> Vec<Line<'static>> {
    let now = unix_now();
    let mut out = Vec::new();
    let urgency = app.inbox_urgency(a, now);
    let state = match urgency {
        0 => Span::styled(
            "needs you",
            Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        2 => Span::styled(a.activity(now), Style::new().fg(Color::Yellow)),
        _ => Span::styled(
            format!("idle {}", fmt_age(now.saturating_sub(a.settled_at()))),
            DIM,
        ),
    };
    let place = a
        .host
        .as_ref()
        .map(|h| h.short())
        .unwrap_or_else(|| "no terminal".into());
    // a full context window degrades answers: nudge toward /clear
    let context = match a.summary.as_ref().and_then(|s| s.context_tokens) {
        Some(n) if n >= 700_000 => Span::styled(
            format!(" · {}k context — time to /clear", n / 1000),
            Style::new().fg(Color::Red),
        ),
        Some(n) if n >= 400_000 => Span::styled(
            format!(" · {}k context", n / 1000),
            Style::new().fg(Color::Yellow),
        ),
        Some(n) => Span::styled(format!(" · {}k context", n / 1000), DIM),
        None => Span::raw(""),
    };
    out.push(Line::from(vec![
        Span::raw(" "),
        dot(app, a, now),
        Span::raw(" "),
        Span::styled(
            short_label(app, a),
            Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" · ", DIM),
        state,
        Span::styled(format!(" · {place}"), DIM),
        context,
    ]));
    if let Some(t) = a.title() {
        out.push(Line::styled(
            format!(" {}", clip(t, w.saturating_sub(1))),
            Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
        ));
    }
    // why it's in this group, and how marina knows
    out.push(Line::styled(
        format!(
            " why: {}",
            clip(&app.inbox_reason(a, now), w.saturating_sub(7))
        ),
        DIM,
    ));
    // say up front what marina can't do here, not after a keystroke fails
    let interactive = a.host.as_ref().is_some_and(|h| h.kind.interactive());
    if !interactive && urgency == 1 {
        let place = match &a.host {
            Some(h) => h.describe(),
            None => "a terminal marina can't reach".into(),
        };
        out.push(Line::styled(
            clip(
                &format!(" it's in {place}: marina can't type there — Enter jumps to it"),
                w.saturating_sub(1),
            ),
            Style::new().fg(Color::Yellow),
        ));
    }
    let s = a.summary.as_ref();
    let section =
        |out: &mut Vec<Line<'static>>, title: &str, color: Color, text: &str, style: Style| {
            out.push(Line::raw(""));
            out.push(Line::styled(
                format!(" {title}"),
                Style::new().fg(color).add_modifier(Modifier::BOLD),
            ));
            for l in wrap(text, w.saturating_sub(3)).into_iter().take(5) {
                out.push(Line::styled(format!("   {l}"), style));
            }
        };
    let white = Style::new().fg(Color::White);
    match urgency {
        0 => {
            let what = match a.pending_tool() {
                Some(t) => format!("It wants to run {t}"),
                None => s
                    .and_then(|s| s.ask())
                    .unwrap_or_else(|| "It's asking you something — p shows its screen".into()),
            };
            let keys = if a.host.as_ref().is_some_and(|h| h.kind.interactive()) {
                "  —  y approve · n reject · r reply"
            } else {
                "  —  Enter to answer it there"
            };
            section(
                &mut out,
                "NEEDS YOU",
                Color::Red,
                &format!("{what}{keys}"),
                white,
            );
        }
        2 => {
            let shells: Vec<String> = a.shells().map(|c| format!("$ {}", c.label)).collect();
            let doing = if shells.is_empty() {
                a.pending_tool().unwrap_or_else(|| "thinking".into())
            } else {
                shells.join(" · ")
            };
            section(&mut out, "WORKING", Color::Yellow, &doing, white);
        }
        _ => {
            if let Some(ask) = s.and_then(|s| s.ask()) {
                section(&mut out, "YOUR TURN", Color::Cyan, &ask, white);
            }
        }
    }
    if let Some(did) = s.and_then(|s| s.did()) {
        if Some(&did) != s.and_then(|s| s.ask()).as_ref() {
            section(
                &mut out,
                "WHAT IT DID",
                Color::DarkGray,
                &did,
                Style::new().fg(Color::Gray).add_modifier(Modifier::ITALIC),
            );
        }
    }
    // context, in one line: where, branch and changes, PR, servers, mates
    out.push(Line::raw(""));
    let mut meta: Vec<Span<'static>> = vec![Span::styled(
        format!(" {}", tildify(&a.cwd.display().to_string())),
        DIM,
    )];
    if let Some(b) = &a.git_branch {
        meta.push(Span::styled(format!(" · {b}"), DIM));
    }
    if let Some(g) = a.git {
        let style = if g.is_clean() {
            Style::new().fg(Color::Green)
        } else {
            Style::new().fg(Color::Yellow)
        };
        meta.push(Span::styled(format!(" · {}", g.describe()), style));
    }
    if let Some(pr) = s.and_then(|s| s.pr_url.as_deref()) {
        let short = pr
            .rsplit_once("/pull/")
            .map(|(_, n)| format!("PR #{n}"))
            .unwrap_or_else(|| pr.to_string());
        meta.push(Span::styled(format!(" · {short}"), DIM));
    }
    let servers: Vec<String> = app
        .agent_servers(a)
        .iter()
        .filter_map(|t| t.ports.first().map(|p| format!(":{p}")))
        .collect();
    if !servers.is_empty() {
        meta.push(Span::styled(
            format!(" · serving {}", servers.join(" ")),
            DIM,
        ));
    }
    let mates = app.tree_mates(a);
    if !mates.is_empty() {
        let clash = a.is_working(now) && mates.iter().any(|m| m.is_working(now));
        let names: Vec<String> = mates.iter().map(|m| short_label(app, m)).collect();
        meta.push(Span::styled(
            format!(" · ⇄ same tree as {}", names.join(", ")),
            Style::new().fg(if clash { Color::Red } else { Color::Yellow }),
        ));
    }
    out.push(Line::from(meta));
    if a.host.as_ref().is_some_and(|h| h.kind.interactive()) && !app.ctl.quick.is_empty() {
        out.push(Line::styled(
            format!(" quick: {}", clip(&app.quick_hint(), w.saturating_sub(8))),
            DIM,
        ));
    }
    out.push(Line::styled(
        format!(" ─ full conversation {}", "─".repeat(w.saturating_sub(22))),
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
    // the recap is in the header ("what it did")
    summary_lines(s, &a.name, w, false)
}

/// You asked / Status / <agent> said, from a transcript summary.
fn summary_lines(
    s: &crate::transcript::Summary,
    agent: &str,
    w: usize,
    with_recap: bool,
) -> Vec<Line<'static>> {
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
    if let Some(r) = s.recap.as_ref().filter(|_| with_recap) {
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
    if let Some(why) = app.screen_error(a.pid) {
        return vec![Line::styled(format!(" {why}"), DIM)];
    }
    match app.screen_of(a.pid) {
        Some(rows) if !rows.is_empty() => rows.iter().map(|r| Line::raw(r.clone())).collect(),
        Some(_) => vec![Line::styled(" (the screen is empty)", DIM)],
        None if interactive => vec![Line::styled(" reading the screen…", DIM)],
        None => vec![Line::styled(
            format!(
                " marina can read the screen of {} sessions only — this is a {}",
                crate::hosts::TYPABLE,
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
