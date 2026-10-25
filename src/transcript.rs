//! What a Claude Code session last said: read from the tail of its
//! transcript (`~/.claude/projects/<dir>/<session>.jsonl`), never the whole
//! file — they run to tens of megabytes. Only the summary fields below are
//! kept; nothing is written, sent anywhere, or logged.
//!
//! The transcript is append-only JSONL. From the end backwards: the newest
//! `ai-title` (the task), `last-prompt` (what the human last asked), the
//! last assistant text (its reply), an `away_summary` written after that
//! reply (Claude's own recap), a `pr-link`, and — when the last message is
//! an assistant `tool_use` with no result yet — the tool it is running or
//! waiting for permission to run.
//!
//! Can be turned off with `[agents] transcripts = false`.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use serde_json::Value;

/// Bytes read from the end first; grown once if no reply turns up in it.
const TAIL: u64 = 256 * 1024;
const TAIL_MAX: u64 = 4 * 1024 * 1024;
/// A changed transcript is re-read at most this often.
const MIN_REREAD: Duration = Duration::from_secs(2);
/// opencode summaries come from a subprocess: this often at most.
const OPENCODE_EVERY: Duration = Duration::from_secs(5);
/// Kept per field: enough for a preview pane, bounded for `--json`.
const MAX_TEXT: usize = 8_000;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Summary {
    /// Claude's generated title for the session.
    pub title: Option<String>,
    /// The human's most recent prompt.
    pub last_prompt: Option<String>,
    /// The assistant's most recent text.
    pub last_reply: Option<String>,
    /// Claude's recap, when written after the last reply.
    pub recap: Option<String>,
    /// The tool call with no result yet: `Bash: cargo test`. Running, or —
    /// when the session is blocked — waiting for approval.
    pub pending_tool: Option<String>,
    /// Unix seconds that tool call was made.
    pub pending_since: Option<u64>,
    /// A turn is under way: the newest record is your prompt or a tool
    /// result, or a tool call — not the `system` record a turn ends with.
    /// (Claude Code doesn't always flush a tool call before asking to run
    /// it, so this catches an approval prompt `pending_tool` misses.)
    pub in_turn: bool,
    pub pr_url: Option<String>,
    /// Unix seconds of the newest record with a timestamp.
    pub updated: Option<u64>,
    /// Tokens in its context window as of the newest assistant message
    /// (input + cache read + cache write), and that message's model.
    pub context_tokens: Option<u64>,
    pub model: Option<String>,
    /// The session's working directory, as its newest record says.
    pub cwd: Option<PathBuf>,
    /// Unix seconds its last turn ended — what the inbox orders by, so a
    /// session doesn't move while it works, only when it finishes.
    pub turn_ended: Option<u64>,
}

/// Sentences of `text`, markdown marks dropped: split at `.`/`?`/`!`
/// followed by a space or line end, and at paragraph breaks.
fn sentences(text: &str) -> Vec<String> {
    let plain = text.replace("**", "").replace(['*', '`'], "");
    let mut out = Vec::new();
    for para in plain.split("\n\n") {
        let para = para.split_whitespace().collect::<Vec<_>>().join(" ");
        let mut cur = String::new();
        let chars: Vec<char> = para.chars().collect();
        for (i, &c) in chars.iter().enumerate() {
            cur.push(c);
            let end =
                matches!(c, '.' | '?' | '!') && chars.get(i + 1).is_none_or(|n| n.is_whitespace());
            if end {
                let t = cur.trim().to_string();
                if !t.is_empty() {
                    out.push(t);
                }
                cur.clear();
            }
        }
        let t = cur.trim().trim_end_matches(':').to_string();
        if !t.is_empty() {
            out.push(t);
        }
    }
    out
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// Imperatives addressed to the human that end a reply: "Say push when…".
const ASKING: [&str; 14] = [
    "Say ",
    "Just say",
    "Tell me",
    "Send ",
    "Let me know",
    "Reply ",
    "Pick ",
    "Choose ",
    "Confirm ",
    "Approve ",
    "Decide ",
    "Open ",
    "Run ",
    "Try ",
];

impl Summary {
    /// What the session wants from you, in a line: the "Next…" of Claude's
    /// recap, else the question (or instruction to you) its reply ends with.
    /// `None` when it's just reporting.
    pub fn ask(&self) -> Option<String> {
        if let Some(r) = &self.recap {
            let lower = r.to_lowercase();
            if lower.contains("nothing is pending") || lower.contains("nothing needed") {
                return None;
            }
            for s in sentences(r) {
                for p in ["Next step:", "Next up:", "Next is ", "Next:", "Next,"] {
                    if let Some(rest) = s.strip_prefix(p) {
                        let rest = rest
                            .trim()
                            .trim_end_matches("(disable recaps in /config)")
                            .trim();
                        if !rest.is_empty() {
                            return Some(capitalize(rest));
                        }
                    }
                }
            }
        }
        let reply = self.last_reply.as_deref()?;
        let last_para = reply.trim_end().rsplit("\n\n").next().unwrap_or(reply);
        let ss = sentences(last_para);
        let last = ss.last()?;
        if last.ends_with('?') {
            // "Shall I begin?" alone says little: add what it's about,
            // question first so it scans
            if last.chars().count() < 45 && ss.len() > 1 {
                return Some(format!("{last} — {}", ss[ss.len() - 2]));
            }
            return Some(last.clone());
        }
        ss.iter()
            .rev()
            .take(2)
            .find(|s| ASKING.iter().any(|p| s.starts_with(p)))
            .cloned()
    }

    /// What it did, in a line or two: the recap before its "Next…", else
    /// the reply's opening sentence.
    pub fn did(&self) -> Option<String> {
        if let Some(r) = &self.recap {
            let before: Vec<String> = sentences(r)
                .into_iter()
                .take_while(|s| !s.starts_with("Next"))
                .take(2)
                .collect();
            if !before.is_empty() {
                return Some(before.join(" "));
            }
        }
        sentences(self.last_reply.as_deref()?).into_iter().next()
    }
}

/// An agent session that has exited but could be resumed.
#[derive(Clone, Debug, PartialEq)]
pub struct Ended {
    /// `claude`, `codex` or `opencode`: how to resume it.
    pub agent: String,
    pub id: String,
    /// When its transcript was last written: roughly when it ended.
    pub at: u64,
    pub cwd: PathBuf,
    pub summary: Summary,
}

/// Ended sessions listed: the most recent this many…
const ENDED_MAX: usize = 8;
/// …from this far back…
const ENDED_WITHIN: u64 = 2 * 86_400;
/// …looked for at most this often.
const ENDED_EVERY: Duration = Duration::from_secs(30);

/// The transcript tail, newest record first. Lines are filtered by their
/// type before parsing: request blobs can be hundreds of kilobytes each.
pub fn parse_tail(text: &str) -> Summary {
    parse_tail_of(text, false)
}

/// [`parse_tail`] of a session's own records, or (`sidechain`) of a
/// subagent's transcript, whose records are all marked as its sidechain.
fn parse_tail_of(text: &str, sidechain: bool) -> Summary {
    let mut s = Summary::default();
    // set once the newest message (assistant/user) has been seen
    let mut seen_message = false;
    // the newest of user / assistant / system decides `in_turn`
    let mut turn_known = false;
    for line in text.lines().rev() {
        // metadata records lead with their type; skip the bulky ones
        // (request blobs run to hundreds of kilobytes) unparsed
        if let Some(t) = leading_type(line) {
            if !matches!(
                t,
                "ai-title" | "last-prompt" | "pr-link" | "system" | "user" | "assistant"
            ) {
                continue;
            }
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue; // the partial first line of the window, or garbage
        };
        let kind = v["type"].as_str();
        if v["isSidechain"].as_bool().unwrap_or(false) != sidechain {
            continue; // a subagent's turn in the session's own transcript
        }
        if s.updated.is_none() {
            s.updated = v["timestamp"].as_str().and_then(parse_ts);
        }
        if s.cwd.is_none() {
            s.cwd = v["cwd"].as_str().map(PathBuf::from);
        }
        if s.turn_ended.is_none() && kind == Some("system") && v["subtype"] == "turn_duration" {
            s.turn_ended = v["timestamp"].as_str().and_then(parse_ts);
        }
        if !turn_known && matches!(kind, Some("user" | "assistant" | "system")) {
            turn_known = true;
            s.in_turn = match kind {
                Some("user") => true,
                Some("assistant") => v["message"]["content"]
                    .as_array()
                    .is_some_and(|b| b.iter().any(|b| b["type"] == "tool_use")),
                _ => false, // a turn ends with a system record
            };
        }
        match kind {
            Some("ai-title") if s.title.is_none() => s.title = text_of(&v["aiTitle"]),
            Some("last-prompt") if s.last_prompt.is_none() => {
                s.last_prompt = text_of(&v["lastPrompt"])
            }
            Some("pr-link") if s.pr_url.is_none() => s.pr_url = text_of(&v["prUrl"]),
            // a recap counts only if nothing was said after it
            Some("system")
                if v["subtype"] == "away_summary"
                    && s.recap.is_none()
                    && s.last_reply.is_none()
                    && !seen_message =>
            {
                s.recap = text_of(&v["content"])
            }
            Some("user") => seen_message = true,
            Some("assistant") => {
                if s.context_tokens.is_none() {
                    let u = &v["message"]["usage"];
                    let n = [
                        "input_tokens",
                        "cache_read_input_tokens",
                        "cache_creation_input_tokens",
                    ]
                    .iter()
                    .filter_map(|k| u[*k].as_u64())
                    .sum::<u64>();
                    if n > 0 {
                        s.context_tokens = Some(n);
                        s.model = text_of(&v["message"]["model"]);
                    }
                }
                let content = v["message"]["content"].as_array();
                let blocks = content.map(Vec::as_slice).unwrap_or_default();
                if !seen_message {
                    // the newest message: a tool call still without a result
                    s.pending_tool = blocks
                        .iter()
                        .rev()
                        .find(|b| b["type"] == "tool_use")
                        .map(describe_tool);
                    if s.pending_tool.is_some() {
                        s.pending_since = v["timestamp"].as_str().and_then(parse_ts);
                    }
                }
                seen_message = true;
                if s.last_reply.is_none() {
                    let text: Vec<&str> = blocks
                        .iter()
                        .filter(|b| b["type"] == "text")
                        .filter_map(|b| b["text"].as_str())
                        .collect();
                    if !text.is_empty() {
                        s.last_reply = Some(clip(text.join("\n\n").trim()));
                    }
                }
            }
            _ => {}
        }
        let done = s.title.is_some()
            && s.last_prompt.is_some()
            && s.last_reply.is_some()
            && s.context_tokens.is_some();
        if done && seen_message {
            break;
        }
    }
    s
}

/// A metadata record's type, read without parsing: those lines start with
/// it (`{"type":"ai-title",…`). Message records start `{"parentUuid"…`.
fn leading_type(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("{\"type\":\"")?;
    Some(&rest[..rest.find('"')?])
}

fn text_of(v: &Value) -> Option<String> {
    v.as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(clip)
}

fn clip(s: &str) -> String {
    match s.char_indices().nth(MAX_TEXT) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

/// `Bash: cargo test --all`, `Edit: src/main.rs`, `WebFetch: https://…`.
fn describe_tool(b: &Value) -> String {
    let name = b["name"].as_str().unwrap_or("tool");
    let input = &b["input"];
    let arg = [
        "command",
        "file_path",
        "path",
        "url",
        "pattern",
        "description",
    ]
    .iter()
    .find_map(|k| input[*k].as_str());
    match arg {
        Some(a) => {
            let first = a.lines().next().unwrap_or("").trim();
            let first: String = first.chars().take(160).collect();
            format!("{name}: {first}")
        }
        None => name.to_string(),
    }
}

/// `2026-10-20T21:48:59.114Z` -> unix seconds (UTC).
pub(crate) fn parse_ts(ts: &str) -> Option<u64> {
    let num = |r: std::ops::Range<usize>| ts.get(r)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, se) = (num(11..13)?, num(14..16)?, num(17..19)?);
    // days from civil (Howard Hinnant's algorithm)
    let y = if mo <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((mo + 9) % 12) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days * 86_400 + h * 3600 + mi * 60 + se).ok()
}

