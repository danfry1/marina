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
  navigate, idles near-zero CPU when nothing changes.
- **Smart resolution** — infers project + tool from cwd, argv, package manifests
  (package.json / Cargo.toml / pyproject / go.mod / …) and git; sees through
  `pnpm`/`npm`/`yarn`/`node` wrappers (`pnpm dev` → `vite`).
- **First-class verbs** — kill (SIGTERM→SIGKILL escalation with undo), restart,
  tail logs inline, copy URL, open in browser.
- **Grouping** — a project's services collapse under one header, and one keystroke
  kills the whole project. Declared groups bundle an app with its database.
- **Agent/script CLI** — `marina ls --json`, `marina kill <project>`, sharing the
  exact same resolution engine as the TUI.

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
| `j` / `k`, `g` / `G` | move / jump to top·bottom |
| `Enter` | fold / unfold a project group |
| `/` | filter (project / command / port) |
| `s` | cycle sort (port / cpu / mem) |
| `K` · `u` | kill selection · undo |
| `R` · `T` | restart · tail logs |
| `Y` · `O` | copy URL · open in browser |
| `q` | quit |

`K`/`R` on a group header act on the whole project.

**CLI** (for scripts and agents)

```sh
marina ls [--json]            # the snapshot — table, or a stable JSON contract
marina kill <selector>        # project name, port (3000 / :3000), or command
marina restart <selector>
marina url <selector>
```

A selector matching a project name acts on **every** service under it. Exit
codes: `0` ok, `1` no match, `2` usage error.

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

Optional `~/.config/marina/config.toml`:

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
```

## Privacy & security

marina is deliberately boring on this front:

- **Local only — no network, ever.** There is no HTTP/TLS or networking client in
  the dependency tree, and no code that opens a connection. No telemetry, no
  phone-home, no data leaves your machine. (`netstat2`/`libproc` *read* the OS's
  socket and process tables locally; `mio` polls the terminal for keystrokes.)
- **Nothing is persisted.** marina reads, displays, and forgets. The only optional
  file it *reads* is `~/.config/marina/config.toml`; it writes nothing.
- **Your processes, your permissions.** It runs unprivileged (no `sudo`) and only
  ever inspects your own user's processes — system/root daemons and anything
  outside `$HOME` are filtered out (and the OS wouldn't let it read others
  anyway). It also never lists the session it runs in (your shell / terminal /
  `ssh`), so you can't accidentally kill your own connection. It's the same class
  of introspection `ps`, `lsof`, and your IDE already do.
- **What it reads:** a process's `cwd`, argv, cpu/memory; the nearest project
  manifest's `name` (package.json / Cargo.toml / …); `.git/HEAD` for the branch;
  and, only when you press `T` to tail logs, its open file descriptors (via
  `lsof`) and the discovered log file.
- **Secrets stay internal.** Command-line args can contain tokens/passwords, so
  marina **never displays, serializes, or logs raw argv** — the UI and
  `ls --json` show only derived labels (`next dev`, `vite`). argv is used for
  classification and captured for `restart`, and never leaves the process.
- **Outbound actions are only the ones you trigger:** `O` opens a URL in your
  browser, `Y` copies to the clipboard, `K`/`R` send signals to *your* processes.

## Design

See [DESIGN.md](./DESIGN.md) and the glossary in [CONTEXT.md](./CONTEXT.md).

## Status

macOS + Linux · ~4k LOC · 55 tests (CI builds + tests on both). Docker container
naming is implemented but pending live verification against a running daemon;
restart env-capture and an MCP wrapper are future work.

## License

[Apache-2.0](./LICENSE).
