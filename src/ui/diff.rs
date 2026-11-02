//! The `d` pane: a work tree's uncommitted changes, one file at a time.
//! Two views, `Tab` between them — the changed files (status, path, +/−),
//! and the selected file's patch with line numbers, red/green lines and the
//! changed part of a replaced line picked out. Colours are the terminal's
//! own, and emphasis is reverse video, so it reads on light and dark themes.
//!
//! The diff is live: while the pane is open it is re-read every few
//! seconds, and a change keeps the cursor on the same file.

use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::git::FileDiff;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum DiffMode {
    Files,
    Patch,
}

/// How often an open diff is re-read.
pub(crate) const REFRESH: Duration = Duration::from_secs(3);
/// How long the title says a re-read changed something.
pub(crate) const UPDATED: Duration = Duration::from_secs(3);

pub(crate) struct DiffView {
    /// The work tree shown; the pane reloads when the selection's differs.
    pub(crate) dir: PathBuf,
    /// `None` until the first read lands.
    pub(crate) files: Option<Vec<FileDiff>>,
    /// A read in flight.
    pub(crate) rx: Option<Receiver<Vec<FileDiff>>>,
    /// When the last read landed (the next is due `REFRESH` after).
    pub(crate) read_at: Instant,
    /// When a re-read last changed what's shown.
    pub(crate) changed_at: Option<Instant>,
    pub(crate) cursor: usize,
    pub(crate) mode: DiffMode,
}

/// What a re-read did to the view.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Landed {
    First,
    Same,
    /// The shown file is still there, with these lines changed or not.
    Kept {
        file_changed: bool,
    },
    /// The shown file is gone; the cursor moved.
    Moved,
}

impl DiffView {
    pub(crate) fn new(dir: PathBuf, rx: Receiver<Vec<FileDiff>>) -> Self {
        DiffView {
            dir,
            files: None,
            rx: Some(rx),
            read_at: Instant::now(),
            changed_at: None,
            cursor: 0,
            mode: DiffMode::Files,
        }
    }

    /// Take in a read: the first sets the view, a later one replaces the
    /// files but keeps the cursor on the file it was on, if it's still there.
    pub(crate) fn land(&mut self, files: Vec<FileDiff>, now: Instant) -> Landed {
        self.rx = None;
        self.read_at = now;
        let Some(old) = self.files.take() else {
            self.files = Some(files);
            return Landed::First;
        };
        if old == files {
            self.files = Some(old);
            return Landed::Same;
        }
        self.changed_at = Some(now);
        let was = old.get(self.cursor);
        let landed = match was.and_then(|w| files.iter().position(|f| f.path == w.path)) {
            Some(i) => {
                let file_changed = Some(&files[i]) != was;
                self.cursor = i;
                Landed::Kept { file_changed }
            }
            None => {
                self.cursor = self.cursor.min(files.len().saturating_sub(1));
                if files.is_empty() {
                    self.mode = DiffMode::Files;
                }
                Landed::Moved
            }
        };
        self.files = Some(files);
        landed
    }

    /// A re-read is due.
    pub(crate) fn due(&self, now: Instant) -> bool {
        self.rx.is_none() && now.duration_since(self.read_at) >= REFRESH
    }

    /// The title's "updated" note is showing.
    pub(crate) fn recently_changed(&self, now: Instant) -> bool {
        self.changed_at
            .is_some_and(|t| now.duration_since(t) < UPDATED)
    }

    /// Where the next (`fwd`) or previous hunk header is from line `top`.
    pub(crate) fn hunk_from(&self, top: usize, fwd: bool) -> Option<usize> {
        let lines = &self.file()?.lines;
        let is_hunk = |i: &usize| lines[*i].starts_with("@@");
        if fwd {
            (top + 1..lines.len()).find(is_hunk)
        } else {
            (0..top.min(lines.len())).rev().find(is_hunk)
        }
    }

    /// The last hunk header of the shown file.
    pub(crate) fn last_hunk(&self) -> usize {
        self.file()
            .and_then(|f| f.lines.iter().rposition(|l| l.starts_with("@@")))
            .unwrap_or(0)
    }

    pub(crate) fn file(&self) -> Option<&FileDiff> {
        self.files.as_ref()?.get(self.cursor)
    }

    pub(crate) fn len(&self) -> usize {
        self.files.as_ref().map_or(0, Vec::len)
    }

    /// Rows in the current view: one per file, or the patch's lines.
    pub(crate) fn rows(&self) -> usize {
        match self.mode {
            DiffMode::Files => self.len(),
            DiffMode::Patch => self.file().map_or(0, |f| f.lines.len()),
        }
    }
}

