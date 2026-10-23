//! `marina digest`: what your agents did over a stretch of time — per
//! project, the sessions active in it, how long they spent working, the
//! tokens they used, commits made in the repo, PRs opened, and each
//! session's latest recap. A standup in one command.
//!
//! Read from Claude Code transcripts (turn durations, token usage, PR links,
//! titles and recaps; their tails only) and `git log`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::Value;

/// How far back into a transcript the window is looked for.
const DIGEST_TAIL: u64 = 8 * 1024 * 1024;

/// One session's part of the window.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct SessionDay {
    pub id: String,
    pub title: Option<String>,
    pub cwd: Option<PathBuf>,
    /// Time spent in turns that ended in the window.
    pub working_secs: u64,
    pub turns: u32,
    /// Tokens sent to the model (input, cache reads and writes) and received.
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub prs: Vec<String>,
    /// Latest recap, else the first sentence of the last reply.
    pub said: Option<String>,
    pub running: bool,
}

/// What a transcript tail says about the window `[start, ∞)`.
pub fn tally(id: &str, tail: &str, start: u64) -> SessionDay {
    let mut d = SessionDay {
        id: id.to_string(),
        ..Default::default()
    };
    for line in tail.lines() {
        // cheap filter: only the records that count
        let wanted = line.contains("\"turn_duration\"")
            || line.contains("\"usage\"")
            || line.contains("\"pr-link\"")
            || line.contains("\"cwd\"");
        if !wanted {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if d.cwd.is_none() {
            d.cwd = v["cwd"].as_str().map(PathBuf::from);
        }
        let at = v["timestamp"]
            .as_str()
            .and_then(crate::transcript::parse_ts)
            .unwrap_or(0);
        if at < start {
            continue;
        }
        if v["isSidechain"].as_bool() == Some(true) {
            continue;
        }
        if v["type"] == "system" && v["subtype"] == "turn_duration" {
            d.working_secs += v["durationMs"].as_u64().unwrap_or(0) / 1000;
            d.turns += 1;
        } else if v["type"] == "assistant" {
            let u = &v["message"]["usage"];
            d.tokens_in += [
                "input_tokens",
                "cache_read_input_tokens",
                "cache_creation_input_tokens",
            ]
            .iter()
            .filter_map(|k| u[*k].as_u64())
            .sum::<u64>();
            d.tokens_out += u["output_tokens"].as_u64().unwrap_or(0);
        } else if v["type"] == "pr-link" {
            if let Some(url) = v["prUrl"].as_str() {
                if !d.prs.iter().any(|p| p == url) {
                    d.prs.push(url.to_string());
                }
            }
        }
    }
    let s = crate::transcript::parse_tail(tail);
    d.title = s.title;
    d.said = s
        .recap
        .or(s.last_reply)
        .map(|t| crate::ui::first_sentence(&t));
    d
}

/// `24h`, `3d`, `today` (since local midnight) → the window's start.
pub fn parse_since(spec: &str, now: u64) -> Option<u64> {
    if spec == "today" {
        return Some(local_midnight(now));
    }
    let (num, unit) = spec.split_at(spec.len().checked_sub(1)?);
    let n: u64 = num.parse().ok()?;
    let secs = match unit {
        "h" => n * 3600,
        "d" => n * 86_400,
        "m" => n * 60,
        _ => return None,
    };
    Some(now.saturating_sub(secs))
}

fn local_midnight(now: u64) -> u64 {
    // SAFETY: localtime_r writes into the struct we pass.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let t = now as libc::time_t;
    unsafe { libc::localtime_r(&t, &mut tm) };
    now - (tm.tm_hour as u64 * 3600 + tm.tm_min as u64 * 60 + tm.tm_sec as u64)
}

/// Commits in the repo at `dir` since `start`: (count, newest subjects).
fn commits(dir: &Path, start: u64) -> (usize, Vec<String>) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["log", "--no-merges", "--format=%s"])
        .arg(format!("--since=@{start}"))
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output();
    let Some(o) = out.ok().filter(|o| o.status.success()) else {
        return (0, Vec::new());
    };
    let subjects: Vec<String> = String::from_utf8_lossy(&o.stdout)
        .lines()
        .map(String::from)
        .collect();
    let n = subjects.len();
    (n, subjects.into_iter().take(3).collect())
}

