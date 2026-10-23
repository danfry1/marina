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
}

/// A Claude Code session that has exited but could be resumed.
#[derive(Clone, Debug, PartialEq)]
pub struct Ended {
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
        if v["isSidechain"].as_bool() == Some(true) {
            continue; // a subagent's turn
        }
        if s.updated.is_none() {
            s.updated = v["timestamp"].as_str().and_then(parse_ts);
        }
        if s.cwd.is_none() {
            s.cwd = v["cwd"].as_str().map(PathBuf::from);
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
    })
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

/// `$CODEX_HOME/sessions` (default `~/.codex/sessions`): the rollout whose
/// name ends in `-<id>.jsonl`.
fn find_codex(id: &str) -> Option<PathBuf> {
    let root = match std::env::var_os("CODEX_HOME") {
        Some(h) => PathBuf::from(h),
        None => PathBuf::from(std::env::var_os("HOME")?).join(".codex"),
    }
    .join("sessions");
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

/// Summaries per session id, re-read only when the transcript changes.
#[derive(Default)]
pub struct Cache {
    entries: HashMap<String, Entry>,
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

    /// Forget sessions not in `live` (nor listed as ended).
    pub fn retain(&mut self, live: &[&str]) {
        let ended: Vec<String> = self
            .ended
            .iter()
            .flat_map(|(_, e)| e.iter().map(|e| e.id.clone()))
            .collect();
        self.entries
            .retain(|id, _| live.contains(&id.as_str()) || ended.contains(id));
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
                id,
                at,
                cwd,
                summary,
            });
        }
        self.ended = Some((Instant::now(), found.clone()));
        found
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
