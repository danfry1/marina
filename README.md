# marina

**One inbox for every coding-agent session you have running — and the dev
servers they start.**

Claude Code, Codex and opencode sessions spread across terminals, panes and
worktrees. marina is a TUI you leave open beside them that says which ones need
you, which have finished and are waiting on your answer, and which are still
working — and lets you approve, reply, start, end and resume them from one place:

```
 NEEDS YOU · 1
▌● api                approve: Bash: cargo publish                             now
 YOUR TURN · 2
 ● web                Want me to commit it all now? — Nothing is committed yet…  2h
 ● docs               Say "push" when you want them shipped.                     5h
 WORKING · 1
 ◐ marina ⇄1          $ cargo test                                               now
 ▸ PARKED · 9   finished, nothing asked · Enter shows · X ends them
```

It also tracks the dev servers and processes behind them — resolved into names
you recognize (`client-portal · next dev · :3000 · 340MB`), with who started
each one, and one-key kill, restart and logs. Not a system monitor (btop/bottom
own that), and not an `lsof` wrapper.

![the servers view: grouping, logs, sort and filter](https://raw.githubusercontent.com/danfry1/marina/main/demo/marina.gif)

**Getting set up:**

```sh
brew install danfry1/tap/marina
marina demo              # try the inbox on made-up sessions first (nothing real is touched)
marina hooks --install   # agents report their own state (see below)
marina doctor            # what marina can see and do here, and what's left to set up
marina                   # opens on the inbox when agents are running
```

![the marina inbox: approving, replying, and sessions finishing](https://raw.githubusercontent.com/danfry1/marina/main/demo/inbox.gif)

> A marina is a harbour full of berthed boats — a cockpit of dev services each
> occupying a port. `marina` enumerates what's listening, resolves each to a
> project / tool / URL, groups them, and lets you act on them with one keystroke.

## Features

- **Live, stable TUI** — grouped by project; no flicker, no cursor-jump while you
  navigate, and it only redraws when something changed — an idle cockpit does
  near-zero work. Mouse works too (click to select, click a header to sort,
  wheel to move / scroll logs).
- **Smart resolution** — infers project + tool from cwd, argv, package manifests
  (package.json / Cargo.toml / pyproject / go.mod / …) and git (worktrees and
  detached HEADs included); sees through `pnpm`/`npm`/`yarn`/`node`/corepack
  wrappers to the real tool (`pnpm dev` → `vite`, `pnpm exec vp dev` → `vp dev`,
  `python -m http.server` → `http.server`).
- **First-class verbs** — kill (fingerprint-verified SIGTERM→SIGKILL escalation,
  `u` cancels the force-kill), restart (waits for the port to free, captures the
  new process's output to a log), tail logs inline, copy URL, open in browser.
  Docker rows stop/restart/tail via the docker CLI.
- **Who started it** — a `VIA` column names the launcher of every server: the
  coding-agent session (`claude`, `codex`, `opencode`, `cursor-agent`, …), the
  editor, or the terminal — even after the agent backgrounded it with
  `&`/`nohup`, via the marker variables agents export (Claude Code, Codex,
  opencode, Cursor, Copilot, Gemini, Amp, Goose, Cline, Qwen, …). Agent sessions
  are told apart by session/thread id, pid and working directory, and the
  inspect panel shows how to get back in (`claude --resume <id>`,
  `codex resume <id>`, `opencode --session <id>`). A server whose Claude Code
  session has **ended** is flagged `claude·ended` — an orphan — and a server
  nobody owns any more reads `detached`.
  `marina kill --orphaned` cleans up after finished sessions; an agent can run
  `marina kill --mine` to stop exactly the servers it started.
- **Runaway processes** — a server or watcher using 90%+ CPU or 2GB+ memory
  (a test runner left spinning, a leak) is flagged: its numbers turn red, the
  servers view counts `⚠ N heavy`, and marina says so once when it starts —
  select it and `K`.
- **Honest signals** — rows flash green when they appear, turn red while dying,
  and if a long-running server vanishes *without* you killing it, marina says so
  (`⚠ client-portal (:3000) exited unexpectedly`). A `:3000!` port badge warns
  when a server is bound to `0.0.0.0` (reachable from your LAN).
- **Grouping** — a project's services collapse under one header, and one keystroke
  kills the whole project. Declared groups bundle an app with its database.
- **Session view** — press `v` to group by *who started it* instead: one header
  per agent session (`claude·fal-app`, `codex·api`), editor, or terminal, across
  projects. `K` on a session header stops everything that session started.
- **Agent sessions** — every running coding-agent session (Claude Code,
  opencode, Codex, …) is listed, even one with no servers: a folded
  `▸ agents (21)  3 working · 13 stale · 14GB` section at the bottom of the
  project view, and a row per session in the session view. Each shows what it
  is doing (`$ cargo test`, `working`, `idle 12m`), its cpu/mem (agent + MCP
  helpers), dir, branch and session id; an agent's background server
  (`opencode serve`) is shown just above that section, always visible, as
  `opencode server`, rather than as a project among the dev servers; `i` lists its shells and helpers and
  the resume command. Working vs idle comes from when its terminal was last
  written to, like `w`'s IDLE column. Sessions whose terminal has had no input
  for over a day are dimmed as *stale* — usually a forgotten session holding
  memory. Background Claude Code
  sessions are included; daemons and pty hosts are not. marina never stops an
  agent: `K` on an agent row explains why, and `K` on its header stops only
  its servers.
- **Jump to a session** — an agent row's `VIA` says where it lives
  (`herdr·ghostty`, `tmux·iterm`, `ghostty`, `—` for no terminal), and
  `Enter` focuses it there — switching the multiplexer's pane and raising
  the terminal app showing it:
  a herdr, tmux or Zellij pane, a kitty window, or the iTerm2 session / Terminal tab / WezTerm pane on
  its tty, or a Ghostty terminal in its directory (macOS asks once to let your
  terminal control that app). Anywhere else it copies
  `cd <dir> && claude --resume <id>` to paste into a terminal. Under herdr, marina also shows herdr's view of the
  session: its task title, and **needs you** when the agent is waiting on an
  approval or question.
- **What it's costing** — `i` shows a two-minute cpu sparkline, peak and
  memory drift for any row (`cpu ▁▁▃▇▅▂ 4.1% · peak 38% · mem 540MB +12MB
  over 2m`), and each helper's memory. Helpers that every session runs its
  own copy of are totalled (`chrome-devtools-mcp ×19 899MB`) on the agents
  section and at the end of `marina agents`.
- **Agents report their own state** — `marina hooks --install` registers a
  small hook with each agent it finds: Claude Code's and Codex's hooks
  (`~/.claude/settings.json`, `~/.codex/hooks.json`) and an opencode plugin.
  Each call writes one tiny file per session to `~/.local/state/marina/reports`,
  so *needs you*, *working* and *done* come from the agent as they happen
  rather than being inferred from its transcript and screen. Other hooks in
  those files are left untouched (the previous file is kept as
  `*.marina-backup`), and `marina hooks --uninstall` takes ours out again.
  Without hooks, marina still infers it all (below).
- **Inbox** — where marina opens when agents are running (`a` switches to
  the servers view), organised by what each session needs from you:
  *needs you* (an approval or a question is up), *your turn* (it finished
  and asks you something — "Want me to commit it all now?"), *working*,
  and, folded away, *parked* (finished, nothing asked) and *ended*
  (exited lately; Enter resumes one). One line per session says what it
  wants, from Claude's own "Next…" recap or the question its reply ends
  with. Sessions with something new are bold until you look; the order
  only changes when a session finishes, and holds still while you browse.
  The detail says *why* each session is where it is ("its last reply asks
  you something", "inferred: a tool call is pending and nothing is
  running", "it reported this itself"), and says up front when a session
  sits in a terminal marina can't type into. `d` sets a session aside —
  parked until it does something new — and `X` on the *parked* fold ends
  every parked session marina can type into at once (after a second press).
  The detail beside it leads with *your turn* and *what it did*, then one
  line of context (directory, branch and changes, PR, servers, shared
  tree, context size) and the full conversation (`[` `]` to scroll); `p`
  switches it to the session's live screen (herdr, tmux, Zellij, iTerm2,
  WezTerm and kitty).
- **Subagents** — the subagents a session starts (Claude Code's Agent
  tool, Codex's spawned threads) are listed under it, not as sessions of
  their own: the detail shows each one running with what it's on, and the
  ones finished in the last half hour with what they said. A session
  counts as working while any of its subagents is, so one waiting on
  background agents isn't shown as done, and `X` won't end it mid-way.
  `marina agents --long/--json` list them too.
- **Land the work** (`L`) — on a session (or an ended one): its branch,
  how far ahead of the main checkout's branch, and what's uncommitted, with
  what can be done from there. `p` has the agent commit, push and open a
  pull request (it writes the message); `m` fast-forwards the main
  checkout's branch to it — only when both trees are clean and it's
  strictly ahead; `w` removes its worktree, and its branch once that's
  merged (an unmerged branch is kept, commits and all) — only when nothing
  would be lost: no uncommitted or untracked files, no ignored files beyond
  dependency and build folders (an `.env` blocks it), no detached commits
  nothing else points to, and no session or server still running there.
  Each is re-checked at the moment it runs; if any of it can't be read,
  nothing is offered; nothing is ever forced.
- **Open what it started** — when an agent brings up a web server, marina
  says so (`claude·web started vite on http://localhost:5173 — O on its
  session opens it`), and `O` on the session opens it in the browser (`Y`
  copies it); with several, pressing again moves to the next. From a
  terminal, `marina open` opens the servers running in the current
  directory, and an agent can run `marina open --mine` for its own.
- **Preview** (`p`) — the same session detail under the project view.
- **Needs you, anywhere** — under herdr marina takes herdr's word; with
  hooks installed, the agent's own; elsewhere
  a session counts as waiting when a turn is under way in its transcript
  (your prompt or a tool call with no end-of-turn record after it) yet
  nothing is running — no shell, a still screen, or a Bash call whose
  command never started: an approval prompt or a question. Notices say what it wants:
  `claude·api needs you: wants to run Bash: cargo publish`, and finished
  ones what it said.
- **Background notices** — `marina watch --install` keeps a small watcher
  running at login (a launchd agent on macOS, a systemd user service on
  Linux) that sends the same notices with marina closed; an open TUI
  leaves desktop notifications to it. With `terminal-notifier` installed
  (macOS) or a `notify-send` that supports actions (Linux), clicking a
  notice runs `marina jump <session>` and takes you to it.
- **Shared work trees** — sessions in the same git work tree carry `⇄N`
  on their card and a warning in the detail; when two are *working* in one
  at once, marina says so, since their edits can overwrite each other (a
  `git worktree` each keeps them apart).
- **Start, end and resume sessions** — `N` starts a session in the
  selected row's directory with an optional first prompt, where you work:
  a new herdr workspace, a new tmux window, or a new Ghostty / iTerm2 /
  Terminal window (the command is typed into that window's shell, so it
  gets your PATH); `Tab` puts it in its own `git worktree` beside the repo
  so it can't collide with sessions already there. `X` (twice) ends a
  herdr, tmux, Zellij, iTerm2, WezTerm or kitty session with its exit command, warning first about
  uncommitted work or servers it leaves running. The inbox lists sessions
  that exited in the last two days under *ended* — Claude Code, Codex CLI
  and opencode alike; `Enter` resumes one (`claude --resume <id>`,
  `codex resume <id>`, `opencode --session <id>`) the same way `N` starts
  one, and `L` lands its work.
- **Daily digest** — `marina digest [today|24h|3d]` summarises your agents'
  work per project: sessions, time spent in turns, tokens used, commits
  made in the repo, PRs opened, and each session's latest recap — a
  standup in one command (`--json` for scripts).
- **Every agent's conversation** — Claude Code sessions are summarised
  from their transcripts, Codex sessions from their rollout files
  (`~/.codex/sessions`), and opencode sessions from its background service
  (`opencode session export`), so the inbox and preview work the same for
  each.
- **Quick replies** — `1`–`9` on a session open the reply box with a canned
  reply (`commit it`, `push it`, `run the tests`, `continue` — set your own
  with `[agents] quick`); Enter sends it.
- **Answer from marina** — on a herdr, tmux, Zellij, iTerm2, WezTerm or kitty session: `y` approves what
  it's asking, `n` rejects it, `r` types a reply (dismissing a waiting
  dialog first). Each re-reads the session's screen at the moment of
  sending: `y` sends Enter only if the highlighted choice is a *Yes*
  (Claude Code's folder-trust prompt highlights *No*), and `n` sends Esc
  only if a dialog is up, so neither can interrupt a working agent. Only on your keystroke, only to the selected session, and
  never into another terminal (Ghostty, Terminal, …) — `Enter` jumps there
  instead.
- **What each agent changed** — the BRANCH column leads with the work tree's
  state (`●4 ↑2 main`: 4 files uncommitted, 2 commits unpushed; `✓` clean),
  and `D` opens the uncommitted diff (stat, untracked files, patch) in the
  pane below. A stale session with no servers and a clean, pushed tree is
  marked *looks finished* — the one to close first.
- **Agent notices** — when a session finishes a stretch of work (30s+, then
  quiet) or starts waiting on you, a desktop notification says who and
  what: `claude·api needs you` — *wants to run pnpm migrate*, or its
  question (*Should it be per user or per API key?*);
  `claude·docs finished after 12m` with the first line of its reply, or
  `· your turn` and its question when it ended on one. Sessions finishing
  together arrive as one (`3 sessions finished` — *claude·docs, claude·web,
  codex·api*), which jumps to the first when clicked; each that needs you
  keeps its own, and clicking it jumps to that session (`[notify] agents
  = false` turns these off).
- **Orphan notices** — when an agent session ends and leaves servers running,
  marina says so once — `⚠ claude session in ~/dev/app ended — left 2 servers
  running (480MB)` — in the status line and as a desktop notification
  (`osascript` / `notify-send`; turn off with `[notify] desktop = false`).
- **Agent/script CLI** — `marina ls --json`, `marina kill <project>`,
  `marina who 3000` / `marina free 3000`, `marina agents`, sharing the exact
  same resolution engine as the TUI.

## Install

**Homebrew** (macOS / Linux):

```sh
brew install danfry1/tap/marina
```

**Prebuilt binary** — macOS (arm64/x64) and Linux (x64), from
[Releases](https://github.com/danfry1/marina/releases/latest):

```sh
# macOS (Apple silicon) — adjust the target for your platform
curl -L https://github.com/danfry1/marina/releases/latest/download/marina-aarch64-apple-darwin.tar.gz | tar xz
./marina
```

**From crates.io** (package is `marina-tui`; the installed command is `marina`):

```sh
cargo install marina-tui
```

**Nix** (flakes):

```sh
nix run github:danfry1/marina               # run without installing
nix profile install github:danfry1/marina   # install
```

**From source:**

```sh
cargo build --release && ./target/release/marina
```

Runs on **macOS and Linux**. On Linux, `O` (open) uses `xdg-open` and `Y` (copy)
uses `wl-copy`/`xclip`; accurate `phys_footprint` memory is macOS-only (Linux
falls back to RSS).

## Run it

However you installed it, the command is **`marina`** (on your `PATH`):

```sh
marina        # launch the TUI — leave it open in a pane while you work
marina ls     # or a one-shot list (add --json for scripts/agents)
```

No flags or config needed — it auto-discovers your running dev servers. Press
`?` inside the TUI for the full key list.

## Usage

**TUI** — `marina`

| key | action |
|---|---|
| `j` / `k`, `g` / `G` | move / jump to top·bottom (mouse: click / wheel) |
| `Enter` | fold / unfold a group · on an agent row: jump to its pane |
| `D` | uncommitted diff of the selected agent's / server's work tree |
| `a` | switch between the agent inbox (where marina opens when agents run) and the servers view |
| `p` | preview an agent: last prompt, recap and reply · again: its live screen |
| `y` · `n` · `r` | approve · reject · reply to the selected agent (herdr / tmux panes) |
| `1`–`9` | quick reply: opens the reply box with a canned reply (Enter sends) |
| `N` · `X` | start a session here (Tab: own git worktree) · end the selected one (twice; on *parked*: all of them) |
| `d` | set a session aside: parked until it does something new (again: bring it back) |
| `v` | group by project ↔ by agent session / launcher (lists agent sessions) |
| `i` | inspect the selection (command, ports, cwd, launcher, pids) |
| `/` | filter (project / command / port / cwd / branch / launcher — `/claude`, `/ended`) |
| `s` | cycle sort (port / cpu / mem) — or click a column header |
| `K` · `u` | kill selection · cancel the pending force-kill |
| `R` · `T` | restart (output captured to a log) · tail logs |
| `[` / `]`, `+` / `-` | scroll / resize the log pane |
| `Y` · `O` | copy URL · open in browser — on an agent session, the web server it started (press again for the next) |
| `Esc` | close pane / clear filter |
| `q` / `Ctrl+C` | quit |

`K`/`R` on a group header act on the whole project. Docker rows map to
`docker stop` / `docker restart` / `docker logs -f`.

**CLI** (for scripts and agents)

```sh
marina ls [sel…] [--json]     # the snapshot — table, or a stable JSON contract
marina ls --mine              # only servers started by the agent session running this
marina kill --orphaned        # stop servers whose agent session has ended
marina kill <selector>        # project name, port (3000 / :3000), or command
marina restart <selector>     # output captured to ~/.local/state/marina/logs/
marina url <selector> [--json]
marina open [selector] [--mine]   # open servers in the browser; bare: the ones running in this directory
marina who <port>… [--json]   # what's holding a port — dev target or not
marina free <port>… [--json]  # stop the dev target on a port, wait until it's released
marina port [name] [--json]   # a stable, collision-free port for this project/worktree
marina port --list            # every assigned port · --release [name] gives one back
marina agents [sel…] [--json] # agent sessions: shells, helpers, servers, resume command
marina agents --long          # …and what each last said, what needs you first
marina jump <session>         # focus a session's terminal (id, pid or project)
marina digest today           # what your agents did: per project, time working,
                              # tokens, commits, PRs, latest recaps (or 24h, 3d)
marina watch --install        # background notices at login (--status, --uninstall)
marina hooks --install        # agents report their own state (--uninstall; bare: status)
marina doctor                 # what marina can see and do here, and what to set up
marina demo                   # the TUI on made-up sessions (approve, reply, end act on them)
marina version
```

`port` is for running many copies of a project at once — parallel agents in
parallel git worktrees all reaching for `:3000`:

```sh
pnpm dev --port $(marina port)        # in each worktree: its own port, every time
PORT=$(marina port api) cargo run     # several services in one tree: name them
```

Each project root (each worktree, each package of a monorepo) gets its own port
from `3100–3999` (`[ports] range` in config), chosen by a stable hash and
remembered in `~/.local/state/marina/ports.json`. Assignments are made under a
file lock, so concurrent callers never collide; a port taken over by another
project's process is replaced (with a note on stderr), while this project's own
server keeps it. Assignments for deleted worktrees are dropped automatically.

`who` and `free` are for the `EADDRINUSE` moment:

```sh
$ marina who 3000
:3000  client-portal · next dev · pid 4121 · up 3d · feat/x · ~/dev/client-portal
$ marina free 3000 && pnpm dev
:3000  freed — stopped client-portal · next dev
```

Both take ports only (no name matching). `who` exits `1` when the port is free.
`free` is idempotent — an already-free port is success — and it refuses to touch
a holder that isn't a dev target (e.g. macOS ControlCenter on `:5000`), naming it
instead.

A selector matching a project name acts on **every** service under it. Exit
codes: `0` ok, `1` no match, `2` usage error — and unknown commands/flags are
errors, they never fall through to the TUI.

## Use with AI agents

The CLI is built to be driven by coding agents. The usage instructions are a
tool-neutral Markdown skill — [.agents/skills/marina/SKILL.md](.agents/skills/marina/SKILL.md).
Install it into your agent's skills directory:

```sh
mkdir -p .agents/skills/marina        # or ~/.agents/skills/marina for every project
curl -fsSL https://raw.githubusercontent.com/danfry1/marina/main/.agents/skills/marina/SKILL.md \
  -o .agents/skills/marina/SKILL.md
```

Claude Code looks in `~/.claude/skills/` instead — same file, that path. For any
other agent, point it at the file or load the directory however it discovers skills.

Then ask *"what's running?"*, *"kill the client-portal project"*, or *"what's on
:3000?"* and the agent drives `marina ls --json` / `marina kill <project>`.

## Config

Optional `~/.config/marina/config.toml` (respects `$XDG_CONFIG_HOME`):

```toml
[[rule]]                       # classify a command -> label (+ optional URL)
match_cmd = "next dev"
label     = "next dev"
url       = "http://localhost:{port}"

[[watch]]                      # port-less workloads to surface (e.g. watchers)
match_cmd = "tsc.*--watch|jest|vitest"
label     = "watcher"

[[override]]                   # pin a stubborn target
match_port = 3000
project    = "client-portal"

[[group]]                      # bundle services that don't share a cwd (app + db)
name    = "client-portal"
members = [3000, 5432, "worker"]

[[ignore]]                     # hide noise the heuristics keep picking up
match_cmd  = "OrbStack|CloudSyncAgent"   # and/or match_port = 7000

[notify]
desktop = false                # no desktop notifications at all
agents  = false                # no "finished" / "needs you" agent notices

[agents]
transcripts = false            # don't read Claude Code transcript tails
                               # (preview, inbox, agents --long)
quick = ["commit it", "push it", "run the tests", "continue"]
                               # replies on keys 1-9 (these are the default)
start = "servers"              # open on the servers view instead of the inbox
```

## Privacy & security

marina is deliberately boring on this front:

- **Local only — no network, ever.** There is no HTTP/TLS or networking client in
  the dependency tree, and no code that opens a connection. No telemetry, no
  phone-home, no data leaves your machine. (`netstat2`/`libproc` *read* the OS's
  socket and process tables locally; `mio` polls the terminal for keystrokes.)
- **Almost nothing is persisted.** marina reads, displays, and forgets. The only
  optional file it *reads* is `~/.config/marina/config.toml`. Its writes are
  user-triggered and live under `~/.local/state/marina/`: when **you** restart a
  process (`R` / `marina restart`), the new process's stdout/stderr is captured
  to `logs/<project>.log` so `T` can always tail it; and `marina port` records
  its assignments (port, project root, optional service name, timestamps) in
  `ports.json`. The inbox keeps which replies you've read (`seen.json`) and
  which sessions you set aside (`aside.json`) — session ids and timestamps.
  With hooks installed, each agent writes `reports/<agent>-<session>.json`:
  its state (working / blocked / idle), the time, and its process ids —
  never the prompt, tool input or output it was handed (deleted after a
  week). `marina hooks --install` edits the agents' own settings only when
  you run it, keeps the previous file as `*.marina-backup`, and touches no
  other hook.
- **Your processes, your permissions.** It runs unprivileged (no `sudo`) and only
  ever inspects your own user's processes — system/root daemons and anything
  outside `$HOME` are filtered out (and the OS wouldn't let it read others
  anyway). It also never lists the session it runs in (your shell / terminal /
  `ssh`), so you can't accidentally kill your own connection. It's the same class
  of introspection `ps`, `lsof`, and your IDE already do.
- **What it reads:** a process's `cwd`, argv, cpu/memory; a fixed allowlist of
  agent-marker environment variables (`CLAUDE_PID`, `CLAUDE_CODE_SESSION_ID`,
  `CODEX_THREAD_ID`, `OPENCODE_SESSION_ID`, `AI_AGENT`, `AGENT`, … — the full
  list is `MARKERS` in [src/launcher.rs](src/launcher.rs)), used to attribute a
  server to its agent session. Of those, only pids and validated session ids
  are kept; flag values are never read, and besides those and the four
  that name a Zellij pane or kitty window (below), no variable is kept,
  displayed, or serialized; for agent sessions, their terminal device's
  last-written / last-read timestamps (never the screen contents), their work
  tree's `git status` / `git diff --shortstat` (run with
  `GIT_OPTIONAL_LOCKS=0`, so marina never takes a lock an agent needs), and —
  only when herdr or tmux is running — those tools' own pane listings
  (`herdr agent list`, `tmux list-panes`; a session whose dialog is
  inferred has its tmux or Zellij screen read to confirm it); the nearest project
  manifest's `name` (package.json / Cargo.toml / …); `.git/HEAD` for the branch;
  and, only when you press `T` to tail logs, its open file descriptors (via
  `lsof`) and the discovered log file.
- **Agent transcripts and screens.** For the preview, the inbox,
  `marina digest` and `marina agents --long/--json`, marina reads the
  *tail* of each Claude Code session's transcript (`~/.claude/projects/*/<session>.jsonl`;
  the last 256KB, more only if the last reply is further back): its title,
  your last prompt, its last reply and recap, a pending tool call, and a PR
  link (the digest also reads turn durations and token counts). For its
  subagents it reads `<session>/subagents/agent-*.meta.json` and the tail of
  each subagent transcript beside it. Codex
  rollouts under `~/.codex/sessions` are read the same way (the first record
  of the last two days' rollouts names any parent thread), and opencode
  summaries come from `opencode session export` while its service runs;
  its ended sessions are listed from its database (`opencode.db`, opened
  read-only with the `sqlite3` CLI: ids, directories, titles, times, and
  whether a session has any message).
  Nothing else from `~/.claude` is read — never credentials or
  settings — and nothing is written or sent anywhere. `[agents] transcripts
  = false` turns this off. The live screen of a herdr, tmux, Zellij, iTerm2, WezTerm or kitty
  session is read (`herdr agent read`, `tmux capture-pane`,
  `zellij action dump-screen`, iTerm2's scripting, `wezterm cli get-text`,
  `kitten @ get-text`) while its preview is open, when you answer it, and —
  except iTerm2, which is never scripted in the background, so macOS asks
  for the automation permission only after a keystroke — to confirm an
  inferred dialog. Which Zellij pane or kitty window a session is in comes
  from four variables in its environment (`ZELLIJ_SESSION_NAME`,
  `ZELLIJ_PANE_ID`, `KITTY_WINDOW_ID`, `KITTY_LISTEN_ON`); no other
  variable is kept. kitty needs remote control on:
  `allow_remote_control socket-only` and `listen_on unix:/tmp/kitty` in
  `kitty.conf` (`marina doctor` says so when a session runs in a kitty
  without it).
- **Secrets stay internal.** Command-line args can contain tokens/passwords, so
  marina **never displays, serializes, or logs raw argv** — the UI and
  `ls --json` show only derived labels (`next dev`, `vite`); even the inspect
  panel shows just the program path and an arg count. argv is used for
  classification and captured for `restart`, and never leaves the process.
- **Outbound actions are only the ones you trigger:** `O` opens a URL in your
  browser, `Y` copies to the clipboard, `K`/`R` send signals to *your* processes,
  `Enter` on an agent asks its multiplexer or terminal app to focus it,
  and `y` / `n` / `r` on an agent send that keystroke or your reply to its
  pane or window (`herdr agent send-keys` / `prompt`, `tmux send-keys`,
  `zellij action write-chars`, iTerm2's `write text`,
  `wezterm cli send-text --no-paste`, `kitten @ send-text`). The one unprompted
  action is a local desktop notification when an agent session leaves servers
  behind, finishes, or needs you (`[notify] desktop = false` disables it).

## Design

See [DESIGN.md](./DESIGN.md) and the glossary in [CONTEXT.md](./CONTEXT.md).

## Status

macOS + Linux · ~8.3k LOC · 127 tests (CI builds + tests on both). Docker container
naming + verbs are implemented but pending live verification against a running
daemon; container cpu/mem (`docker stats`), restart env-capture, and an MCP
wrapper are future work.

## License

[Apache-2.0](./LICENSE).
