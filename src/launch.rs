//! Starting agent sessions from marina (`N`, and resuming an ended one):
//! where you already work — a new herdr workspace, a new tmux window, or a
//! new window of your terminal app — optionally in a fresh git worktree so
//! it can't collide with sessions already in that tree.
//!
//! In a terminal app the command is typed into a new window's shell (not
//! run in place of it), so the session gets your PATH and environment and
//! the window stays when it exits. Everything user-supplied is single-quoted
//! for that shell and passed to AppleScript as argv, never spliced in.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde::Deserialize;

const TIMEOUT: Duration = Duration::from_secs(10);

/// herdr's per-pane variables, which must not leak into sessions started
/// outside herdr.
const HERDR_VARS: [&str; 6] = [
    "HERDR_PANE_ID",
    "HERDR_TAB_ID",
    "HERDR_WORKSPACE_ID",
    "HERDR_SOCKET_PATH",
    "HERDR_ENV",
    "HERDR_BIN_PATH",
];

/// What to start.
#[derive(Clone, Debug, PartialEq)]
pub struct Launch {
    pub dir: PathBuf,
    /// `claude`, `opencode`, `codex`.
    pub agent: String,
    /// A first prompt.
    pub prompt: Option<String>,
    /// A session to resume instead of starting fresh.
    pub resume: Option<String>,
    /// Make a git worktree for it first.
    pub worktree: bool,
}

/// Where marina itself runs, which is where new sessions go.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Place {
    Herdr,
    Tmux,
    Ghostty,
    ITerm,
    Terminal,
}

impl Place {
    pub fn name(self) -> &'static str {
        match self {
            Place::Herdr => "a new herdr workspace",
            Place::Tmux => "a new tmux window",
            Place::Ghostty => "a new Ghostty window",
            Place::ITerm => "a new iTerm2 window",
            Place::Terminal => "a new Terminal window",
        }
    }
}

/// The agent's arguments for a first prompt or a resume — each agent spells
/// them differently, and a prompt must never read as a flag or a
/// subcommand (`opencode <dir>` takes its prompt from `--prompt`).
pub fn agent_args(agent: &str, prompt: Option<&str>, resume: Option<&str>) -> Vec<String> {
    let mut a = Vec::new();
    match (agent, resume) {
        ("codex", Some(id)) => a.extend(["resume".to_string(), id.to_string()]),
        ("opencode", Some(id)) => a.extend(["--session".to_string(), id.to_string()]),
        (_, Some(id)) => a.extend(["--resume".to_string(), id.to_string()]),
        _ => {}
    }
    if let Some(p) = prompt.filter(|p| !p.trim().is_empty()) {
        match agent {
            "opencode" => a.extend(["--prompt".to_string(), p.to_string()]),
            _ => a.extend(["--".to_string(), p.to_string()]),
        }
    }
    a
}

/// From marina's own environment: inside tmux or herdr, else the terminal
/// app (`TERM_PROGRAM`, macOS only — they're driven by AppleScript).
pub fn here() -> Option<Place> {
    let env = |k: &str| std::env::var(k).ok();
    // innermost first: tmux started inside a herdr pane inherits herdr's
    // variables, but marina is in tmux
    if env("TMUX").is_some() {
        return Some(Place::Tmux);
    }
    if env("HERDR_PANE_ID").is_some() {
        return Some(Place::Herdr);
    }
    if !cfg!(target_os = "macos") {
        return None;
    }
    match env("TERM_PROGRAM").as_deref()? {
        "ghostty" => Some(Place::Ghostty),
        "iTerm.app" => Some(Place::ITerm),
        "Apple_Terminal" => Some(Place::Terminal),
        _ => None,
    }
}