pub(crate) fn read_tail(path: &Path, bytes: u64) -> Option<String> {
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let start = len.saturating_sub(bytes);
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::with_capacity((len - start) as usize);
    f.read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// `prev` is the last summary of the same file: a reply older than the
/// window is the one already found, so a long tool-heavy stretch doesn't
/// re-read megabytes every couple of seconds.
fn summarize(path: &Path, prev: Option<&Summary>) -> Option<Summary> {
    let len = std::fs::metadata(path).ok()?.len();
    let mut s = parse_tail(&read_tail(path, TAIL)?);
    if s.last_reply.is_some() || len <= TAIL {
        return Some(s);
    }
    if let Some(p) = prev.filter(|p| p.last_reply.is_some()) {
        s.last_reply = p.last_reply.clone();
        s.title = s.title.or_else(|| p.title.clone());
        s.last_prompt = s.last_prompt.or_else(|| p.last_prompt.clone());
        s.pr_url = s.pr_url.or_else(|| p.pr_url.clone());
        s.context_tokens = s.context_tokens.or(p.context_tokens);
        s.model = s.model.or_else(|| p.model.clone());
        return Some(s);
    }
    // a long tool-heavy stretch since the last reply: look further back,
    // keeping the newer window's fields
    let wide = parse_tail(&read_tail(path, TAIL_MAX)?);
    Some(Summary {
        title: s.title.or(wide.title),
        last_prompt: s.last_prompt.or(wide.last_prompt),
        last_reply: wide.last_reply,
        recap: s.recap.or(wide.recap),
        pending_tool: s.pending_tool,
        pending_since: s.pending_since,
        in_turn: s.in_turn,
        pr_url: s.pr_url.or(wide.pr_url),
        updated: s.updated.or(wide.updated),
        context_tokens: s.context_tokens.or(wide.context_tokens),
        model: s.model.or(wide.model),
        cwd: s.cwd.or(wide.cwd),
        turn_ended: s.turn_ended.or(wide.turn_ended),
    })
}

/// Claude Code keeps a session's transcript under its starting directory,
/// encoded (`/Users/me/dev/app` → `-Users-me-dev-app`). Is session `id`'s
/// transcript there for `dir`? `None` when transcripts can't be checked.
pub fn belongs_to(id: &str, dir: &Path) -> Option<bool> {
    if !crate::launcher::is_session_id(id) {
        return Some(false);
    }
    let projects = claude_dir()?.join("projects");
    if !projects.is_dir() {
        return None;
    }
    let enc: String = dir
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    Some(projects.join(enc).join(format!("{id}.jsonl")).is_file())
}

/// Claude Code transcripts (top-level sessions, not subagents') written
/// at or after `start` (unix seconds).
pub fn written_since(start: u64) -> Vec<PathBuf> {
    let Some(projects) = claude_dir().map(|d| d.join("projects")) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for dir in std::fs::read_dir(projects).into_iter().flatten().flatten() {
        for f in std::fs::read_dir(dir.path())
            .into_iter()
            .flatten()
            .flatten()
        {
            let p = f.path();
            let fresh = f
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .is_some_and(|d| d.as_secs() >= start);
            let ok_name = p.extension().is_some_and(|x| x == "jsonl")
                && p.file_stem()
                    .and_then(|s| s.to_str())
                    .is_some_and(crate::launcher::is_session_id);
            if fresh && ok_name {
                out.push(p);
            }
        }
    }
    out
}

/// The first `cwd` a transcript records: where its session started.
fn first_cwd(path: &Path) -> Option<PathBuf> {
    let f = std::fs::File::open(path).ok()?;
    let mut buf = Vec::new();
    f.take(64 * 1024).read_to_end(&mut buf).ok()?;
    String::from_utf8_lossy(&buf)
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find_map(|v| v["cwd"].as_str().map(PathBuf::from))
}

/// Codex: the tail of its rollout (`~/.codex/sessions/…/rollout-…-<id>.jsonl`)
/// and the `session_meta` line it starts with (for the working directory).
pub fn parse_codex(meta: &str, tail: &str) -> Summary {
    let mut s = Summary::default();
    let mut turn_known = false;
    for line in tail.lines().rev() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if s.updated.is_none() {
            s.updated = v["timestamp"].as_str().and_then(parse_ts);
        }
        let p = &v["payload"];
        match (v["type"].as_str(), p["type"].as_str()) {
            (Some("event_msg"), Some("task_complete")) => {
                if s.turn_ended.is_none() {
                    s.turn_ended = v["timestamp"].as_str().and_then(parse_ts);
                }
                if !turn_known {
                    turn_known = true;
                    s.in_turn = false;
                }
                if s.last_reply.is_none() {
                    s.last_reply = text_of(&p["last_agent_message"]);
                }
            }
            (Some("event_msg"), Some("task_started")) if !turn_known => {
                turn_known = true;
                s.in_turn = true;
            }
            (Some("event_msg"), Some("user_message")) => {
                if !turn_known {
                    turn_known = true;
                    s.in_turn = true;
                }
                if s.last_prompt.is_none() {
                    s.last_prompt = text_of(&p["message"]);
                }
            }
            (Some("event_msg"), Some("token_count")) if s.context_tokens.is_none() => {
                s.context_tokens = p["info"]["last_token_usage"]["input_tokens"].as_u64();
            }
            (Some("response_item"), Some("message"))
                if p["role"] == "assistant" && s.last_reply.is_none() =>
            {
                let text: Vec<&str> = p["content"]
                    .as_array()
                    .map(Vec::as_slice)
                    .unwrap_or_default()
                    .iter()
                    .filter_map(|c| c["text"].as_str())
                    .collect();
                if !text.is_empty() {
                    s.last_reply = Some(clip(text.join("\n\n").trim()));
                }
            }
            _ => {}
        }
        if turn_known
            && s.last_reply.is_some()
            && s.last_prompt.is_some()
            && s.context_tokens.is_some()
        {
            break;
        }
    }
    if let Some(m) = meta
        .lines()
        .next()
        .and_then(|l| serde_json::from_str::<Value>(l).ok())
    {
        s.cwd = m["payload"]["cwd"].as_str().map(PathBuf::from);
        s.model = text_of(&m["payload"]["model_provider"]);
    }
    s
}