fn fmt_dur(secs: u64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
    }
}

fn fmt_tokens(n: u64) -> String {
    match n {
        n if n >= 1_000_000 => format!("{:.1}M", n as f64 / 1e6),
        n if n >= 1000 => format!("{}k", n / 1000),
        n => n.to_string(),
    }
}

#[derive(Serialize)]
struct ProjectDay {
    project: String,
    dir: PathBuf,
    sessions: Vec<SessionDay>,
    commits: usize,
    recent_commits: Vec<String>,
}

/// `marina digest [24h|3d|today] [--json]`.
pub fn run(since: Option<&str>, json: bool) -> i32 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let spec = since.unwrap_or("24h");
    let Some(start) = parse_since(spec, now) else {
        eprintln!("digest: since what? `24h`, `3d`, `90m` or `today`");
        return 2;
    };
    // which sessions are still running
    let snap = crate::sampler::Sampler::new().build();
    let live: Vec<&str> = snap
        .agents
        .iter()
        .filter_map(|a| a.session.as_deref())
        .collect();

    let mut days: Vec<SessionDay> = Vec::new();
    for path in crate::transcript::written_since(start) {
        let Some(id) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Some(tail) = crate::transcript::read_tail(&path, DIGEST_TAIL) else {
            continue;
        };
        let mut d = tally(id, &tail, start);
        if d.turns == 0 && d.tokens_out == 0 {
            continue; // nothing happened in the window
        }
        d.running = live.contains(&id);
        days.push(d);
    }

    // by repository (or directory)
    let mut by: BTreeMap<PathBuf, Vec<SessionDay>> = BTreeMap::new();
    for d in days {
        let dir = d
            .cwd
            .as_deref()
            .map(|c| crate::model::work_tree(c).unwrap_or_else(|| c.to_path_buf()))
            .unwrap_or_default();
        by.entry(dir).or_default().push(d);
    }
    let mut projects: Vec<ProjectDay> = by
        .into_iter()
        .map(|(dir, mut sessions)| {
            sessions.sort_by_key(|s| std::cmp::Reverse(s.working_secs));
            let (n, recent) = commits(&dir, start);
            ProjectDay {
                project: dir
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "—".into()),
                dir,
                sessions,
                commits: n,
                recent_commits: recent,
            }
        })
        .collect();
    projects
        .sort_by_key(|p| std::cmp::Reverse(p.sessions.iter().map(|s| s.working_secs).sum::<u64>()));

    if json {
        match serde_json::to_string_pretty(&projects) {
            Ok(s) => println!("{s}"),
            Err(e) => eprintln!("marina: json error: {e}"),
        }
        return 0;
    }
    if projects.is_empty() {
        println!("no agent activity since {spec}");
        return 0;
    }
    let all = projects.iter().flat_map(|p| &p.sessions);
    let (work, tin, tout, sessions, prs): (u64, u64, u64, usize, usize) =
        all.fold((0, 0, 0, 0, 0), |acc, s| {
            (
                acc.0 + s.working_secs,
                acc.1 + s.tokens_in,
                acc.2 + s.tokens_out,
                acc.3 + 1,
                acc.4 + s.prs.len(),
            )
        });
    let commits: usize = projects.iter().map(|p| p.commits).sum();
    println!(
        "since {spec}: {sessions} sessions · {} of agent work · {commits} commits · {prs} PRs · {} tokens in, {} out",
        fmt_dur(work),
        fmt_tokens(tin),
        fmt_tokens(tout)
    );
    for p in &projects {
        let work: u64 = p.sessions.iter().map(|s| s.working_secs).sum();
        let prs: usize = p.sessions.iter().map(|s| s.prs.len()).sum();
        println!();
        println!(
            "{}  ·  {} session{} · {} working · {} commit{}{}",
            p.project,
            p.sessions.len(),
            if p.sessions.len() == 1 { "" } else { "s" },
            fmt_dur(work),
            p.commits,
            if p.commits == 1 { "" } else { "s" },
            if prs > 0 {
                format!(" · {prs} PR{}", if prs == 1 { "" } else { "s" })
            } else {
                String::new()
            }
        );
        for s in &p.sessions {
            println!(
                "  {} {} — {} · {} turns · {} tokens",
                if s.running { "●" } else { "○" },
                s.title.as_deref().unwrap_or("untitled"),
                fmt_dur(s.working_secs),
                s.turns,
                fmt_tokens(s.tokens_in + s.tokens_out)
            );
            if let Some(said) = &s.said {
                println!("      {said}");
            }
            for pr in &s.prs {
                println!("      PR {pr}");
            }
        }
        for c in &p.recent_commits {
            println!("  · {c}");
        }
        if p.commits > p.recent_commits.len() {
            println!("  · … {} more", p.commits - p.recent_commits.len());
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tallies_only_the_window() {
        let tail = [
            r#"{"type":"user","cwd":"/Users/me/dev/app","timestamp":"2026-10-22T08:00:00Z"}"#,
            r#"{"type":"system","subtype":"turn_duration","durationMs":600000,"timestamp":"2026-10-22T08:10:00Z"}"#,
            r#"{"type":"assistant","isSidechain":false,"message":{"usage":{"input_tokens":10,"cache_read_input_tokens":1000,"cache_creation_input_tokens":0,"output_tokens":50},"content":[{"type":"text","text":"Old."}]},"timestamp":"2026-10-22T08:09:00Z"}"#,
            r#"{"type":"assistant","isSidechain":false,"message":{"usage":{"input_tokens":5,"cache_read_input_tokens":2000,"cache_creation_input_tokens":100,"output_tokens":70},"content":[{"type":"text","text":"Shipped it. All green."}]},"timestamp":"2026-10-23T09:05:00Z"}"#,
            r#"{"type":"system","subtype":"turn_duration","durationMs":125000,"timestamp":"2026-10-23T09:05:01Z"}"#,
            r#"{"type":"system","subtype":"turn_duration","durationMs":99000,"isSidechain":true,"timestamp":"2026-10-23T09:06:00Z"}"#,
            r#"{"type":"pr-link","prUrl":"https://github.com/o/r/pull/9","timestamp":"2026-10-23T09:05:02Z"}"#,
            r#"{"type":"pr-link","prUrl":"https://github.com/o/r/pull/9","timestamp":"2026-10-23T09:05:03Z"}"#,
        ]
        .join("\n");
        let start = crate::transcript::parse_ts("2026-10-23T00:00:00Z").unwrap();
        let d = tally("abc", &tail, start);
        assert_eq!(
            (d.working_secs, d.turns),
            (125, 1),
            "yesterday's and subagents' excluded"
        );
        assert_eq!((d.tokens_in, d.tokens_out), (2105, 70));
        assert_eq!(d.prs, vec!["https://github.com/o/r/pull/9"]);
        assert_eq!(d.cwd.as_deref(), Some(Path::new("/Users/me/dev/app")));
        assert_eq!(d.said.as_deref(), Some("Shipped it. All green."));
    }

    #[test]
    fn since_specs() {
        assert_eq!(parse_since("24h", 100_000), Some(100_000 - 86_400));
        assert_eq!(parse_since("3d", 1_000_000), Some(1_000_000 - 259_200));
        assert_eq!(parse_since("90m", 10_000), Some(10_000 - 5_400));
        assert!(parse_since("today", 1_792_752_000).unwrap() <= 1_792_752_000);
        assert_eq!(parse_since("soon", 1), None);
        assert_eq!(parse_since("", 1), None);
    }
}
