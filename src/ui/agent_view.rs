//! Drawing for agent control: the preview pane and the inbox table.

use ratatui::layout::Constraint;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, Paragraph, Row};

use super::control::{inbox_rank, last_active, PreviewMode};
use super::format::{fmt_age, unix_now};
use super::state::App;
use crate::model::Agent;

/// Inbox columns: SESSION, STATE, WHERE, AGO, LATEST (the rest).
pub(crate) fn inbox_header() -> Row<'static> {
    Row::new(["SESSION", "STATE", "WHERE", "AGO", "LATEST"].map(Cell::from))
        .style(Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD))
}

pub(crate) fn inbox_widths() -> Vec<Constraint> {
    vec![
        Constraint::Length(30),
        Constraint::Length(14),
        Constraint::Length(14),
        Constraint::Length(5),
        Constraint::Min(20),
    ]
}

/// One inbox row: who, what state, where, how long ago, and the latest
/// thing it said (or what it's waiting to do).
pub(crate) fn inbox_row(a: &Agent, app: &App) -> Row<'static> {
    let now = unix_now();
    let label = app.agent_label(a);
    let (state, style) = match inbox_rank(a, now) {
        0 => (
            "needs you".to_string(),
            Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        1 => ("done".to_string(), Style::new().fg(Color::Green)),
        2 => (a.activity(now), Style::new().fg(Color::Yellow)),
        4 => ("stale".to_string(), Style::new().fg(Color::DarkGray)),
        _ => ("idle".to_string(), Style::new().fg(Color::DarkGray)),
    };
    let s = a.summary.as_ref();
    let latest = if a.is_blocked() {
        s.and_then(|s| s.pending_tool.clone())
            .map(|t| format!("wants to run {t}"))
            .or_else(|| a.title().map(|t| format!("asks about: {t}")))
            .unwrap_or_else(|| "waiting on a question — p to see it".into())
    } else {
        s.and_then(|s| s.recap.as_deref())
            .map(plain)
            .or_else(|| s.and_then(|s| s.last_reply.as_deref()).map(first_line))
            .or_else(|| a.title().map(String::from))
            .unwrap_or_else(|| "—".into())
    };
    let last = last_active(a);
    let ago = if last == 0 {
        "—".into()
    } else {
        fmt_age(now.saturating_sub(last))
    };
    let dim = Style::new().fg(Color::DarkGray);
    Row::new(vec![
        Cell::from(Span::styled(label, Style::new().fg(Color::Magenta))),
        Cell::from(Span::styled(state, style)),
        Cell::from(match &a.host {
            Some(h) => Span::raw(h.short()),
            None => Span::styled("—", dim),
        }),
        Cell::from(Span::styled(ago, dim)),
        Cell::from(latest),
    ])
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

/// The preview pane for the selected agent, sized `width` x `height`
/// (outer, including the border).
pub(crate) fn preview_panel(app: &App, width: u16, height: u16) -> Paragraph<'static> {
    let inner_w = width.saturating_sub(2) as usize;
    let inner_h = height.saturating_sub(2) as usize;
    let Some(a) = app.selected_agent() else {
        return Paragraph::new(" select an agent session to preview it")
            .block(Block::bordered().title(" preview · Esc close "));
    };
    let label = app.agent_label(a);
    let waiting = app.waiting_on_you(a);
    let mode = app.ctl.preview.unwrap_or(PreviewMode::Reply);
    let key = Style::new().fg(Color::Cyan);
    let dim = Style::new().fg(Color::DarkGray);
    let interactive = a.host.as_ref().is_some_and(|h| h.kind.interactive());

    let mut head: Vec<Line<'static>> = Vec::new();
    if waiting {
        let what = a
            .summary
            .as_ref()
            .and_then(|s| s.pending_tool.clone())
            .map(|t| format!(" to run {t}"))
            .unwrap_or_default();
        head.push(Line::from(vec![
            Span::styled(
                format!(" ⚠ waiting on you{what}"),
                Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" — y approve · n reject · r reply", key),
        ]));
    }

    let body: Vec<Line<'static>> = match mode {
        PreviewMode::Screen => match app.screen_of(a.pid) {
            Some(rows) if !rows.is_empty() => rows.iter().map(|r| Line::raw(r.clone())).collect(),
            Some(_) => vec![Line::styled(" (the screen is empty)", dim)],
            None if interactive => vec![Line::styled(" reading the screen…", dim)],
            None => vec![Line::styled(
                format!(
                    " marina can read the screen of herdr and tmux panes only — this is a {}",
                    a.host
                        .as_ref()
                        .map(|h| h.describe())
                        .unwrap_or_else(|| "session with no terminal".into())
                ),
                dim,
            )],
        },
        PreviewMode::Reply => match &a.summary {
            None => vec![Line::styled(
                if interactive {
                    " no transcript for this session — p shows its screen"
                } else {
                    " no transcript for this session (marina reads Claude Code's)"
                },
                dim,
            )],
            Some(s) => {
                let mut lines = Vec::new();
                if let Some(p) = &s.last_prompt {
                    for (i, l) in wrap(p, inner_w.saturating_sub(7))
                        .into_iter()
                        .take(3)
                        .enumerate()
                    {
                        let k = if i == 0 { " you   " } else { "       " };
                        lines.push(Line::from(vec![Span::styled(k, key), Span::styled(l, dim)]));
                    }
                }
                if let Some(r) = &s.recap {
                    for (i, l) in wrap(r, inner_w.saturating_sub(7)).into_iter().enumerate() {
                        let k = if i == 0 { " recap " } else { "       " };
                        lines.push(Line::from(vec![
                            Span::styled(k, key),
                            Span::styled(l, Style::new().add_modifier(Modifier::ITALIC)),
                        ]));
                    }
                }
                if let Some(pr) = &s.pr_url {
                    lines.push(Line::from(vec![
                        Span::styled(" pr    ", key),
                        Span::raw(pr.clone()),
                    ]));
                }
                lines.push(Line::raw(""));
                let reply = s.last_reply.as_deref().unwrap_or("(no reply yet)");
                lines.extend(wrap(reply, inner_w.saturating_sub(2)).into_iter().map(|l| {
                    let mut spans = vec![Span::raw(" ")];
                    spans.extend(md_spans(&l));
                    Line::from(spans)
                }));
                lines
            }
        },
    };

    // keep the head; show the end of the body (where agents ask things)
    let room = inner_h.saturating_sub(head.len());
    let mut lines = head;
    if body.len() > room {
        let skip = body.len() - room + 1;
        lines.push(Line::styled(format!(" … {skip} lines above"), dim));
        lines.extend(body.into_iter().skip(skip));
    } else {
        lines.extend(body);
    }

    let other = match (mode, interactive, a.summary.is_some()) {
        (PreviewMode::Reply, true, _) => " · p screen",
        (PreviewMode::Screen, _, true) => " · p close",
        _ => "",
    };
    let what = match mode {
        PreviewMode::Reply => "last reply",
        PreviewMode::Screen => "screen",
    };
    let title = match a.title() {
        Some(t) => format!(" {what} · {label} — {t}{other} · Esc close "),
        None => format!(" {what} · {label}{other} · Esc close "),
    };
    Paragraph::new(lines).block(Block::bordered().title(title))
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