/// Single-quoting for the user's shell: POSIX `'…'\''…'`, or fish's,
/// which also treats `\\` and `\'` as escapes inside single quotes.
pub fn sh_quote_for(s: &str, fish: bool) -> String {
    if fish {
        format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// Is the login shell fish? (Its quoting differs.)
fn user_shell_is_fish() -> bool {
    std::env::var("SHELL").is_ok_and(|s| s.rsplit('/').next() == Some("fish"))
}

/// `cd '<dir>' && claude '<arg>'…` — typed into the user's shell.
fn shell_line(dir: &Path, agent: &str, args: &[String], fish: bool) -> String {
    let q = |s: &str| sh_quote_for(s, fish);
    let mut line = format!("cd {} && {}", q(&dir.display().to_string()), agent);
    for a in args {
        line.push(' ');
        line.push_str(&q(a));
    }
    line
}

/// A worktree beside the repo: `<repo>-<stamp>` on branch `marina/<stamp>`.
fn make_worktree(dir: &Path) -> Result<PathBuf, String> {
    let root = crate::model::work_tree(dir).ok_or("not in a git repository")?;
    let name = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".into());
    // seconds plus sub-second digits: unique across launches
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| format!("{}{:03}", d.as_secs(), d.subsec_millis()))
        .unwrap_or_default();
    let path = root
        .parent()
        .ok_or("repository has no parent directory")?
        .join(format!("{name}-{stamp}"));
    let branch = format!("marina/{stamp}");
    let out = Command::new("git")
        .arg("-C")
        .arg(&root)
        .args(["worktree", "add", "-b", &branch])
        .arg(&path)
        .output()
        .map_err(|e| format!("git: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git worktree add failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    // keep the same subdirectory, if the session was in one
    let sub = dir.strip_prefix(&root).unwrap_or(Path::new(""));
    Ok(path.join(sub))
}

#[derive(Deserialize)]
struct HerdrCreated {
    result: HerdrCreatedResult,
}
#[derive(Deserialize)]
struct HerdrCreatedResult {
    root_pane: HerdrPane,
}
#[derive(Deserialize)]
struct HerdrPane {
    pane_id: String,
}

fn run(program: &str, args: &[&str], timeout: Duration) -> Result<String, String> {
    let mut cmd = Command::new(program);
    cmd.args(args);
    crate::exec::output(cmd, timeout)
        .ok_or_else(|| format!("{program} {} failed", args[..2.min(args.len())].join(" ")))
}

const GHOSTTY_NEW: &str = r#"on run argv
  tell application "Ghostty"
    set cfg to new surface configuration
    set initial working directory of cfg to (item 1 of argv)
    set initial input of cfg to (item 2 of argv) & return
    new window with configuration cfg
    activate
  end tell
end run"#;

const TERMINAL_NEW: &str = r#"on run argv
  tell application "Terminal"
    do script (item 1 of argv)
    activate
  end tell
end run"#;

const ITERM_NEW: &str = r#"on run argv
  tell application "iTerm2"
    set w to (create window with default profile)
    tell current session of w to write text (item 1 of argv)
    activate
  end tell
end run"#;

/// Start it. Returns a status line. A failure undoes what it made — the
/// worktree and its branch, a herdr workspace — so nothing is left behind.
pub fn start(req: &Launch, place: Place) -> Result<String, String> {
    let args = agent_args(&req.agent, req.prompt.as_deref(), req.resume.as_deref());
    let made = if req.worktree {
        Some(make_worktree(&req.dir)?)
    } else {
        None
    };
    let dir = made.clone().unwrap_or_else(|| req.dir.clone());
    let started = open(&dir, &req.agent, &args, place);
    if started.is_err() {
        if let Some(wt) = &made {
            remove_worktree(wt);
        }
    }
    started?;
    Ok(format!(
        "started {} in {} ({})",
        req.agent,
        crate::ui::tildify(&dir.display().to_string()),
        place.name()
    ))
}

/// Undo `make_worktree`: the worktree, then its branch.
fn remove_worktree(dir: &Path) {
    let Some(root) = crate::model::work_tree(dir) else {
        return;
    };
    let branch = Command::new("git")
        .arg("-C")
        .arg(&root)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    let _ = Command::new("git")
        .arg("-C")
        .arg(&root)
        .args(["worktree", "remove", "--force"])
        .arg(&root)
        .output();
    if let Some(b) = branch.filter(|b| b.starts_with("marina/")) {
        let _ = Command::new("git")
            .args(["branch", "-D", &b])
            .current_dir(root.parent().unwrap_or(Path::new("/")))
            .output();
    }
}

fn open(dir: &Path, agent: &str, args: &[String], place: Place) -> Result<(), String> {
    let label = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| agent.to_string());
    let d = dir.display().to_string();
    match place {
        Place::Herdr => {
            let created = run(
                "herdr",
                &[
                    "workspace",
                    "create",
                    "--cwd",
                    &d,
                    "--label",
                    &label,
                    "--focus",
                ],
                TIMEOUT,
            )?;
            let created: HerdrCreated = serde_json::from_str(&created)
                .map_err(|_| "herdr didn't say which pane it created")?;
            let pane = created.result.root_pane.pane_id;
            // herdr names agents; keep it unique per launch
            let name = format!("{label}-{}", suffix());
            // `agent start` needs the new pane at its shell prompt
            std::thread::sleep(Duration::from_millis(800));
            let mut argv = vec![
                "agent".to_string(),
                "start".into(),
                name,
                "--kind".into(),
                agent.to_string(),
                "--pane".into(),
                pane.clone(),
                "--".into(),
            ];
            argv.extend(args.iter().cloned());
            let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
            // it waits for the agent to be ready for input
            if let Err(e) = run("herdr", &argv, Duration::from_secs(45)) {
                // don't leave an empty workspace behind
                if let Some(ws) = pane.split(':').next() {
                    let _ = run("herdr", &["workspace", "close", ws], TIMEOUT);
                }
                return Err(e);
            }
        }
        Place::Tmux => {
            // through the user's login shell, so it has their PATH and
            // aliases; the shell `exec`s the agent
            let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
            let fish = user_shell_is_fish();
            let mut cmd = format!("exec {agent}");
            for a in args {
                cmd.push(' ');
                cmd.push_str(&sh_quote_for(a, fish));
            }
            // without herdr's variables: a tmux server started inside a herdr
            // pane hands them down, and the agent's hooks would then report
            // to that pane as if it were the session there
            let mut argv: Vec<String> = ["new-window", "-c", &d, "-n", &label, "--", "env"]
                .map(String::from)
                .to_vec();
            for k in HERDR_VARS {
                argv.extend(["-u".to_string(), k.to_string()]);
            }
            argv.extend([shell, "-lc".into(), cmd]);
            let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
            run("tmux", &argv, TIMEOUT)?;
        }
        Place::Ghostty | Place::Terminal | Place::ITerm => {
            let line = shell_line(dir, agent, args, user_shell_is_fish());
            let (script, argv): (&str, Vec<&str>) = match place {
                Place::Ghostty => (GHOSTTY_NEW, vec![d.as_str(), line.as_str()]),
                Place::Terminal => (TERMINAL_NEW, vec![line.as_str()]),
                _ => (ITERM_NEW, vec![line.as_str()]),
            };
            let mut cmd = Command::new("osascript");
            cmd.arg("-e").arg(script).args(&argv);
            // a first use may wait on the automation permission prompt
            crate::exec::output(cmd, Duration::from_secs(30)).ok_or(
                "couldn't open a window — allow marina's terminal under System Settings › Privacy › Automation",
            )?;
        }
    }
    Ok(())
}

/// Four varying digits, so launches get distinct herdr agent names.
fn suffix() -> u32 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() % 10_000)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_lines_quote_everything_user_supplied() {
        let l = shell_line(
            Path::new("/Users/me/dev/it's"),
            "claude",
            &["--".into(), "fix the tests; rm -rf ~".into()],
            false,
        );
        assert_eq!(
            l,
            r#"cd '/Users/me/dev/it'\''s' && claude '--' 'fix the tests; rm -rf ~'"#
        );
        assert_eq!(
            shell_line(Path::new("/x"), "claude", &[], false),
            "cd '/x' && claude"
        );
        // fish: backslash and quote escapes inside single quotes
        assert_eq!(sh_quote_for(r"x\'; rm", true), r"'x\\\'; rm'");
    }

    #[test]
    fn prompts_never_read_as_flags_or_subcommands() {
        assert_eq!(
            agent_args("claude", Some("-p hi"), None),
            vec!["--", "-p hi"]
        );
        assert_eq!(
            agent_args("opencode", Some("upgrade"), None),
            vec!["--prompt", "upgrade"]
        );
        assert_eq!(
            agent_args("claude", None, Some("abc")),
            vec!["--resume", "abc"]
        );
        assert_eq!(agent_args("codex", None, Some("t1")), vec!["resume", "t1"]);
        assert_eq!(
            agent_args("opencode", None, Some("ses_1")),
            vec!["--session", "ses_1"]
        );
        assert!(agent_args("claude", Some("  "), None).is_empty());
    }

    #[test]
    fn worktrees_go_beside_the_repo_on_their_own_branch() {
        let base = std::env::temp_dir().join(format!("marina-launch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let repo = base.join("app");
        std::fs::create_dir_all(repo.join("web")).unwrap();
        let git = |args: &[&str]| {
            Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .output()
                .unwrap()
        };
        git(&["init", "-q", "-b", "main"]);
        std::fs::write(repo.join("web/a.txt"), "a").unwrap();
        git(&["add", "."]);
        git(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            "init",
        ]);
        let wt = make_worktree(&repo.join("web")).unwrap();
        assert!(
            wt.starts_with(&base) && wt.ends_with("web"),
            "{}",
            wt.display()
        );
        assert!(wt.join("a.txt").is_file(), "same subdirectory, checked out");
        assert!(make_worktree(&base).is_err(), "not a repo");
        let _ = std::fs::remove_dir_all(&base);
    }
}
