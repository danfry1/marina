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
| `uptime_secs`, `pids`, `anchor_pid`, `cwd`, `branch` | process details |

An empty array means nothing dev-relevant is running.

## 2. Act on it

A **selector** matches by project name (exact or substring), a port (`3000` or
`:3000`), or a command label.

```sh
marina kill <selector>      # SIGTERM, then SIGKILL after a 4s grace period
marina restart <selector>   # kill the subtree, wait, re-exec in the same cwd
marina url <selector>       # print matching URLs (one per line)
```

**Killing a project name stops every service under it.** If `client-portal` runs
a `next dev` on :3000 and a `postgres` on :5432, `marina kill client-portal`
stops both.

Exit codes — check them: `0` ok · `1` no match · `2` usage error.

## Handling common requests

- **"what's running" / "what's on :3000"** → `marina ls --json`, then summarize.
- **"stop/kill the X project"** → `marina ls --json` to find the exact `project`
  value, then `marina kill <project>`. Prefer the exact name to avoid
  over-matching (a substring like `api` could match several).
- **"free up port 5432"** → `marina kill 5432`.
- **"restart the api"** → `marina restart api` (if ambiguous, list first and confirm).
- **"open / give me the URL for the frontend"** → `marina url <project>`.

## Good to know

- marina only sees and touches the **current user's own** processes, and never
  lists or kills the shell/session it (or you) run in — so you can't accidentally
  kill your own terminal.
- It makes no network calls and persists nothing.
- Resolution is heuristic; if a `project`/`command` looks wrong, fall back to the
  `port` selector, which is exact.