/// A subagent a session started: Claude Code's Agent tool, a Codex
/// spawned thread. Its work happens inside the session's own process.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Subagent {
    pub id: String,
    /// What it was asked to do (Claude Code's description; Codex's
    /// nickname, else its first prompt).
    pub task: String,
    /// `Explore`, `general-purpose`, a Codex role.
    pub kind: Option<String>,
    pub background: bool,
    pub running: bool,
    /// Unix seconds: its first and newest records.
    pub started: Option<u64>,
    pub updated: u64,
    /// The tool call it's on, while running.
    pub doing: Option<String>,
    /// What it last said.
    pub said: Option<String>,
}

/// A subagent nothing has been heard from this long isn't running, whatever
/// its record says (its session was killed mid-turn).
const SUBAGENT_STALE: u64 = 30 * 60;
/// Finished subagents are listed this long after they last did something.
const SUBAGENT_KEEP: u64 = 30 * 60;

/// A Claude Code subagent from its `agent-<id>.meta.json` (`meta`) and the
/// tail of its transcript: running until the meta says `finished`.
pub fn parse_claude_subagent(
    id: &str,
    meta: &str,
    tail: &str,
    started: Option<u64>,
    mtime: u64,
    now: u64,
) -> Subagent {
    let m: Value = serde_json::from_str(meta).unwrap_or_default();
    let s = parse_tail_of(tail, true);
    let updated = s.updated.unwrap_or(mtime).max(mtime);
    let end = subagent_end(tail);
    // done: the meta says so (finished, or stopped with Esc), or its
    // transcript ends a turn — older Claude Codes never write `finished`
    let running = m["finished"].as_bool() != Some(true)
        && m["stoppedByUser"].as_bool() != Some(true)
        && !end.ended
        && now.saturating_sub(updated) < SUBAGENT_STALE;
    Subagent {
        id: id.to_string(),
        task: text_of(&m["description"])
            .or(s.last_prompt.clone())
            .unwrap_or_else(|| "subagent".into()),
        kind: text_of(&m["agentType"]),
        background: m["requestShape"] == "background",
        running,
        started,
        updated,
        doing: s.pending_tool.filter(|_| running),
        said: end.report.or(s.last_reply),
    }
}

/// How a subagent's transcript ends: whether its newest message closes a
/// turn, and the report it handed back, if it did that.
struct SubagentEnd {
    ended: bool,
    report: Option<String>,
}

/// The newest messages of a subagent's transcript: an assistant message
/// that ended its turn (`end_turn`), or the result of its handback tool
/// call (the report it delivered to its caller) — whose message is what it
/// said.
fn subagent_end(tail: &str) -> SubagentEnd {
    let mut messages = tail
        .lines()
        .rev()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| matches!(v["type"].as_str(), Some("user" | "assistant")));
    let newest = messages.next();
    let handback = |v: &Value| {
        v["message"]["content"].as_array().and_then(|b| {
            b.iter()
                .find(|b| b["type"] == "tool_use" && b["name"] == "SubagentHandback")
                .map(|b| text_of(&b["input"]["message"]))
        })
    };
    match newest {
        Some(v) if v["type"] == "assistant" => SubagentEnd {
            ended: v["message"]["stop_reason"] == "end_turn",
            report: handback(&v).flatten(),
        },
        Some(_) => {
            let report = messages.next().as_ref().and_then(handback);
            SubagentEnd {
                ended: report.is_some(),
                report: report.flatten(),
            }
        }
        None => SubagentEnd {
            ended: false,
            report: None,
        },
    }
}

/// The thread that spawned a Codex rollout, and the nickname and role it
/// gave it.
pub type Spawner = (String, Option<String>, Option<String>);

/// The parent thread of a Codex rollout, from its `session_meta`: set on
/// threads another thread spawned (`source.subagent.thread_spawn`).
pub fn codex_parent(head: &str) -> Option<Spawner> {
    let m: Value = serde_json::from_str(head.lines().next()?).ok()?;
    let spawn = &m["payload"]["source"]["subagent"]["thread_spawn"];
    let parent = spawn["parent_thread_id"].as_str()?.to_string();
    Some((
        parent,
        text_of(&spawn["agent_nickname"]),
        text_of(&spawn["agent_role"]),
    ))
}

/// opencode: `opencode session export <id>` JSON.
pub fn parse_opencode(json: &str) -> Option<Summary> {
    let v: Value = serde_json::from_str(json).ok()?;
    let info = &v["info"];
    let mut s = Summary {
        title: text_of(&info["title"]),
        updated: info["time"]["updated"].as_u64().map(|ms| ms / 1000),
        ..Default::default()
    };
    let t = &info["tokens"];
    let n = ["input"].iter().filter_map(|k| t[*k].as_u64()).sum::<u64>()
        + t["cache"]["read"].as_u64().unwrap_or(0)
        + t["cache"]["write"].as_u64().unwrap_or(0);
    s.context_tokens = (n > 0).then_some(n);
    let mut turn_known = false;
    for m in v["messages"].as_array()?.iter().rev() {
        match m["type"].as_str() {
            Some("idle") if !turn_known => {
                turn_known = true;
                s.in_turn = false;
            }
            Some("user") => {
                if !turn_known {
                    turn_known = true;
                    s.in_turn = true;
                }
                if s.last_prompt.is_none() {
                    s.last_prompt = text_of(&m["text"]);
                }
            }
            Some("assistant") if s.last_reply.is_none() => {
                let text: Vec<&str> = m["content"]
                    .as_array()
                    .map(Vec::as_slice)
                    .unwrap_or_default()
                    .iter()
                    .filter(|c| c["type"] == "text")
                    .filter_map(|c| c["text"].as_str())
                    .collect();
                if !text.is_empty() {
                    s.last_reply = Some(clip(text.join("\n\n").trim()));
                }
            }
            _ => {}
        }
        if turn_known && s.last_reply.is_some() && s.last_prompt.is_some() {
            break;
        }
    }
    Some(s)
}