/// The first file row shown in a list `h` rows tall, so the cursor stays
/// in view.
pub(crate) fn files_offset(cursor: usize, h: usize) -> usize {
    cursor.saturating_sub(h.saturating_sub(1))
}

fn counts(f: &FileDiff) -> String {
    match (f.added, f.removed) {
        (0, 0) => String::new(),
        (a, 0) => format!("+{a}"),
        (0, r) => format!("−{r}"),
        (a, r) => format!("+{a} −{r}"),
    }
}

fn status_style(c: char) -> Style {
    Style::new().fg(match c {
        'A' | '?' => Color::Green,
        'D' => Color::Red,
        'R' => Color::Cyan,
        _ => Color::Yellow,
    })
}

/// Keep the end of a path that doesn't fit — the file name matters most.
fn clip_left(s: &str, w: usize) -> String {
    let n = s.chars().count();
    if n <= w {
        return s.to_string();
    }
    let keep: String = s.chars().skip(n + 1 - w.max(1)).collect();
    format!("…{keep}")
}

/// The files view, `h` rows of it, `w` wide.
pub(crate) fn file_rows(d: &DiffView, w: usize, h: usize) -> Vec<Line<'static>> {
    let Some(files) = &d.files else {
        return vec![Line::styled("  reading…", Style::new().fg(Color::DarkGray))];
    };
    if files.is_empty() {
        return vec![Line::styled(
            "  no uncommitted changes",
            Style::new().fg(Color::DarkGray),
        )];
    }
    let cw = files
        .iter()
        .map(|f| counts(f).chars().count())
        .max()
        .unwrap_or(0);
    // "▌ " + status + " " … path … " " + counts
    let shown = |f: &FileDiff| match &f.from {
        Some(from) => format!("{from} → {}", f.path),
        None => f.path.clone(),
    };
    // counts line up after the longest path, or at the right edge
    let longest = files
        .iter()
        .map(|f| shown(f).chars().count())
        .max()
        .unwrap_or(0);
    let pw = w
        .saturating_sub(2 + 2 + 2 + cw)
        .clamp(8.min(longest), longest.max(1));
    files
        .iter()
        .enumerate()
        .skip(files_offset(d.cursor, h))
        .take(h)
        .map(|(i, f)| {
            let path = clip_left(&shown(f), pw);
            let pad = pw.saturating_sub(path.chars().count());
            let mut spans = vec![
                Span::raw(if i == d.cursor { "▌ " } else { "  " }),
                Span::styled(format!("{} ", f.status), status_style(f.status)),
                Span::raw(path),
                Span::raw(" ".repeat(pad + 2)),
            ];
            let c = counts(f);
            let lead = cw - c.chars().count();
            spans.push(Span::raw(" ".repeat(lead)));
            if let Some((a, r)) = c.split_once(' ') {
                spans.push(Span::styled(a.to_string(), Style::new().fg(Color::Green)));
                spans.push(Span::raw(" "));
                spans.push(Span::styled(r.to_string(), Style::new().fg(Color::Red)));
            } else if c.starts_with('+') {
                spans.push(Span::styled(c, Style::new().fg(Color::Green)));
            } else {
                spans.push(Span::styled(c, Style::new().fg(Color::Red)));
            }
            let line = Line::from(spans);
            if i == d.cursor {
                line.style(Style::new().add_modifier(Modifier::REVERSED))
            } else {
                line
            }
        })
        .collect()
}

/// `@@ -a,b +c,d @@` → (a, c).
fn hunk_starts(h: &str) -> Option<(usize, usize)> {
    let mut it = h.split_whitespace().skip(1);
    let num = |s: &str| s[1..].split(',').next()?.parse().ok();
    Some((num(it.next()?)?, num(it.next()?)?))
}

/// The changed middle of `a` and `b` once their common start and end are
/// set aside, as byte ranges — or `None` when they share too little for
/// that to mean anything.
fn changed_middle(a: &str, b: &str) -> Option<((usize, usize), (usize, usize))> {
    let pre = a
        .char_indices()
        .zip(b.chars())
        .find(|((_, x), y)| x != y)
        .map_or(a.len().min(b.len()), |((i, _), _)| i);
    // `pre` is a char boundary in both (the shared prefix is identical)
    let (ra, rb) = (&a[pre..], &b[pre..]);
    let mut suf = ra
        .chars()
        .rev()
        .zip(rb.chars().rev())
        .take_while(|(x, y)| x == y)
        .map(|(x, _)| x.len_utf8())
        .sum::<usize>();
    // whole words: `false` → `true`, not `fals` → `tru`
    let word = |c: char| c.is_alphanumeric() || c == '_';
    let mut pre = pre;
    while let Some(c) = a[..pre].chars().next_back().filter(|&c| word(c)) {
        pre -= c.len_utf8();
    }
    let joined = |s: &str, suf: usize| {
        s[..s.len() - suf].chars().next_back().is_some_and(word)
            && s[s.len() - suf..].chars().next().is_some_and(word)
    };
    while suf > 0 && (joined(a, suf) || joined(b, suf)) {
        suf -= a[a.len() - suf..].chars().next().map_or(1, char::len_utf8);
    }
    let common = pre + suf;
    let longer = a.len().max(b.len());
    if common == longer || common * 3 < longer {
        return None; // identical, or rewritten wholesale
    }
    Some(((pre, a.len() - suf), (pre, b.len() - suf)))
}

