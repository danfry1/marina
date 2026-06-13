# ADR 0001 — The cockpit unit is a `Target`, and a port is optional

- **Status:** Accepted
- **Date:** 2026-06-13

## Context

The product brief is "the dev servers **and processes** I care about." Two
forces pull against each other:

1. A **port-anchored** model gives the cleanest, most stable identity (a row is
   "the thing on :3000", survives child-PID churn) — but it excludes port-less
   dev workloads the user explicitly cares about: `tsc --watch`, `jest --watch`,
   watch-mode bundlers.
2. Showing **every process** would cover those, but turns the tool into btop —
   the exact thing the brief says it must not be ("not a general system monitor").

Separately, the working term **"service"** collides with launchd/systemd
services and with "microservice", importing the wrong mental model.

## Decision

The unit of the cockpit is a **`Target`** (not "service").

A port is a strong attribute and the **primary identity when present**, but is
**not required**. Targets come from two discovery sources:

- **Listeners** — anything listening on a TCP port. Always shown. Identity is the
  port.
- **Watched** — port-less processes whose command matches a **watch rule**
  (default rules for common watchers; user-extensible via config). Shown only as
  standalone targets.

**Subtree absorption** keeps the view focused: a port-less process that is a
descendant of a listener target is folded into that target's subtree (and CPU/mem
rollup), not shown as its own row. A port-less process becomes its own Target only
if it is standalone **and** matches a watch rule.

`TargetKey` is therefore an enum:

- `Port(u16)` — listener targets (with the anchor-fingerprint reuse guard).
- `Command { project, label, cwd }` — watched targets.

## Consequences

- The identity key is heterogeneous; reconciliation and selection-follow logic
  must handle both variants.
- Focus is preserved by curation (watch rules + subtree absorption) rather than
  by a port requirement.
- The resolution layer must handle the no-port case (project/label still resolve
  from cwd/argv/package.json/git; URL may be absent).
- We ship sensible default watch rules so the feature works out of the box.

## Alternatives considered

- **Port-strict** (identity *requires* a port). Rejected: silently drops the
  port-less watchers the brief explicitly includes.
- **Show all processes.** Rejected: becomes a general system monitor.