fn read_head(path: &Path, bytes: u64) -> Option<String> {
    let f = std::fs::File::open(path).ok()?;
    let mut buf = Vec::new();
    f.take(bytes).read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// `$CODEX_HOME/sessions` (default `~/.codex/sessions`).
fn codex_sessions() -> Option<PathBuf> {
    Some(
        match std::env::var_os("CODEX_HOME") {
            Some(h) => PathBuf::from(h),
            None => PathBuf::from(std::env::var_os("HOME")?).join(".codex"),
        }
        .join("sessions"),
    )
}

/// Rollouts under Codex's sessions dir in the newest `days` day
/// directories (`sessions/YYYY/MM/DD`).
fn codex_recent_rollouts(days: usize) -> Vec<PathBuf> {
    let Some(root) = codex_sessions() else {
        return Vec::new();
    };
    let subdirs = |d: &Path| -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = std::fs::read_dir(d)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        v.sort();
        v
    };
    let mut dirs: Vec<PathBuf> = subdirs(&root)
        .into_iter()
        .rev()
        .take(2)
        .flat_map(|y| subdirs(&y).into_iter().rev().take(2))
        .flat_map(|m| subdirs(&m).into_iter().rev().take(days))
        .collect();
    dirs.sort();
    dirs.reverse();
    dirs.truncate(days);
    dirs.iter()
        .flat_map(|d| std::fs::read_dir(d).into_iter().flatten().flatten())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .collect()
}

/// opencode's session database (`$XDG_DATA_HOME/opencode/opencode.db`).
fn opencode_db() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))?;
    Some(base.join("opencode/opencode.db")).filter(|p| p.is_file())
}

/// Top-level opencode sessions updated between a minute and two days ago,
/// read from its database with the `sqlite3` CLI, read-only — opencode's
/// own CLI lists one project at a time and may start its service. Without
/// `sqlite3`, none.
fn opencode_ended(live: &[&str], busy_dirs: &[PathBuf]) -> Vec<Ended> {
    let Some(db) = opencode_db() else {
        return Vec::new();
    };
    let now = mtime_secs(SystemTime::now());
    let (from, to) = (
        now.saturating_sub(ENDED_WITHIN) * 1000,
        now.saturating_sub(60) * 1000,
    );
    // integers only in the query: nothing from outside is spliced in
    let query = format!(
        "select id, directory, title, time_updated from session \
         where parent_id is null and time_archived is null \
         and time_updated between {from} and {to} \
         and exists (select 1 from message where message.session_id = session.id) \
         order by time_updated desc limit {ENDED_MAX}"
    );
    let mut cmd = std::process::Command::new("sqlite3");
    cmd.arg("-readonly").arg("-json").arg(&db).arg(&query);
    let Some(json) = crate::exec::output(cmd, Duration::from_secs(2)) else {
        return Vec::new();
    };
    parse_opencode_sessions(&json)
        .into_iter()
        .filter(|e| !live.contains(&e.id.as_str()))
        .filter(|e| e.cwd.is_dir() && !busy_dirs.contains(&e.cwd))
        .collect()
}

/// `sqlite3 -json` rows of opencode sessions. A session opencode never
/// titled keeps "New session - <date>", which says nothing: no title then.
pub fn parse_opencode_sessions(json: &str) -> Vec<Ended> {
    let rows: Vec<Value> = serde_json::from_str(json).unwrap_or_default();
    rows.iter()
        .filter_map(|r| {
            let id = r["id"]
                .as_str()
                .filter(|id| crate::launcher::is_session_id(id))?;
            let title = text_of(&r["title"]).filter(|t| !t.starts_with("New session - "));
            let at = r["time_updated"].as_u64()? / 1000;
            let cwd = PathBuf::from(r["directory"].as_str()?);
            cwd.is_absolute().then(|| Ended {
                agent: "opencode".into(),
                id: id.to_string(),
                at,
                cwd: cwd.clone(),
                summary: Summary {
                    title,
                    updated: Some(at),
                    cwd: Some(cwd),
                    ..Default::default()
                },
            })
        })
        .collect()
}

/// Running first, then the most recently active.
fn sort_subagents(v: &mut [Subagent]) {
    v.sort_by(|a, b| b.running.cmp(&a.running).then(b.updated.cmp(&a.updated)));
}

/// The rollout under Codex's sessions dir whose name ends in `-<id>.jsonl`.
fn find_codex(id: &str) -> Option<PathBuf> {
    let root = codex_sessions()?;
    let suffix = format!("-{id}.jsonl");
    let mut stack = vec![root];
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.to_string_lossy().ends_with(&suffix) {
                return Some(p);
            }
        }
    }
    None
}

fn claude_dir() -> Option<PathBuf> {
    match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(d) => Some(PathBuf::from(d)),
        None => Some(PathBuf::from(std::env::var_os("HOME")?).join(".claude")),
    }
}

struct Entry {
    path: PathBuf,
    stamp: (SystemTime, u64),
    read_at: Instant,
    summary: Summary,
}

/// A subagent as last read, and the file stamps it was read at.
struct SubEntry {
    stamp: (SystemTime, u64, SystemTime),
    read_at: Instant,
    sub: Subagent,
}

/// Rollouts lately written under Codex's sessions dir, looked for at most
/// every `CODEX_SCAN_EVERY`: path -> the thread that spawned it, if any.
#[derive(Default)]
struct CodexScan {
    at: Option<Instant>,
    parents: HashMap<PathBuf, Option<Spawner>>,
}

const CODEX_SCAN_EVERY: Duration = Duration::from_secs(5);

/// The first record's timestamp.
fn first_ts(path: &Path) -> Option<u64> {
    let head = read_head(path, 16 * 1024)?;
    let v: Value = serde_json::from_str(head.lines().next()?).ok()?;
    v["timestamp"].as_str().and_then(parse_ts)
}