fn expand_tabs(s: &str) -> String {
    s.replace('\t', "    ")
}

/// The selected file's patch, every line (the caller scrolls).
pub(crate) fn patch_lines(f: &FileDiff) -> Vec<Line<'static>> {
    let dim = Style::new().fg(Color::DarkGray);
    if f.lines.is_empty() {
        return vec![Line::styled("  no changes to show", dim)];
    }
    let body: Vec<String> = f.lines.iter().map(|l| expand_tabs(l)).collect();

    // line numbers, and the emphasis for paired −/+ runs
    let mut nums: Vec<(Option<usize>, Option<usize>)> = Vec::with_capacity(body.len());
    let mut emph: Vec<Option<(usize, usize)>> = vec![None; body.len()];
    let (mut old, mut new, mut top) = (0, 0, 0);
    let mut i = 0;
    while i < body.len() {
        let l = &body[i];
        match l.as_bytes().first() {
            Some(b'@') => {
                (old, new) = hunk_starts(l).unwrap_or((0, 0));
                nums.push((None, None));
            }
            Some(b'-') => {
                // a run of removals and the additions right after it
                let rm = i;
                while i < body.len() && body[i].starts_with('-') {
                    nums.push((Some(old), None));
                    old += 1;
                    i += 1;
                }
                let ad = i;
                while i < body.len() && body[i].starts_with('+') {
                    nums.push((None, Some(new)));
                    new += 1;
                    i += 1;
                }
                for k in 0..(ad - rm).min(i - ad) {
                    let (a, b) = (&body[rm + k][1..], &body[ad + k][1..]);
                    if let Some((ra, rb)) = changed_middle(a, b) {
                        emph[rm + k] = Some(ra);
                        emph[ad + k] = Some(rb);
                    }
                }
                top = top.max(old).max(new);
                continue;
            }
            Some(b'+') => {
                nums.push((None, Some(new)));
                new += 1;
            }
            Some(b'\\') => nums.push((None, None)),
            _ => {
                nums.push((Some(old), Some(new)));
                old += 1;
                new += 1;
            }
        }
        top = top.max(old).max(new);
        i += 1;
    }
    let w = top.max(1).to_string().len();
    let num = |n: Option<usize>| n.map_or(" ".repeat(w), |n| format!("{n:>w$}"));

    body.iter()
        .zip(nums)
        .zip(emph)
        .map(|((l, (o, n)), e)| {
            let gutter = Span::styled(format!("{} {} │ ", num(o), num(n)), dim);
            match l.as_bytes().first() {
                Some(b'@') => {
                    // `@@ -12,9 +12,13 @@ fn ctx` → the context, set apart
                    let mut parts = l.splitn(3, "@@").skip(1).map(str::trim);
                    let (range, ctx) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
                    let cyan = Style::new().fg(Color::Cyan);
                    Line::from(vec![
                        Span::styled(format!("{} {} ┊ ", " ".repeat(w), " ".repeat(w)), dim),
                        Span::styled(format!("{range} "), cyan),
                        Span::styled(ctx.to_string(), cyan.add_modifier(Modifier::BOLD)),
                    ])
                }
                Some(c @ (b'+' | b'-')) => {
                    let st = Style::new().fg(if *c == b'+' { Color::Green } else { Color::Red });
                    let text = &l[1..];
                    let sign = Span::styled(format!("{} ", *c as char), st);
                    let mut spans = vec![gutter, sign];
                    match e {
                        Some((s, end)) => {
                            spans.push(Span::styled(text[..s].to_string(), st));
                            spans.push(Span::styled(
                                text[s..end].to_string(),
                                st.add_modifier(Modifier::REVERSED),
                            ));
                            spans.push(Span::styled(text[end..].to_string(), st));
                        }
                        None => spans.push(Span::styled(text.to_string(), st)),
                    }
                    Line::from(spans)
                }
                Some(b'\\') => Line::from(vec![
                    gutter,
                    Span::styled(
                        l[1..].trim().to_string(),
                        dim.add_modifier(Modifier::ITALIC),
                    ),
                ]),
                _ => Line::from(vec![
                    gutter,
                    Span::raw(format!("  {}", l.get(1..).unwrap_or(""))),
                ]),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(l: &Line) -> String {
        l.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn the_changed_middle_of_a_replaced_line() {
        let a = "  payments: false,";
        let b = "  payments: true,";
        let ((s, e), (s2, e2)) = changed_middle(a, b).unwrap();
        assert_eq!((&a[s..e], &b[s2..e2]), ("false", "true"));
        let (a, b) = ("x = user.cards;", "x = user.savedCards;");
        let ((s, e), (s2, e2)) = changed_middle(a, b).unwrap();
        assert_eq!((&a[s..e], &b[s2..e2]), ("cards", "savedCards"));
        assert!(changed_middle("abc", "xyz").is_none(), "rewritten");
        assert!(changed_middle("same", "same").is_none());
        // multibyte stays on char boundaries
        let ((s, e), _) = changed_middle("é→a", "é→b").unwrap();
        assert_eq!(&"é→a"[s..e], "a");
    }

    #[test]
    fn a_patch_gets_line_numbers() {
        let f = FileDiff {
            path: "x".into(),
            status: 'M',
            added: 1,
            removed: 1,
            lines: [
                "@@ -9,3 +9,3 @@ fn main()",
                " keep",
                "-old line",
                "+new line",
                " tail",
            ]
            .map(String::from)
            .to_vec(),
            from: None,
        };
        let out: Vec<String> = patch_lines(&f).iter().map(text).collect();
        assert!(out[0].contains("-9,3 +9,3 fn main()"), "{out:?}");
        assert_eq!(out[1], " 9  9 │   keep");
        assert_eq!(out[2], "10    │ - old line");
        assert_eq!(out[3], "   10 │ + new line");
        assert_eq!(out[4], "11 11 │   tail");
    }

    fn file(path: &str, lines: &[&str]) -> FileDiff {
        FileDiff {
            path: path.into(),
            status: 'M',
            lines: lines.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn a_reread_keeps_the_cursor_on_its_file() {
        let (_tx, rx) = std::sync::mpsc::channel();
        let mut d = DiffView::new("/w".into(), rx);
        let now = Instant::now();
        let a = file("a", &["+1"]);
        let b = file("b", &["+2"]);
        assert_eq!(d.land(vec![a.clone(), b.clone()], now), Landed::First);
        d.cursor = 1;
        assert_eq!(d.land(vec![a.clone(), b.clone()], now), Landed::Same);
        assert!(!d.recently_changed(now));
        // a new file sorts in ahead of b: the cursor follows b
        let z = file("aa", &["+3"]);
        let got = d.land(vec![a.clone(), z.clone(), b.clone()], now);
        assert_eq!(
            got,
            Landed::Kept {
                file_changed: false
            }
        );
        assert_eq!(d.file().unwrap().path, "b");
        assert!(d.recently_changed(now));
        // b changes
        let b2 = file("b", &["+2", "+4"]);
        let got = d.land(vec![a.clone(), z.clone(), b2], now);
        assert_eq!(got, Landed::Kept { file_changed: true });
        // b is committed away: the cursor stays in range
        assert_eq!(d.land(vec![a], now), Landed::Moved);
        assert_eq!(d.cursor, 0);
        assert!(!d.due(now), "a read just landed");
        assert!(d.due(now + REFRESH));
    }

    #[test]
    fn hunks_are_found_either_way() {
        let (_tx, rx) = std::sync::mpsc::channel();
        let mut d = DiffView::new("/w".into(), rx);
        d.land(
            vec![file("a", &["@@ 1", " x", "@@ 2", " y", "@@ 3"])],
            Instant::now(),
        );
        assert_eq!(d.hunk_from(0, true), Some(2));
        assert_eq!(d.hunk_from(2, true), Some(4));
        assert_eq!(d.hunk_from(4, true), None);
        assert_eq!(d.hunk_from(3, false), Some(2));
        assert_eq!(d.hunk_from(0, false), None);
        assert_eq!(d.last_hunk(), 4);
    }

    #[test]
    fn the_file_list_keeps_the_cursor_in_view() {
        assert_eq!(files_offset(0, 5), 0);
        assert_eq!(files_offset(4, 5), 0);
        assert_eq!(files_offset(7, 5), 3);
        assert_eq!(clip_left("src/very/long/name.rs", 10), "…g/name.rs");
    }
}
