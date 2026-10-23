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

/// What to start.
#[derive(Clone, Debug, PartialEq)]
pub struct Launch {
    pub dir: PathBuf,
    /// `claude`, `opencode`, `codex`.
    pub agent: String,
    /// Arguments after the agent: a first prompt, or `--resume <id>`.
    pub args: Vec<String>,
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

/// From marina's own environment: inside tmux or herdr, else the terminal
/// app (`TERM_PROGRAM`).
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
    match env("TERM_PROGRAM").as_deref()? {
        "ghostty" => Some(Place::Ghostty),
        "iTerm.app" => Some(Place::ITerm),
        "Apple_Terminal" => Some(Place::Terminal),
        _ => None,
    }
}

/// POSIX single-quoting.
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// `cd '<dir>' && claude '<arg>'…` — for a shell.
fn shell_line(dir: &Path, agent: &str, args: &[String]) -> String {
    let mut line = format!("cd {} && {}", sh_quote(&dir.display().to_string()), agent);
    for a in args {
        line.push(' ');
        line.push_str(&sh_quote(a));
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
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() % 1_000_000)
        .unwrap_or(0);
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

/// Start it. Returns a status line.
pub fn start(req: &Launch, place: Place) -> Result<String, String> {
    let dir = if req.worktree {
        make_worktree(&req.dir)?
    } else {
        req.dir.clone()
    };
    let label = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| req.agent.clone());
    let where_ = crate::ui::tildify(&dir.display().to_string());
    match place {
        Place::Herdr => {
            let d = dir.display().to_string();
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
            let pane = serde_json::from_str::<HerdrCreated>(&created)
                .map_err(|_| "herdr didn't say which pane it created")?
                .result
                .root_pane
                .pane_id;
            // herdr names agents; keep it unique per launch
            let name = format!("{label}-{}", suffix());
            // `agent start` needs the new pane at its shell prompt
            std::thread::sleep(Duration::from_millis(800));
            let mut args = vec![
                "agent".to_string(),
                "start".into(),
                name,
                "--kind".into(),
                req.agent.clone(),
                "--pane".into(),
                pane,
                "--".into(),
            ];
            args.extend(req.args.iter().cloned());
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            // it waits for the agent to be ready for input
            run("herdr", &args, Duration::from_secs(45))?;
        }
        Place::Tmux => {
            let d = dir.display().to_string();
            let mut args = vec!["new-window", "-c", &d, "-n", &label, "--", &req.agent];
            args.extend(req.args.iter().map(String::as_str));
            run("tmux", &args, TIMEOUT)?;
        }
        Place::Ghostty | Place::Terminal | Place::ITerm => {
            let line = shell_line(&dir, &req.agent, &req.args);
            let d = dir.display().to_string();
            let (script, argv): (&str, Vec<&str>) = match place {
                Place::Ghostty => (GHOSTTY_NEW, vec![d.as_str(), line.as_str()]),
                Place::Terminal => (TERMINAL_NEW, vec![line.as_str()]),
                _ => (ITERM_NEW, vec![line.as_str()]),
            };
            let mut cmd = Command::new("osascript");
            cmd.arg("-e").arg(script).args(&argv);
            // a first use may wait on the automation permission prompt
            crate::exec::output(cmd, Duration::from_secs(30))
                .ok_or("couldn't open a window — allow marina's terminal under System Settings › Privacy › Automation")?;
        }
    }
    Ok(format!(
        "started {} in {where_} ({})",
        req.agent,
        place.name()
    ))
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
            &["fix the tests; rm -rf ~".into()],
        );
        assert_eq!(
            l,
            r#"cd '/Users/me/dev/it'\''s' && claude 'fix the tests; rm -rf ~'"#
        );
        assert_eq!(
            shell_line(Path::new("/x"), "claude", &[]),
            "cd '/x' && claude"
        );
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
