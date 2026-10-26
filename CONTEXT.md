# CONTEXT.md — glossary

> Canonical terms for marina. This file is a **glossary, not a spec** —
> it defines what words mean, not how the system behaves. Behaviour lives in
> `DESIGN.md`.

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
  verbs act on. Carries the fingerprint.

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
  key, scroll offset, active sort, open log/inspect pane, modal mode, pending
  kill, row-flash timers.

- **Exposed** — a listener bound to a non-loopback address (`0.0.0.0` / `::`),
  i.e. reachable from the LAN. Rendered as a red `:3000!` badge; `exposed` in
  the JSON.

- **Ignore rule** — a config `[[ignore]]` entry (by port and/or command regex)
  that hides a target the curation heuristics keep surfacing. The complement of
  a watch rule.

- **Dying row** — a target `X` sent SIGTERM to that hasn't exited yet,
  rendered red as `stopping…`; `X` on it again sends SIGKILL.

- **Held row** — the placeholder (`stopped`, `restarting…`) that keeps a row
  marina just stopped or restarted under the cursor after it leaves the
  snapshot, so a second keypress can't hit its neighbour. Moving lets it go.

- **Inspect panel** — the `i` pane showing everything about the selection that
  doesn't fit a row: program + arg count (never raw argv), all ports, url, cwd,
  branch, launcher, pid count, container. Follows the cursor.

- **Launcher** — who started a Target: an *agent* session (`claude`, `codex`,
  …), an *editor*, a *terminal*, or nobody any more (*detached*). Shown in the
  `VIA` column; the basis of the `--mine` / `--ended` scopes.

- **Port lease** — a `marina port` assignment: (project root, optional service
  name) → port, stable across calls and exclusive across worktrees. Stored in
  `ports.json`; dropped when its root is deleted or on `--release`.

- **Session view** — the `v` grouping mode: group headers are launchers (agent
  sessions, editors, terminals) instead of projects. Also lists the agent
  sessions themselves.

- **Agent session** — a running coding-agent process a person (or another
  agent) is talking to: `claude`, `opencode`, `codex`, … — not its plumbing
  (daemons, pty hosts, `app-server`, `serve`). *Not a Target* (ADR 0004): no
  verb, selector or scope reaches it. Its **shells** are the direct children
  running a command now; its **helpers** are the long-lived ones (MCP servers).
  Listed in session view and by `marina agents`. Its plumbing that holds a
  port (`opencode serve`) is still a Target — an **agent service** — but the
  project view shows it beside the agents section, not as a project.

- **Host** — the multiplexer or terminal app an agent session's terminal runs
  in, when marina can drive it (herdr, tmux; iTerm2, Terminal, Ghostty,
  WezTerm). Jump target, and for herdr a source of the session's status
  (including *blocked*) and task title.

- **Summary** — what a Claude Code session last said, from the tail of its
  transcript: title, last prompt, last reply, recap, pending tool, PR link.
  Shown in the preview, the inbox and `marina agents --long/--json`.

- **Inbox** — the view of all agent sessions grouped by what each needs
  from you: *needs you* (an approval or question is up), *your turn*
  (finished and asking something, or unread), *working*, *parked*
  (finished, nothing asked, or set aside), and *ended*.

- **Subagent** — an agent a session started inside its own process
  (Claude Code's Agent tool, a Codex spawned thread). Shown under its
  session, never as a session; while one runs, its session is working.

- **Report** — what an agent said of its own state through marina's hooks
  (working, blocked, idle). Trusted over inference until the transcript
  records something newer.

- **Set aside** — a session you've parked with `d` until it does something
  new.

- **Heavy** — a Target using 90%+ CPU or 2GB+ memory: flagged as a likely
  runaway.

- **Unread** — a session whose transcript has news since you last viewed
  it in marina.

- **Looks finished** — an agent session that is stale, has no live servers,
  and whose work tree is clean and pushed: the safest one to close.

- **Orphaned** — a Target whose launching agent session has ended while the
  server kept running (`claude·ended`; the CLI's `--ended`, JSON
  `launcher.ended`). Distinct from *detached*, which carries no agent markers
  at all.