fn mtime_secs(t: SystemTime) -> u64 {
    t.duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Summaries per session id, re-read only when the transcript changes.
#[derive(Default)]
pub struct Cache {
    entries: HashMap<String, Entry>,
    subs: HashMap<PathBuf, SubEntry>,
    codex_scan: CodexScan,
    /// The last look for ended sessions, and what it found.
    ended: Option<(Instant, Vec<Ended>)>,
}

impl Cache {
    /// The summary of Claude Code session `id`, if its transcript exists.
    pub fn get(&mut self, id: &str) -> Option<Summary> {
        if !crate::launcher::is_session_id(id) {
            return None;
        }
        let path = match self.entries.get(id) {
            Some(e) => e.path.clone(),
            None => find(id)?,
        };
        let meta = std::fs::metadata(&path).ok()?;
        let stamp = (meta.modified().ok()?, meta.len());
        if let Some(e) = self.entries.get(id) {
            if e.stamp == stamp || e.read_at.elapsed() < MIN_REREAD {
                return Some(e.summary.clone());
            }
        }
        let prev = self.entries.get(id).map(|e| &e.summary);
        let summary = summarize(&path, prev)?;
        self.entries.insert(
            id.to_string(),
            Entry {
                path,
                stamp,
                read_at: Instant::now(),
                summary: summary.clone(),
            },
        );
        Some(summary)
    }

    /// The summary of Codex thread `id`, from its rollout file.
    pub fn get_codex(&mut self, id: &str) -> Option<Summary> {
        if !crate::launcher::is_session_id(id) {
            return None;
        }
        let path = match self.entries.get(id) {
            Some(e) => e.path.clone(),
            None => find_codex(id)?,
        };
        let meta = std::fs::metadata(&path).ok()?;
        let stamp = (meta.modified().ok()?, meta.len());
        if let Some(e) = self.entries.get(id) {
            if e.stamp == stamp || e.read_at.elapsed() < MIN_REREAD {
                return Some(e.summary.clone());
            }
        }
        let summary = parse_codex(&read_head(&path, 256 * 1024)?, &read_tail(&path, TAIL)?);
        self.entries.insert(
            id.to_string(),
            Entry {
                path,
                stamp,
                read_at: Instant::now(),
                summary: summary.clone(),
            },
        );
        Some(summary)
    }

    /// The summary of opencode session `id`, from `opencode session export`
    /// (only while its background service runs — it would start one
    /// otherwise). At most every few seconds per session.
    pub fn get_opencode(&mut self, id: &str, service_running: bool) -> Option<Summary> {
        if !crate::launcher::is_session_id(id) {
            return None;
        }
        if let Some(e) = self.entries.get(id) {
            if !service_running || e.read_at.elapsed() < OPENCODE_EVERY {
                return Some(e.summary.clone());
            }
        }
        if !service_running {
            return None;
        }
        let mut cmd = std::process::Command::new("opencode");
        cmd.args(["session", "export", id]);
        let json = crate::exec::output(cmd, Duration::from_secs(3))?;
        let summary = parse_opencode(&json)?;
        self.entries.insert(
            id.to_string(),
            Entry {
                path: PathBuf::new(),
                stamp: (SystemTime::UNIX_EPOCH, 0),
                read_at: Instant::now(),
                summary: summary.clone(),
            },
        );
        Some(summary)
    }

    /// The subagents of Claude Code session `id` (whose summary `get` has
    /// read): running ones and those finished in the last half hour,
    /// newest first. Each is re-read only when its files change.
    pub fn claude_subagents(&mut self, id: &str, now: u64) -> Vec<Subagent> {
        let Some(dir) = self
            .entries
            .get(id)
            .map(|e| e.path.with_extension("").join("subagents"))
        else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let meta_path = e.path();
            let Some(sub_id) = meta_path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_suffix(".meta.json"))
                .and_then(|n| n.strip_prefix("agent-"))
                .map(str::to_string)
            else {
                continue;
            };
            let log = dir.join(format!("agent-{sub_id}.jsonl"));
            let (Ok(lm), Ok(mm)) = (std::fs::metadata(&log), e.metadata()) else {
                continue;
            };
            let (Ok(lt), Ok(mt)) = (lm.modified(), mm.modified()) else {
                continue;
            };
            let latest = mtime_secs(lt).max(mtime_secs(mt));
            let stamp = (lt, lm.len(), mt);
            let known = self.subs.get(&log);
            // unchanged, or changed again within the re-read throttle
            let fresh = known.is_some_and(|s| s.stamp == stamp || s.read_at.elapsed() < MIN_REREAD);
            let started = known.and_then(|s| s.sub.started);
            if !fresh {
                // long finished and never read: not worth opening
                if now.saturating_sub(latest) > SUBAGENT_KEEP {
                    continue;
                }
                let (Some(meta), Some(tail)) = (
                    std::fs::read_to_string(&meta_path).ok(),
                    read_tail(&log, 64 * 1024),
                ) else {
                    continue;
                };
                let started = started.or_else(|| first_ts(&log));
                let sub = parse_claude_subagent(&sub_id, &meta, &tail, started, latest, now);
                self.subs.insert(
                    log.clone(),
                    SubEntry {
                        stamp,
                        read_at: Instant::now(),
                        sub,
                    },
                );
            }
            let Some(s) = self.subs.get_mut(&log) else {
                continue;
            };
            // time passes without the files changing: go stale
            s.sub.running &= now.saturating_sub(s.sub.updated) < SUBAGENT_STALE;
            if s.sub.running || now.saturating_sub(s.sub.updated) <= SUBAGENT_KEEP {
                out.push(s.sub.clone());
            }
        }
        sort_subagents(&mut out);
        out
    }

    /// The subagents of Codex thread `id`: rollouts written lately whose
    /// `session_meta` names it as the parent. Running while a turn is open.
    pub fn codex_subagents(&mut self, id: &str, now: u64) -> Vec<Subagent> {
        self.scan_codex(now);
        let kids: Vec<(PathBuf, Option<String>, Option<String>)> = self
            .codex_scan
            .parents
            .iter()
            .filter_map(|(p, parent)| match parent {
                Some((pid, nick, role)) if pid == id => {
                    Some((p.clone(), nick.clone(), role.clone()))
                }
                _ => None,
            })
            .collect();
        let mut out = Vec::new();
        for (path, nick, role) in kids {
            let Ok(m) = std::fs::metadata(&path) else {
                continue;
            };
            let Ok(mt) = m.modified() else { continue };
            let stamp = (mt, m.len(), mt);
            let known = self.subs.get(&path);
            let fresh = known.is_some_and(|s| s.stamp == stamp || s.read_at.elapsed() < MIN_REREAD);
            if !fresh {
                // the head (its session_meta) never changes: read it once
                let started = known.and_then(|s| s.sub.started);
                let head = match started {
                    Some(_) => String::new(),
                    None => read_head(&path, 256 * 1024).unwrap_or_default(),
                };
                let started = started.or_else(|| {
                    let m: Value = serde_json::from_str(head.lines().next()?).ok()?;
                    m["payload"]["timestamp"]
                        .as_str()
                        .or(m["timestamp"].as_str())
                        .and_then(parse_ts)
                });
                let Some(tail) = read_tail(&path, TAIL) else {
                    continue;
                };
                let s = parse_codex(&head, &tail);
                let updated = s.updated.unwrap_or(mtime_secs(mt));
                let sub = Subagent {
                    id: path
                        .file_stem()
                        .and_then(|n| n.to_str())
                        .and_then(|n| n.get(n.len().saturating_sub(36)..))
                        .unwrap_or_default()
                        .to_string(),
                    task: nick
                        .or(s.last_prompt.clone())
                        .unwrap_or_else(|| "subagent".into()),
                    kind: role,
                    background: true,
                    running: s.in_turn,
                    started,
                    updated,
                    doing: None,
                    said: s.last_reply,
                };
                self.subs.insert(
                    path.clone(),
                    SubEntry {
                        stamp,
                        read_at: Instant::now(),
                        sub,
                    },
                );
            }
            let Some(s) = self.subs.get_mut(&path) else {
                continue;
            };
            s.sub.running &= now.saturating_sub(s.sub.updated) < SUBAGENT_STALE;
            if s.sub.running || now.saturating_sub(s.sub.updated) <= SUBAGENT_KEEP {
                out.push(s.sub.clone());
            }
        }
        sort_subagents(&mut out);
        out
    }

    /// Rollouts in the two newest day directories, written in the last
    /// half hour: which thread spawned each (read once per file).
    fn scan_codex(&mut self, now: u64) {
        if self
            .codex_scan
            .at
            .is_some_and(|at| at.elapsed() < CODEX_SCAN_EVERY)
        {
            return;
        }
        self.codex_scan.at = Some(Instant::now());
        let Some(root) = codex_sessions() else { return };
        // sessions/YYYY/MM/DD: the newest two days
        let newest = |d: &Path| -> Vec<PathBuf> {
            let mut v: Vec<PathBuf> = std::fs::read_dir(d)
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect();
            v.sort();
            v
        };
        let mut days: Vec<PathBuf> = newest(&root)
            .into_iter()
            .rev()
            .take(2)
            .flat_map(|y| newest(&y).into_iter().rev().take(2))
            .flat_map(|m| newest(&m).into_iter().rev().take(2))
            .collect();
        days.sort();
        days.reverse();
        days.truncate(2);
        let mut seen = Vec::new();
        for day in days {
            for e in std::fs::read_dir(&day).into_iter().flatten().flatten() {
                let p = e.path();
                let recent = e
                    .metadata()
                    .and_then(|m| m.modified())
                    .is_ok_and(|t| now.saturating_sub(mtime_secs(t)) <= SUBAGENT_KEEP);
                if !recent || p.extension().is_none_or(|x| x != "jsonl") {
                    continue;
                }
                if !self.codex_scan.parents.contains_key(&p) {
                    let parent = read_head(&p, 256 * 1024).and_then(|h| codex_parent(&h));
                    self.codex_scan.parents.insert(p.clone(), parent);
                }
                seen.push(p);
            }
        }
        self.codex_scan.parents.retain(|p, _| seen.contains(p));
        self.subs
            .retain(|p, _| seen.contains(p) || !p.starts_with(&root));
    }

    /// Forget sessions not in `live` (nor listed as ended).
    pub fn retain(&mut self, live: &[&str]) {
        let ended: Vec<String> = self
            .ended
            .iter()
            .flat_map(|(_, e)| e.iter().map(|e| e.id.clone()))
            .collect();
        self.entries
            .retain(|id, _| live.contains(&id.as_str()) || ended.contains(id));
        // subagents of sessions still known (Codex's are pruned by the scan)
        let dirs: Vec<PathBuf> = self
            .entries
            .values()
            .map(|e| e.path.with_extension(""))
            .collect();
        let codex = codex_sessions();
        self.subs.retain(|p, _| {
            dirs.iter().any(|d| p.starts_with(d))
                || codex.as_ref().is_some_and(|c| p.starts_with(c))
        });
    }

    /// Recently exited Claude Code sessions, newest first: transcripts
    /// written in the last two days whose session isn't running. `busy_dirs`
    /// are directories with a running session marina couldn't identify —
    /// a transcript there may be its, so it isn't offered for resuming.
    pub fn ended(&mut self, live: &[&str], busy_dirs: &[PathBuf]) -> Vec<Ended> {
        if let Some((at, found)) = &self.ended {
            if at.elapsed() < ENDED_EVERY {
                return found
                    .iter()
                    .filter(|e| !live.contains(&e.id.as_str()))
                    .cloned()
                    .collect();
            }
        }
        let now = SystemTime::now();
        let mut recent: Vec<(String, PathBuf, u64)> = Vec::new();
        if let Some(projects) = claude_dir().map(|d| d.join("projects")) {
            for dir in std::fs::read_dir(projects).into_iter().flatten().flatten() {
                for f in std::fs::read_dir(dir.path())
                    .into_iter()
                    .flatten()
                    .flatten()
                {
                    let p = f.path();
                    let Some(id) = p
                        .file_stem()
                        .filter(|_| p.extension().is_some_and(|x| x == "jsonl"))
                        .and_then(|s| s.to_str())
                        .filter(|id| crate::launcher::is_session_id(id) && !live.contains(id))
                    else {
                        continue;
                    };
                    let Some(mtime) = f.metadata().ok().and_then(|m| m.modified().ok()) else {
                        continue;
                    };
                    let age = now.duration_since(mtime).map(|d| d.as_secs()).unwrap_or(0);
                    // written in the last minute: likely still running
                    if (60..=ENDED_WITHIN).contains(&age) {
                        let at = mtime
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs())
                            .unwrap_or(0);
                        recent.push((id.to_string(), p, at));
                    }
                }
            }
        }
        recent.sort_by_key(|r| std::cmp::Reverse(r.2));
        let mut found = Vec::new();
        // bounded work per scan, and unchanged files aren't re-read
        for (id, path, at) in recent.into_iter().take(ENDED_MAX * 2) {
            if found.len() == ENDED_MAX {
                break;
            }
            let stamp = std::fs::metadata(&path)
                .ok()
                .and_then(|m| Some((m.modified().ok()?, m.len())));
            let cached = self
                .entries
                .get(&id)
                .filter(|e| Some(e.stamp) == stamp)
                .map(|e| e.summary.clone());
            let summary = match cached {
                Some(s) => s,
                None => {
                    let Some(mut s) = summarize(&path, None) else {
                        continue;
                    };
                    // resume looks the session up from the directory it
                    // started in: the transcript's first recorded cwd
                    s.cwd = first_cwd(&path).or(s.cwd);
                    if let Some(stamp) = stamp {
                        self.entries.insert(
                            id.clone(),
                            Entry {
                                path: path.clone(),
                                stamp,
                                read_at: Instant::now(),
                                summary: s.clone(),
                            },
                        );
                    }
                    s
                }
            };
            // only sessions that said something, in a directory that's still
            // there and isn't busy with an unidentified session
            let Some(cwd) = summary
                .cwd
                .clone()
                .filter(|c| c.is_absolute() && c.is_dir() && !busy_dirs.contains(c))
                .filter(|_| summary.last_reply.is_some())
            else {
                continue;
            };
            found.push(Ended {
                agent: "claude".into(),
                id,
                at,
                cwd,
                summary,
            });
        }
        found.extend(self.codex_ended(live, busy_dirs));
        found.extend(opencode_ended(live, busy_dirs));
        found.sort_by_key(|e| std::cmp::Reverse(e.at));
        found.truncate(ENDED_MAX);
        self.ended = Some((Instant::now(), found.clone()));
        found
    }

    /// Codex CLI sessions (not desktop-app threads, `exec` runs or
    /// subagents) whose rollout was last written between a minute and two
    /// days ago, and that said something.
    fn codex_ended(&mut self, live: &[&str], busy_dirs: &[PathBuf]) -> Vec<Ended> {
        let now = SystemTime::now();
        let mut out = Vec::new();
        for path in codex_recent_rollouts(3) {
            let Ok(meta) = std::fs::metadata(&path) else {
                continue;
            };
            let Ok(mtime) = meta.modified() else { continue };
            let age = now.duration_since(mtime).map(|d| d.as_secs()).unwrap_or(0);
            if !(60..=ENDED_WITHIN).contains(&age) {
                continue;
            }
            let Some(id) = path
                .file_stem()
                .and_then(|n| n.to_str())
                .and_then(|n| n.get(n.len().saturating_sub(36)..))
                .filter(|id| crate::launcher::is_session_id(id) && !live.contains(id))
                .map(str::to_string)
            else {
                continue;
            };
            let stamp = (mtime, meta.len());
            let summary = match self.entries.get(&id).filter(|e| e.stamp == stamp) {
                Some(e) => e.summary.clone(),
                None => {
                    let Some(head) = read_head(&path, 256 * 1024) else {
                        continue;
                    };
                    let Some(m) = head
                        .lines()
                        .next()
                        .and_then(|l| serde_json::from_str::<Value>(l).ok())
                    else {
                        continue;
                    };
                    // what `codex resume` brings back: the CLI's own sessions
                    if m["payload"]["source"] != "cli" {
                        continue;
                    }
                    let Some(tail) = read_tail(&path, TAIL) else {
                        continue;
                    };
                    let s = parse_codex(&head, &tail);
                    self.entries.insert(
                        id.clone(),
                        Entry {
                            path: path.clone(),
                            stamp,
                            read_at: Instant::now(),
                            summary: s.clone(),
                        },
                    );
                    s
                }
            };
            let Some(cwd) = summary
                .cwd
                .clone()
                .filter(|c| c.is_absolute() && c.is_dir() && !busy_dirs.contains(c))
                .filter(|_| summary.last_reply.is_some())
            else {
                continue;
            };
            out.push(Ended {
                agent: "codex".into(),
                id,
                at: mtime_secs(mtime),
                cwd,
                summary,
            });
        }
        out
    }
}

