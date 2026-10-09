---
name: marina
description: >-
  Inspect and control the user's running local dev servers and processes via the
  `marina` CLI. Use when the user asks what's running locally, what's using a
  port, or to stop/restart/open a dev server or project — e.g. "what's running",
  "what's on :3000", "is the frontend up", "kill the API", "stop the
  client-portal project", "free up port 5432", "restart the worker". Requires the
  `marina` binary on PATH.
---

# Driving marina from an agent

`marina` is a developer-process cockpit: it resolves the user's *running* local
dev processes into recognizable names (project · tool · port · memory) and lets
you act on them. It has an interactive TUI (the bare `marina` command) — **never
run the bare `marina`; it blocks.** Use the subcommands below.

If `marina` isn't on PATH, tell the user to install it (`brew install
danfry1/tap/marina`, `cargo install marina-tui`, or `nix run github:danfry1/marina`)
— see https://github.com/danfry1/marina.

## 1. See what's running (always start here)

JSON is the stable contract — parse it, don't scrape the table.

```sh
marina ls --json
```

Each element:

| field | meaning |
|---|---|
| `project` | resolved project name (e.g. `client-portal`) — the main thing to match on |
| `command` | the tool (`next dev`, `vite`, `uvicorn`, `postgres`, …) |
| `kind` | `listener` (has a port) or `watched` (port-less, e.g. a file watcher) |
| `ports` | listening ports (array) |
| `url` | e.g. `http://localhost:3000`, a `postgres://…`, or `null` |
| `cpu_pct`, `mem_bytes` | resource use; `null` when not measurable (e.g. a docker container) |
| `exposed` | `true` = bound to 0.0.0.0/:: — reachable from the LAN (worth flagging to the user) |
| `container` | docker container name, or `null` for a native process |
| `launcher` | who started it, or `null`: `{kind, name, pid, alive, orphaned, session, cwd}` — `kind` is `agent` / `editor` / `terminal` / `detached`; `name` e.g. `claude`, `codex`, `opencode`, `cursor`, `copilot`, `tmux`; `session` is the agent's session/thread id; `orphaned: true` = its agent session has ended |
| `uptime_secs`, `pids`, `anchor_pid`, `cwd`, `branch` | process details |

An empty array means nothing dev-relevant is running. You can pre-filter with a
selector: `marina ls api --json`.

## 2. Act on it

A **selector** matches by project name (exact or substring), a port (`3000` or
`:3000`), or a command label.

```sh
marina kill <selector>      # SIGTERM, then verified SIGKILL (docker: docker stop)
marina restart <selector>   # kill the subtree, wait for the port to free, re-exec
                            # in the same cwd; output is captured to
                            # ~/.local/state/marina/logs/<project>.log
marina url <selector>       # print matching URLs (add --json for structure)
marina who <port>           # what's holding a port — a dev target, another
                            # process (named), or free (exit 1). --json available
marina free <port>          # stop the dev target on a port and wait until the
                            # port is released; already free = exit 0. Refuses
                            # (exit 1) if the holder isn't a dev target
marina port [name]          # a stable, free port for the current project /
                            # worktree — same every call, never shared with
                            # another worktree. stdout is just the number
marina port --list          # all assignments · `--release [name]` returns one
marina version              # version check (also proves the binary works)
```

**Scope flags** (on `ls` / `kill` / `restart` / `url`, with or without selectors):

```sh
marina ls --mine            # only servers started by *your* agent session
marina kill --mine          # stop exactly the servers you started — nothing else
marina ls --orphaned        # servers whose launching agent session has ended
marina kill --orphaned      # clean up after finished sessions
```

`--mine` matches by your session/thread id (Claude Code, Codex, opencode, Copilot,
Amp, …) or agent pid, and exits 2 if marina isn't running inside an agent session.

**Killing a project name stops every service under it.** If `client-portal` runs
a `next dev` on :3000 and a `postgres` on :5432, `marina kill client-portal`
stops both.

Exit codes — check them: `0` ok · `1` no match · `2` usage error.

## Handling common requests

- **"what's running"** → `marina ls --json`, then summarize.
- **"what's on :3000" / EADDRINUSE** → `marina who 3000` (`--json` for structure;
  `status` is `target`, `other`, or `free`).
- **"stop/kill the X project"** → `marina ls --json` to find the exact `project`
  value, then `marina kill <project>`. Prefer the exact name to avoid
  over-matching (a substring like `api` could match several).
- **Starting a dev server** (especially in a git worktree, or when other agents
  may be running the same project) → don't hard-code `:3000`; ask marina:
  `pnpm dev --port $(marina port)` / `PORT=$(marina port) npm run dev`. Use a
  name per service when starting several: `$(marina port web)`,
  `$(marina port api)`. It's stable, so repeat calls (and restarts) get the same
  port. When you're done with the worktree, `marina port --release`.
- **"free up port 5432"** → `marina free 5432`. If it reports a holder that
  isn't a dev target, tell the user what it is rather than killing it yourself.
- **"clean up" / end of a task where you started servers** → `marina kill --mine`.
  It only touches servers launched from your own session — never the user's own
  servers or another agent's. Don't kill by port/name to clean up after yourself.
- **"what's this server / who started it?"** → `marina who <port>` or the
  `launcher` field. Servers with `launcher.name == "claude"` but a different
  `session` belong to another Claude Code session — leave them alone unless asked.
- **"kill leftover/zombie dev servers"** → `marina ls --orphaned` first, show the
  user, then `marina kill --orphaned`.
- **"restart the api"** → `marina restart api` (if ambiguous, list first and confirm).
- **"open / give me the URL for the frontend"** → `marina url <project>`.

## Good to know

- If the user runs Claude Code, `marina hooks install` wires this in
  automatically: running servers at session start, duplicate/port notes before
  dev-server commands, and cleanup of your servers when the session ends.

- marina only sees and touches the **current user's own** processes, and never
  lists or kills the shell/session it (or you) run in — so you can't accidentally
  kill your own terminal.
- It makes no network calls. The only file it writes is the per-project restart
  log (`~/.local/state/marina/logs/<project>.log`) — read that when the user asks
  why a restarted server is misbehaving.
- Unknown subcommands/flags exit 2 immediately (they never fall through to the
  blocking TUI), so it's safe to script against.
- Resolution is heuristic; if a `project`/`command` looks wrong, fall back to the
  `port` selector, which is exact.
