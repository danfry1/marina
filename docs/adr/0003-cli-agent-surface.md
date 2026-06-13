# ADR 0003 — The resolution engine is exposed as a CLI/JSON surface

- **Status:** Accepted
- **Date:** 2026-06-13

## Context

The valuable, novel part of marina is the resolution engine: it maps
*arbitrary already-running* dev processes — however they were launched — back to
a project, ports, and pids. Existing tools don't fill this gap: port killers
(`lsof`/`fkill`) have no project sense, and supervisors (`pm2`, `foreman`,
`overmind`, `docker compose`) only know the project→process mapping for things
*they* launched.

Two adjacent wants surfaced: (a) acting on a whole *project* at once (a project
runs web + db + worker), and (b) letting agents query and control running
projects precisely ("stop client-portal" → it knows the exact ports/pids).

## Decision

Expose the engine as a **non-interactive CLI with JSON output**, separate from
the TUI:

```
marina ls [--json]      marina kill <selector>...
marina url <selector>   marina restart <selector>
```

A **selector** matches a target by project name (exact/substring), port
(`3000`/`:3000`), or command label. **Killing a project selector takes down every
target under it** — so the CLI's selector resolution *is* the project-grouping
primitive, shared by (a future) TUI group-kill.

Chosen over an MCP server for now: CLI+JSON is universal (any agent shells out),
zero-setup, and scriptable. An MCP wrapper can sit over it later without rework.

## Consequences

- The engine must be cleanly callable headless: `Sampler::new().build()` yields a
  full `Snapshot`; the CLI serializes/acts on it. (`ls` builds twice for a real
  CPU delta; mutating commands build once.)
- The JSON schema (`project`, `command`, `kind`, `ports`, `url`, `cpu_pct`,
  `mem_bytes`, `uptime_secs`, `pids`, `anchor_pid`, `cwd`, `branch`) is an agent
  contract — keep it stable; `cpu_pct`/`mem_bytes` are null when unmeasurable
  (docker).
- "Project" as a selector is now load-bearing in two surfaces; TUI grouping
  should reuse the same matching, not reinvent it.

## Alternatives considered

- **MCP server only.** Deferred: more machinery/setup; CLI+JSON already lets any
  agent drive it, and an MCP shim can wrap the CLI later.
- **TUI-only.** Rejected: leaves the engine's value locked behind a human;
  agents/scripts can't act on it.
