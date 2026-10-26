# marina

A **developer-process cockpit**. A TUI you leave open in a pane all day that
shows the dev servers and processes you actually care about — resolved into names
you recognize:

```
client-portal · next dev · :3000 · 340MB
```

Not a system monitor (btop/bottom own that). Not an `lsof` wrapper. Ruthlessly
developer-process-centric.

![marina in action](https://raw.githubusercontent.com/danfry1/marina/main/demo/marina.gif)

> A marina is a harbour full of berthed boats — a cockpit of dev services each
> occupying a port. `marina` enumerates what's listening, resolves each to a
> project / tool / URL, groups them, and lets you act on them with one keystroke.

## Features

- **Live, stable TUI** — grouped by project; no flicker, no cursor-jump while you
  navigate, and it only redraws when something changed — an idle cockpit does
  near-zero work. Mouse works too (click to select, click the selected row
  again for what Enter does, click a header to sort, wheel to move / scroll
  logs). Mouse capture means a plain drag doesn't select text: hold Shift
  (Option in iTerm2) to select.
- **Smart resolution** — infers project + tool from cwd, argv, package manifests
  (package.json / Cargo.toml / pyproject / go.mod / …) and git (worktrees and
  detached HEADs included); sees through `pnpm`/`npm`/`yarn`/`node`/corepack
  wrappers to the real tool (`pnpm dev` → `vite`, `pnpm exec vp dev` → `vp dev`,
  `python -m http.server` → `http.server`).
- **First-class verbs** — kill (`X`: a fingerprint-verified SIGTERM; a server
  still running after 4s is reported, and `X` again force-kills it — nothing
  escalates on its own), restart (waits for the port to free, captures the new
  process's output to a log), tail logs inline, copy URL, open in browser. On
  a group header, `X` and `R` act on the servers the filter shows, after a
  confirm. Docker rows stop/restart/tail via the docker CLI.
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
  `marina kill --ended` cleans up after finished sessions; an agent can run
  `marina kill --mine` to stop exactly the servers it started.
- **Honest signals** — rows flash green when they appear, turn red while dying,
  and if a long-running server vanishes *without* you killing it, marina says so
  (`⚠ client-portal (:3000) exited unexpectedly`). A `:3000!` port badge warns
  when a server is bound to `0.0.0.0` (reachable from your LAN).
- **Grouping** — a project's services collapse under one header, and one keystroke
  kills the whole project. Declared groups bundle an app with its database.
- **Session view** — press `v` to group by *who started it* instead: one header
  per agent session (`claude·fal-app`, `codex·api`), editor, or terminal, across
  projects. `X` on a session header stops everything that session started.
- **Agent sessions** — every running coding-agent session (Claude Code,
  opencode, Codex, …) is listed, even one with no servers: a folded
  `▸ agents (21)  3 working · 13 stale · 14GB` section at the bottom of the
  project view, and a row per session in the session view. Each shows what it
  is doing (`$ cargo test`, `working`, `idle 12m`), its cpu/mem (agent + MCP
  helpers) and branch, and its detail line the session id and where it runs;
  an agent's background server
  (`opencode serve`) is shown just above that section, always visible, as
  `opencode server`, rather than as a project among the dev servers; `i` lists its shells and helpers and
  the resume command. Working vs idle comes from when its terminal was last
  written to, like `w`'s IDLE column. Sessions whose terminal has had no input
  for over a day are dimmed as *stale* — usually a forgotten session holding
  memory. Background Claude Code
  sessions are included; daemons and pty hosts are not. marina never stops an
  agent: `X` on an agent row explains why, and `X` on its header stops only
  its servers.
- **Jump to a session** — an agent row's detail line says where it lives
  (`Enter jumps (herdr·ghostty)`, `tmux·iterm`, `ghostty`; `Enter copies
  resume` for no terminal), and `Enter` focuses it there — switching the multiplexer's pane and raising
  the terminal app showing it:
  a herdr or tmux pane, or the iTerm2 session / Terminal tab / WezTerm pane on
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
- **What each agent changed** — the BRANCH column leads with the work tree's
  state (`●4 ↑2 main`: 4 files uncommitted, 2 commits unpushed; `✓` clean),
  and `d` opens the uncommitted diff (stat, untracked files, patch) in the
  pane below. A stale session with no servers and a clean, pushed tree is
  marked *looks finished* — the one to close first.
- **Orphan notices** — when an agent session ends and leaves servers running,
  marina says so once — `⚠ claude session in ~/dev/app ended — left 2 servers
  running (480MB)` — in the status line and as a desktop notification
  (`osascript` / `notify-send`; turn off with `[notify] desktop = false`).
- **Where an agent's server came up** — when an agent session starts a web
  server, the status line says so (`claude·api started vite on
  http://localhost:5173 — o on its session opens it`), once per server.
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

Runs on **macOS and Linux**. On Linux, `o` (open) uses `xdg-open` and `y` (copy)
uses `wl-copy`/`xclip`; accurate `phys_footprint` memory is macOS-only (Linux
falls back to RSS).

## Run it

However you installed it, the command is **`marina`** (on your `PATH`):

```sh
marina        # launch the TUI — leave it open in a pane while you work
marina ls     # or a one-shot list (add --json for scripts/agents)
marina demo   # the TUI on a made-up machine: try the keys, nothing real is touched
```

No flags or config needed — it auto-discovers your running dev servers. Press
`?` inside the TUI for the full key list.

## Usage

**TUI** — `marina`

| key | action |
|---|---|
| `j` `k` / `↑` `↓` | move (stops at the ends; mouse: click / wheel) |
| `g` `G` / `Home` `End` | top · bottom |
| `Ctrl-d` `Ctrl-u`, `PgDn` `PgUp` | half a page · a page (with a pane open, `PgDn`/`PgUp` page it) |
| `h` `l` / `←` `→` | fold · unfold a header; `h` on a row under one goes up to it |
| `Enter` | fold a header · inspect a server · jump to an agent session's pane |
| `i` | inspect the selection (command, ports, cwd, launcher, pids) |
| `t` · `d` | the server's log · the uncommitted diff of its (or an agent's) work tree, in a pane; again closes it |
| `[` `]`, `+` `-` | scroll · resize the pane |
| `y` · `o` | copy URL · open in browser (on a session or group: its first web server; again: the next) |
| `X` | kill: SIGTERM; again on a server still stopping: SIGKILL |
| `R` | restart (output captured to a log) |
| `/` | filter (project / command / port / cwd / branch / launcher — `/claude`, `/ended`); Esc while typing restores the previous filter |
| `s` · `S` | cycle sort (port / cpu / mem) · reverse it — or click a column header (again: reverse) |
| `v` | group by project ↔ by who started them (lists agent sessions) |
| `?` | all the keys |
| `Esc` | close the pane, then inspect, then clear the filter |
| `q` · `Ctrl+C` | close the open pane, else quit · quit |

Capital letters touch processes (`X`, `R`); everything else is lowercase,
with vim's `G` as the one exception. `X`/`R` on a group header act on the
servers it shows (in session view, that session's; with a filter, only the
ones it lets through), and ask first when that's more than one. Docker rows
map to `docker stop` / `docker restart` / `docker logs -f`.

**CLI** (for scripts and agents)

```sh
marina ls [sel…] [--json]     # the snapshot — table, or a stable JSON contract
marina ls --mine              # only servers started by the agent session running this, in this worktree
marina kill --session         # the same session's servers in every worktree (subagents included)
marina kill --ended           # stop servers whose agent session has ended
marina kill <selector>        # project name, port (3000 / :3000), or command
marina kill api --grace 10    # SIGTERM, then SIGKILL after 10s (default 5)
marina restart <selector>     # output captured to ~/.local/state/marina/logs/
marina url [selector…] [--json] # bare: the servers running in this directory
marina open [selector…]       # open in the browser — bare: the servers running in this directory
marina who <port>… [--json]   # what's holding a port — dev target or not
marina free <port>… [--json]  # stop the dev target on a port, wait until it's released
marina port [name] [--json]   # a stable, collision-free port for this project/worktree
marina port --list            # every assigned port · --release [name] gives one back
marina agents [sel…] [--json] # agent sessions: shells, helpers, servers, resume command
marina demo                   # the TUI on made-up servers and sessions
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

Agents only use `marina port` when they're told to; left alone they start on
the framework's default. Put this in the project's `CLAUDE.md` or `AGENTS.md`:

```md
## Dev servers
- Run servers on `$(marina port)`, not a fixed port.
  One per service: `marina port <name>`.
- Your server's URL: `marina url --mine`.
- When done, stop yours: `marina kill --mine`.
- Another agent in this worktree? Name your port
  (`marina port <task>`) and kill by port.
```

`--mine` matches servers started by the agent session running the command (by
the session id the agent exports, else by process ancestry) **in the git
worktree it's run from**, so one agent's `kill --mine` leaves every other
session's servers alone. The worktree limit is for subagents: they share their
parent's session id, so without it a subagent in `.claude/worktrees/a` would
stop its siblings' servers too. `--session` drops the limit, for an
orchestrator cleaning up after all its subagents at once. Outside an agent
session both exit `2` rather than matching everything, and so does `--mine`
outside a git worktree, where it couldn't tell an agent from its subagents.

Subagents sharing one worktree can't be told apart by marina at all, so the
last line of the snippet has each take a named port and stop by port.

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

A selector is a port (`3000`, `:3000` — ports only) or a name: an exact
project name, else an exact command, else part of either. A project name acts
on **every** server under it. `kill`, `restart` and `open` refuse a name that
reaches more than one project without naming one (`api` matching `api-gateway`
and `rapid-ui`), listing the candidates; `--all` goes ahead. `open` lists
rather than opens more than three servers unless `--all` is given.

`kill --json` adds a `result` to each server: `terminated`, `force_killed`,
`still_running` or `denied`. `--ended` was formerly `--orphaned` (still
accepted), and the JSON's `launcher.ended` was `launcher.orphaned`.

Exit codes: `0` ok · `1` a selector matched nothing, or a server didn't stop ·
`2` usage error or an ambiguous selector · `3` the port scan failed, so
nothing is reported rather than an empty list. `kill --mine` / `kill --ended`
with nothing in scope is success. `-h` / `--help` works after any command, and
unknown commands/flags are errors — they never fall through to the TUI.

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

[ports]
range = [3100, 3999]           # where `marina port` assigns from
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
  to `logs/<project>.log` so `t` can always tail it; and `marina port` records
  its assignments (port, project root, optional service name, timestamps) in
  `ports.json`.
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
  are kept; flag values are never read, and no other variable is kept,
  displayed, or serialized; for agent sessions, their terminal device's
  last-written / last-read timestamps (never the screen contents), their work
  tree's `git status` / `git diff --shortstat` (run with
  `GIT_OPTIONAL_LOCKS=0`, so marina never takes a lock an agent needs), and —
  only when herdr or tmux is running — those tools' own pane listings
  (`herdr agent list`, `tmux list-panes`); the nearest project
  manifest's `name` (package.json / Cargo.toml / …); `.git/HEAD` for the branch;
  and, only when you press `t` to tail logs, its open file descriptors (via
  `lsof`) and the discovered log file.
- **Secrets stay internal.** Command-line args can contain tokens/passwords, so
  marina **never displays, serializes, or logs raw argv** — the UI and
  `ls --json` show only derived labels (`next dev`, `vite`); even the inspect
  panel shows just the program path and an arg count. argv is used for
  classification and captured for `restart`, and never leaves the process.
- **Outbound actions are only the ones you trigger:** `o` opens a URL in your
  browser, `y` copies to the clipboard, `X`/`R` send signals to *your* processes,
  `Enter` on an agent asks its multiplexer or terminal app to focus it. The one unprompted
  action is a local desktop notification when an agent session leaves servers
  behind (`[notify] desktop = false` disables it).

## Design

See [DESIGN.md](./DESIGN.md) and the glossary in [CONTEXT.md](./CONTEXT.md).

## Status

macOS + Linux · ~14k LOC · 178 tests (CI builds + tests on both). Docker container
naming + verbs are implemented but pending live verification against a running
daemon; container cpu/mem (`docker stats`), restart env-capture, and an MCP
wrapper are future work.

## License

[Apache-2.0](./LICENSE).
