# CONTEXT.md — glossary

> Canonical terms for marina. This file is a **glossary, not a spec** —
> it defines what words mean, not how the system behaves. Behaviour lives in
> `DESIGN.md` and `docs/adr/`.

- **Target** — a developer workload surfaced as one row in the cockpit and acted
  on by verbs; the unit of selection. May be backed by a whole process subtree.
  _Supersedes the earlier word "service" (rejected: collides with launchd/systemd
  services and "microservice")._

- **Listener target** — a Target whose identity is a listening TCP port. The
  primary, always-shown discovery source.

- **Watched target** — a **port-less** Target included only because its command
  matches a watch rule (e.g. `tsc --watch`, `jest --watch`). Standalone watchers,
  not children of a listener.

- **Subtree absorption** — a port-less process that is a descendant of a listener
  target is folded into that target's subtree (and its CPU/mem rollup), **not**
  shown as its own row. This is what keeps the cockpit focused rather than a full
  process list.

- **TargetKey** — the stable identity used to reconcile a Target across snapshots
  and to follow the cursor selection. `Port(u16)` for listener targets;
  `Command { project, label, cwd }` for watched targets.

- **Project root** (`R`) — the nearest **project marker** directory found by
  walking up from a process's `cwd`. A project marker is any recognized manifest
  (`package.json`, `Cargo.toml`, `go.mod`, `pyproject.toml`, `Gemfile`, `pom.xml`,
  `mix.exs`, …) or `.git` as the universal fallback. Language-agnostic. Defines
  the boundary a Target may span.

- **Workload root / Anchor** — the top-most ancestor of the listener PID still
  inside the project root `R` (not crossing a shell / pid 1 / `tmux` / `ssh`).
  The representative process of a Target: what resolution reads from and what
  verbs act on. Carries the fingerprint. See [ADR 0002](./docs/adr/0002-anchor-and-subtree-boundary.md).

- **Anchor fingerprint** — `(pid, start_time)` (plus resolved project) used to
  detect when a port or command has been handed to a genuinely *different*
  process, which triggers an identity reset (cached resolution + CPU EWMA cleared).

- **Subtree rollup** — aggregation of CPU% and memory across a Target's whole
  process subtree into the Target's displayed figures.

- **Snapshot** — an immutable, point-in-time set of Targets produced by the
  sampler and consumed by the UI. Swapped atomically.

- **Sampler** — the background thread that builds Snapshots; owns all blocking
  I/O. The UI thread never blocks.

- **Resolution** — turning raw process data (cwd, argv, package.json, git) into
  human fields: project name, command label, URL.

- **Verb** — a one-keystroke action on the selected Target: kill, restart, tail
  logs, copy URL, open in browser.

- **Selector** — a CLI argument that resolves to one or more Targets: a project
  name (exact/substring), a port (`3000`/`:3000`), or a command label. Killing a
  project selector stops every Target under it — the grouping primitive shared by
  the CLI and TUI group-kill.

- **Group** — a set of Targets sharing a project name, rendered together with a
  collapsible header and group-level verbs. **Auto-groups** form from a shared
  project root (an app + its watchers). **Declared groups** (`config [[group]]`)
  override a Target's project to bundle pieces that don't share a cwd — an app
  and its database — selected by port / project / command.

- **View state** — UI-thread-only state that survives snapshot swaps: selection
  key, scroll offset, active sort, open log pane, modal mode.
