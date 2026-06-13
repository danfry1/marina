# marina

A **developer-process cockpit**. A TUI you leave open in a pane all day that
shows the dev servers and processes you actually care about — resolved into names
you recognize:

```
client-portal · next dev · :3000 · 340MB
```

Not a system monitor (btop/bottom own that). Not an `lsof` wrapper. Ruthlessly
developer-process-centric.

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

```sh
cargo build --release
./target/release/marina
```

macOS-first. Linux source adapters sit behind trait boundaries but aren't
implemented yet.

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

## Design

See [DESIGN.md](./DESIGN.md), the glossary in [CONTEXT.md](./CONTEXT.md), and the
decision records under [docs/adr/](./docs/adr/).

## Status

macOS-first · ~4k LOC · 47 tests. Docker container naming is implemented but
pending live verification against a running daemon; restart env-capture and an
MCP wrapper are future work.

## License

[Apache-2.0](./LICENSE).
