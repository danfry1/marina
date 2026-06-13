# ADR 0002 — Anchor selection stops at the package boundary

- **Status:** Accepted
- **Date:** 2026-06-13

## Context

`netstat2` reports the PID that *holds* a listening socket. For most dev servers
that PID is a forked **worker child** whose argv is uninformative (`node`), while
the recognizable command (`next dev`) and the project `cwd` live on an
**ancestor**. So the socket-holding PID is the wrong thing to resolve from and
often the wrong thing to kill.

The naive fix — "climb to the top-most same-user ancestor" — overshoots:

- It attaches to the shell, `tmux`, `ssh`, or `login` that happened to launch the
  server.
- In a monorepo, `turbo run dev` spawns `next dev` (:3000) **and** an api (:4000)
  under one parent. Climbing both listeners to `turbo` makes two Targets claim the
  same root → CPU/mem double-counted, and `kill` on one would take out the other.

## Decision

**The climb stops at the package boundary.**

1. From the listener PID, read `cwd`; find the nearest **project marker** upward
   (`package.json`, `Cargo.toml`, `go.mod`, `pyproject.toml`, `Gemfile`, `pom.xml`,
   `mix.exs`, … or `.git`) → **project root** `R`. Language-agnostic.
2. Walk *up* the parent chain while each ancestor's `cwd` stays inside `R`, and it
   is not a boundary process (shell, pid 1 / launchd, `tmux`, `ssh`, `login`). The
   top-most ancestor still inside `R` is the **workload root** = the **anchor**.
3. The Target's process set = anchor + all of its descendants that are also inside
   `R`. CPU/mem roll up across exactly that set.

**Invariant: a process belongs to at most one Target.**

`kill`/`restart` operate on the anchor's in-boundary subtree — never the
out-of-boundary parent.

## Consequences

- Monorepos resolve correctly: each app stops at its own package; the shared
  supervisor (`turbo`) is outside every boundary, so it anchors none and is never
  double-counted. (It can still appear as its own Watched target if a watch rule
  matches; otherwise it's ignored.)
- Detached / double-forked servers (reparented to launchd) can't climb → the
  listener PID anchors itself. Label may be generic; the `[[override]]` config is
  the escape hatch.
- Anchor selection and resolution are mildly circular (anchor needs `R`, label
  needs the chain). Resolved by computing `R` from the listener cwd first, then
  climbing, then picking the label from the best argv match within the chain.

## Alternatives considered

- **Anchor = socket-holding PID.** Rejected: uninformative argv, and `kill` leaves
  the parent (which may re-spawn the worker or hold the port).
- **Climb to top-most same-user ancestor.** Rejected: attaches to shell/tmux and
  double-counts monorepo siblings under a shared supervisor.