/// `<claude dir>/projects/*/<id>.jsonl`.
fn find(id: &str) -> Option<PathBuf> {
    let projects = claude_dir()?.join("projects");
    std::fs::read_dir(projects)
        .ok()?
        .flatten()
        .map(|d| d.path().join(format!("{id}.jsonl")))
        .find(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_claude_subagent_runs_until_its_meta_says_finished() {
        let now = 1_800_000_000;
        let ts = "2027-01-15T08:00:00Z";
        let at = parse_ts(ts).unwrap();
        let tail = [
            line(
                serde_json::json!({"type":"user","isSidechain":true,"timestamp":ts,
                "message":{"role":"user","content":"Find the flaky test"}}),
            ),
            line(
                serde_json::json!({"type":"assistant","isSidechain":true,"timestamp":ts,
                "message":{"role":"assistant","content":[{"type":"tool_use","name":"Bash",
                "input":{"command":"cargo test"}}]}}),
            ),
        ]
        .join("\n");
        let meta = r#"{"agentType":"Explore","description":"Find the flaky test","requestShape":"background"}"#;
        let s = parse_claude_subagent("a1", meta, &tail, Some(at - 60), at, at + 5);
        assert!(s.running);
        assert_eq!(s.task, "Find the flaky test");
        assert_eq!(s.kind.as_deref(), Some("Explore"));
        assert!(s.background);
        assert_eq!(s.doing.as_deref(), Some("Bash: cargo test"));
        // finished
        let done = meta.replace('}', r#","finished":true}"#);
        let s = parse_claude_subagent("a1", &done, &tail, None, at, at + 5);
        assert!(!s.running && s.doing.is_none());
        // heard nothing from for over half an hour: not running
        let s = parse_claude_subagent("a1", meta, &tail, None, at, at + SUBAGENT_STALE + 1);
        assert!(!s.running);
        let _ = now;
    }

    #[test]
    fn a_subagent_is_done_when_its_transcript_ends_a_turn_or_it_was_stopped() {
        let ts = "2027-01-15T08:00:00Z";
        let at = parse_ts(ts).unwrap();
        let meta = r#"{"description":"Find the flaky test"}"#;
        // older Claude Codes never write `finished`: its last message ends the turn
        let ended = line(
            serde_json::json!({"type":"assistant","isSidechain":true,"timestamp":ts,
            "message":{"role":"assistant","stop_reason":"end_turn",
            "content":[{"type":"text","text":"Found it."}]}}),
        );
        let s = parse_claude_subagent("a", meta, &ended, None, at, at + 5);
        assert!(!s.running);
        assert_eq!(s.said.as_deref(), Some("Found it."));
        // a report handed back through a tool, and its result
        let handback = [
            line(serde_json::json!({"type":"assistant","isSidechain":true,"timestamp":ts,
                "message":{"role":"assistant","stop_reason":"tool_use","content":[{"type":"tool_use",
                "name":"SubagentHandback","input":{"message":"Three tests use local time."}}]}})),
            line(serde_json::json!({"type":"user","isSidechain":true,"timestamp":ts,
                "message":{"role":"user","content":[{"type":"tool_result","content":"delivered"}]}})),
        ]
        .join("\n");
        let s = parse_claude_subagent("a", meta, &handback, None, at, at + 5);
        assert!(!s.running);
        assert_eq!(s.said.as_deref(), Some("Three tests use local time."));
        // stopped with Esc: never finished, but not running
        let mid = line(
            serde_json::json!({"type":"assistant","isSidechain":true,"timestamp":ts,
            "message":{"role":"assistant","stop_reason":"tool_use","content":[{"type":"tool_use",
            "name":"Bash","input":{"command":"cargo test"}}]}}),
        );
        assert!(parse_claude_subagent("a", meta, &mid, None, at, at + 5).running);
        let stopped = r#"{"description":"x","stoppedByUser":true}"#;
        assert!(!parse_claude_subagent("a", stopped, &mid, None, at, at + 5).running);
    }

    #[test]
    fn opencode_sessions_from_its_database() {
        let rows = r#"[
          {"id":"ses_1cf4f4cdaffeOcfwrwgHVCC6Ia","directory":"/Users/me/dev/api","title":"Rate limits","time_updated":1781213946819},
          {"id":"ses_147675f2bffeVjtl8vMGbsPFci","directory":"/Users/me/dev/web","title":"New session - 2026-06-11T21:31:10.932Z","time_updated":1781213000000},
          {"id":"$(rm -rf ~)","directory":"/x","title":"t","time_updated":1},
          {"id":"ses_x","directory":"relative/dir","title":"t","time_updated":1}
        ]"#;
        let e = parse_opencode_sessions(rows);
        assert_eq!(e.len(), 2, "bad ids and relative dirs dropped");
        assert_eq!((e[0].agent.as_str(), e[0].at), ("opencode", 1_781_213_946));
        assert_eq!(e[0].summary.title.as_deref(), Some("Rate limits"));
        assert_eq!(
            e[1].summary.title, None,
            "opencode's placeholder title says nothing"
        );
    }

    #[test]
    fn a_codex_rollout_names_the_thread_that_spawned_it() {
        let head = r#"{"type":"session_meta","payload":{"id":"t2","source":{"subagent":{"thread_spawn":{"parent_thread_id":"t1","depth":1,"agent_nickname":"Ada","agent_role":"explorer"}}}}}"#;
        assert_eq!(
            codex_parent(head),
            Some(("t1".into(), Some("Ada".into()), Some("explorer".into())))
        );
        let own = r#"{"type":"session_meta","payload":{"id":"t1","source":"cli"}}"#;
        assert_eq!(codex_parent(own), None);
    }

    #[test]
    fn a_sessions_own_summary_skips_its_subagents_records() {
        let tail = [
            line(serde_json::json!({"type":"assistant","isSidechain":false,
                "message":{"role":"assistant","content":[{"type":"text","text":"Mine."}]}})),
            line(serde_json::json!({"type":"assistant","isSidechain":true,
                "message":{"role":"assistant","content":[{"type":"text","text":"The subagent's."}]}})),
        ]
        .join("\n");
        assert_eq!(parse_tail(&tail).last_reply.as_deref(), Some("Mine."));
        assert_eq!(
            parse_tail_of(&tail, true).last_reply.as_deref(),
            Some("The subagent's.")
        );
    }

    fn line(v: serde_json::Value) -> String {
        v.to_string()
    }

    #[test]
    fn summary_from_a_finished_turn() {
        let t = [
            "partial line from the middle of a record\"}".to_string(),
            line(serde_json::json!({"type":"user","isSidechain":false,
                "message":{"role":"user","content":"stop the mutation"},"timestamp":"2026-10-20T21:48:40.000Z"})),
            line(serde_json::json!({"parentUuid":"x","isSidechain":false,"type":"assistant",
                "message":{"role":"assistant","content":[{"type":"tool_use","name":"Bash","input":{"command":"pkill stryker"}}]}})),
            line(serde_json::json!({"type":"user","isSidechain":false,
                "message":{"role":"user","content":[{"type":"tool_result","content":"ok"}]}})),
            line(serde_json::json!({"type":"assistant","isSidechain":true,
                "message":{"content":[{"type":"text","text":"subagent chatter"}]}})),
            line(serde_json::json!({"parentUuid":"y","isSidechain":false,"type":"assistant",
                "message":{"role":"assistant","model":"claude-opus-5-5",
                    "usage":{"input_tokens":2,"cache_read_input_tokens":169000,"cache_creation_input_tokens":1300,"output_tokens":90},
                    "content":[{"type":"text","text":"I stopped the run.\n\nTell me which."}]},
                "timestamp":"2026-10-20T21:48:59.114Z"})),
            line(serde_json::json!({"type":"api-request-blob","message":{"role":"assistant","content":[{"type":"text","text":"not this"}]}})),
            line(serde_json::json!({"type":"system","subtype":"away_summary","isSidechain":false,
                "content":"Releasing 0.13.0; next: pick a fix.","timestamp":"2026-10-20T21:52:01.803Z"})),
            line(serde_json::json!({"type":"last-prompt","lastPrompt":"stop the mutation"})),
            line(serde_json::json!({"type":"ai-title","aiTitle":"Repo validation"})),
            line(serde_json::json!({"type":"pr-link","prUrl":"https://github.com/o/r/pull/33"})),
        ]
        .join("\n");
        let s = parse_tail(&t);
        assert_eq!(s.title.as_deref(), Some("Repo validation"));
        assert_eq!(s.last_prompt.as_deref(), Some("stop the mutation"));
        assert_eq!(
            s.last_reply.as_deref(),
            Some("I stopped the run.\n\nTell me which.")
        );
        assert_eq!(
            s.recap.as_deref(),
            Some("Releasing 0.13.0; next: pick a fix.")
        );
        assert_eq!(s.pending_tool, None, "the last message is text");
        assert!(!s.in_turn, "a recap after the reply: the turn is over");
        assert_eq!(s.pr_url.as_deref(), Some("https://github.com/o/r/pull/33"));
        assert_eq!(s.updated, Some(1792533121), "the newest timestamp");
        assert_eq!(s.context_tokens, Some(170_302));
        assert_eq!(s.model.as_deref(), Some("claude-opus-5-5"));
    }

    #[test]
    fn a_tool_call_without_a_result_is_pending_and_old_recaps_drop() {
        let t = [
            line(serde_json::json!({"type":"system","subtype":"away_summary","isSidechain":false,"content":"old recap"})),
            line(serde_json::json!({"type":"assistant","isSidechain":false,
                "message":{"content":[{"type":"text","text":"Publishing now."}]}})),
            line(serde_json::json!({"type":"assistant","isSidechain":false,
                "message":{"content":[{"type":"tool_use","name":"Bash",
                    "input":{"command":"cargo publish\n--allow-dirty"}}]}})),
        ]
        .join("\n");
        let s = parse_tail(&t);
        assert_eq!(s.pending_tool.as_deref(), Some("Bash: cargo publish"));
        assert!(s.in_turn);
        assert_eq!(s.last_reply.as_deref(), Some("Publishing now."));
        assert_eq!(s.recap, None, "written before the latest reply");

        let two = [
            line(serde_json::json!({"type":"assistant","isSidechain":false,
                "message":{"content":[{"type":"text","text":"Done."}]}})),
            line(serde_json::json!({"type":"system","subtype":"away_summary","isSidechain":false,"content":"older recap"})),
            line(serde_json::json!({"type":"system","subtype":"away_summary","isSidechain":false,"content":"newer recap"})),
        ]
        .join("\n");
        assert_eq!(parse_tail(&two).recap.as_deref(), Some("newer recap"));
        let asked = [
            line(serde_json::json!({"type":"system","subtype":"turn_duration","isSidechain":false})),
            line(serde_json::json!({"type":"user","isSidechain":false,"message":{"content":"create bye.txt"}})),
            line(serde_json::json!({"type":"api-request","x":1})),
        ]
        .join("\n");
        let s = parse_tail(&asked);
        assert!(
            s.in_turn && s.pending_tool.is_none(),
            "asked, nothing flushed since"
        );
    }

    #[test]
    fn codex_rollouts_and_opencode_exports_summarise_too() {
        let meta = r#"{"timestamp":"2026-10-18T10:53:10.697Z","type":"session_meta","payload":{"id":"t1","cwd":"/Users/me/dev/app","model_provider":"openai"}}"#;
        let tail = [
            r#"{"timestamp":"2026-10-18T11:00:00Z","type":"event_msg","payload":{"type":"user_message","message":"make it rain"}}"#,
            r#"{"timestamp":"2026-10-18T11:00:05Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Done — it rains."}]}}"#,
            r#"{"timestamp":"2026-10-18T11:00:06Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":130773}}}}"#,
            r#"{"timestamp":"2026-10-18T11:00:07Z","type":"event_msg","payload":{"type":"task_complete","last_agent_message":"Done — it rains."}}"#,
        ]
        .join("\n");
        let s = parse_codex(meta, &tail);
        assert_eq!(s.last_prompt.as_deref(), Some("make it rain"));
        assert_eq!(s.last_reply.as_deref(), Some("Done — it rains."));
        assert_eq!(s.context_tokens, Some(130_773));
        assert_eq!(s.cwd.as_deref(), Some(Path::new("/Users/me/dev/app")));
        assert!(!s.in_turn, "task complete");

        let oc = r#"{"info":{"title":"Test run","time":{"updated":1792747187517},
            "tokens":{"input":9374,"cache":{"read":896,"write":0}}},
          "messages":[
            {"type":"user","text":"test"},
            {"type":"assistant","content":[{"type":"reasoning","text":"hmm"},{"type":"text","text":"Hi! What should I test?"}]},
            {"type":"idle","outcome":"succeeded"}]}"#;
        let s = parse_opencode(oc).unwrap();
        assert_eq!(s.title.as_deref(), Some("Test run"));
        assert_eq!(s.last_reply.as_deref(), Some("Hi! What should I test?"));
        assert_eq!(s.last_prompt.as_deref(), Some("test"));
        assert_eq!(s.context_tokens, Some(10_270));
        assert!(!s.in_turn);
        assert!(parse_opencode("nope").is_none());
    }

    #[test]
    fn transcripts_belong_to_their_encoded_directory() {
        let base = std::env::temp_dir().join(format!("marina-belongs-{}", std::process::id()));
        let proj = base.join("projects/-Users-me-dev-danfry-dev");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(proj.join("abc123.jsonl"), "{}").unwrap();
        // SAFETY: tests touching CLAUDE_CONFIG_DIR run in this one test
        unsafe { std::env::set_var("CLAUDE_CONFIG_DIR", &base) };
        assert_eq!(
            belongs_to("abc123", Path::new("/Users/me/dev/danfry.dev")),
            Some(true)
        );
        assert_eq!(
            belongs_to("abc123", Path::new("/Users/me/dev/other")),
            Some(false)
        );
        assert_eq!(
            belongs_to("$(x)", Path::new("/Users/me/dev/danfry.dev")),
            Some(false)
        );
        unsafe { std::env::remove_var("CLAUDE_CONFIG_DIR") };
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn the_ask_and_what_it_did_come_from_recaps_and_replies() {
        let recap = |r: &str| Summary {
            recap: Some(r.into()),
            ..Default::default()
        };
        let reply = |r: &str| Summary {
            last_reply: Some(r.into()),
            ..Default::default()
        };
        let s = recap("Releasing astrotime 0.13.0; it's unpublished. Next: say whether I should retry now, or raise the limit.");
        assert_eq!(
            s.ask().as_deref(),
            Some("Say whether I should retry now, or raise the limit.")
        );
        assert_eq!(
            s.did().as_deref(),
            Some("Releasing astrotime 0.13.0; it's unpublished.")
        );
        let s =
            recap("The polish is in PR #26. Next is your call on merging #26 or trying it first.");
        assert_eq!(
            s.ask().as_deref(),
            Some("Your call on merging #26 or trying it first.")
        );
        let s = recap("Four tricks are live. Next step: tell me whether to draft the post.");
        assert_eq!(
            s.ask().as_deref(),
            Some("Tell me whether to draft the post.")
        );
        let s = recap("No task yet. Next, send the task you want. (disable recaps in /config)");
        assert_eq!(s.ask().as_deref(), Some("Send the task you want."));
        assert_eq!(
            recap("All merged. Nothing is pending, so the next session can start fresh.").ask(),
            None
        );

        let s =
            reply("Build 7 is live.\n\nNothing is committed yet. Want me to commit it all now?");
        assert_eq!(
            s.ask().as_deref(),
            Some("Want me to commit it all now? — Nothing is committed yet.")
        );
        let s = reply("Plan below.\n\nI'd start with a **design doc**. Shall I begin?");
        assert_eq!(
            s.ask().as_deref(),
            Some("Shall I begin? — I'd start with a design doc.")
        );
        let s = reply(
            "The grid is done. Say \"push\" when you want them shipped, or tell me what to change.",
        );
        assert!(s.ask().unwrap().starts_with("Say \"push\""));
        assert_eq!(
            reply("I stopped the dev server and deleted the branch.").ask(),
            None,
            "just reporting"
        );
        assert_eq!(
            reply("I stopped the dev server.\n\nMore below.")
                .did()
                .as_deref(),
            Some("I stopped the dev server.")
        );
    }

    #[test]
    fn parses_timestamps_and_line_types() {
        assert_eq!(parse_ts("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_ts("2000-03-01T12:30:15.5Z"), Some(951_913_815));
        assert_eq!(parse_ts("nope"), None);
        assert_eq!(
            leading_type(r#"{"type":"ai-title","x":1}"#),
            Some("ai-title")
        );
        assert_eq!(
            leading_type(r#"{"parentUuid":"a","type":"assistant"}"#),
            None
        );
    }

    #[test]
    fn reads_only_the_tail_of_a_big_file() {
        let dir = std::env::temp_dir().join(format!("marina-transcript-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("s.jsonl");
        let filler = line(serde_json::json!({"type":"api-request","pad":"x".repeat(1000)}));
        let mut body = String::new();
        body.push_str(&line(
            serde_json::json!({"type":"assistant","isSidechain":false,
            "message":{"content":[{"type":"text","text":"early reply"}]}}),
        ));
        body.push('\n');
        for _ in 0..400 {
            body.push_str(&filler); // ~400KB of request blobs since the reply
            body.push('\n');
        }
        std::fs::write(&p, &body).unwrap();
        let s = summarize(&p, None).unwrap();
        assert_eq!(
            s.last_reply.as_deref(),
            Some("early reply"),
            "found by widening"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod live {
    /// `MARINA_TRANSCRIPT=<session id> cargo test -- --ignored live` prints
    /// the summary of a real session.
    #[test]
    #[ignore]
    fn print_a_real_summary() {
        let id = std::env::var("MARINA_TRANSCRIPT").unwrap();
        let t = std::time::Instant::now();
        let s = super::Cache::default().get(&id);
        eprintln!("{:#?}\nin {:?}", s, t.elapsed());
    }
}
