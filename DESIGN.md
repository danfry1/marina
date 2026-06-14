# marina — design

> A developer-process cockpit. A TUI you leave open in a pane all day that shows
> the dev servers and processes you actually care about, resolved into names you
> recognize: `client-portal · next dev · :3000 · 340MB`.
>
> Not a system monitor (btop/bottom own that). Not an lsof wrapper. Ruthlessly
> developer-process-centric.

_Name: **marina** — a harbour full of berthed boats, i.e. a cockpit of dev
services each occupying a port. Binary + crate: `marina`._
_Terminology is defined in [CONTEXT.md](./CONTEXT.md)._

## Goals, in priority order

1. **Live data, stable feel.** Live-updating without flicker, without the list
   jumping under the cursor during navigation, without pegging a core. **This is
   the most important property.**
2. **Smart resolution.** Raw PID → human: project from cwd / command /
   package.json / git, so a row reads `client-portal · next dev · :3000 · 340MB`.
3. **First-class verbs.** One keystroke each on the selected row: kill, restart,
   tail logs inline, copy URL, open in browser. Vim-modal navigation.

macOS-first, cross-platform-ish (Linux is a later target, behind trait boundaries).

---

## Locked decisions

| Decision | Choice |
|---|---|
| **Unit (`Target`)** | A row is a **Target** — a dev workload acted on by verbs. Port-centric where a port exists; aggregates a process subtree; CPU/mem rolled up. Survives child-PID churn. |
| **Port optionality** | A port is the primary identity *when present*, not required. Port-less watchers are included via watch rules; port-less descendants of a listener are absorbed into its subtree. |
| **Stack** | **Rust + Ratatui + crossterm.** Single static binary; immediate-mode diff rendering handles flicker. |
| **Concurrency** | **std threads + channels, no async/tokio.** Sampler thread → `Arc<Snapshot>` → UI thread. |
| **Log tailing** | **Best-effort, scoped.** fd-sniff for a `.log` file; read supervisor logs (pm2/foreman/docker) when detected; full capture only for targets we launch/restart. Honest about gaps. |

## Default calls (override freely)

**1. `TargetKey` identity → enum, port-primary with a reuse guard.**
- `Port(u16)` for **listener targets** — most stable identity; survives child-PID
  churn. Reuse guard: each target carries an **anchor fingerprint**
  `(anchor_pid, start_time)` + resolved project; if a port matches across a swap
  but the fingerprint changed, it's a **new** target (cached resolution + CPU EWMA
  reset, not carried). Multi-port: anchor on the lowest port, list the rest.
- `Command { project, label, cwd }` for **watched targets** — stable across the
  watcher's own restarts (new PID, same identity).

**2. Resolution config → TOML at `~/.config/marina/config.toml`.**
Read by `resolve` (command classifier + URL builder) and `verbs` (URL to open/copy).

```toml
# Classify a command into a friendly label + default URL scheme.
[[rule]]
match_cmd = "next( |$)|next dev"   # regex against argv-joined command
label     = "next dev"
url       = "http://localhost:{port}"

[[rule]]
match_cmd = "vite"
label     = "vite"
url       = "http://localhost:{port}"

# Port-less workloads to surface as their own (watched) targets.
[[watch]]
match_cmd = "tsc.*--watch|jest|vitest|tailwindcss.*-w"
label     = "watcher"

# Manual override: pin a stubborn target to a name regardless of heuristics.
[[override]]
match_port = 3000
match_cmd  = "node"
project    = "client-portal"

# Declared group: bundle targets that don't share a cwd (an app + its db).
[[group]]
name    = "client-portal"
members = [3000, 5432, "worker"]   # ports, project names, or command labels
```

---

## Discovery & curation (what becomes a row)

```
all listening TCP sockets ──► Listener targets        (always shown; key = Port)
all processes ──┬─ descendant of a listener? ──► absorbed into that target's subtree
                └─ standalone + matches [[watch]]? ──► Watched target (key = Command)
                   (otherwise: ignored — this is what keeps it from being btop)
```

The "ignored" branch is the whole point: focus by curation, not by enumeration.

---

## Architecture

```
[sampler thread]                          [UI thread / main]
  loop:                                     loop:
    netstat2 → {port → pid}                   event::poll(≈100ms)
    sysinfo refresh → {pid → cpu/mem/cwd/argv/parent}
    discover targets (listeners + watched)    ├─ key event → mutate view state
    roll subtree into each Target             │     (select / scroll / verb)
    resolve (cached; only new targets)        └─ snapshot msg → swap Arc<Snapshot>
    build immutable Snapshot                  draw(frame) from (snapshot + view state)
    send Arc<Snapshot> ───────────────────►
```

- **UI thread owns no blocking I/O.** It selects between input events and snapshot
  messages, and redraws from `(latest snapshot + view state)`.
- **Snapshots are immutable and swapped atomically** (`Arc<Snapshot>` over a
  channel, or `ArcSwap`). No half-updated frames → no flicker from partial data.
- **View state lives only on the UI thread** and survives swaps: selection key,
  scroll offset, active sort, open log pane, modal mode.

### Adaptive cadence — IMPLEMENTED

The sampler runs a single build path on an **adaptive interval**: **1s** while the
topology (set of target keys) is changing, **doubling toward a 5s cap** once it's
stable. Start or stop a server and it snaps back to 1s; otherwise a cockpit left
open all day idles near zero. With resolution cached (below) the per-tick cost is
just one `sysinfo` refresh + one `netstat2` enumeration, both cheap — so a true
two-cadence split (separate fast resources / slow topology) wasn't needed; it
remains an option if profiling ever shows it. Terminal-focus-based backoff is a
possible future refinement (crossterm focus events).

### Stability levers (priority-1 detail)

1. **Flicker** — Ratatui double-buffered diff render. Never clear-and-reprint.
2. **No jump under cursor** —
   - Selection is a held `TargetKey`, never an array index. Reconcile by key on
     each swap; if the selected target vanished, fall to nearest neighbor.
   - Default sort is a **stable** key (port number). Sorting by a volatile metric
     (CPU/mem) applies **hysteresis** (reorder only on large, sustained deltas)
     and **freezes reordering while navigating** (pin order ~2s after last
     keypress, reconcile on idle).
3. **Refresh never resets view state** — model/view fully decoupled (above).
4. **No core peg** — sampler off the UI thread; two cadences; adaptive idle.
5. **CPU% jitter** — EWMA smoothing on displayed CPU%.

---

## Crate stack

| Crate | Role |
|---|---|
| `ratatui` + `crossterm` | Rendering (diff buffer) + input/backend |
| `sysinfo` | Per-process cpu/mem, cwd, cmd (argv), exe, parent/tree. Handles cumulative-CPU delta math. |
| `netstat2` | Listening socket → PID, cross-platform (libproc on macOS). The port→PID join. |
| `libproc` (macOS, later) | Precision sysinfo lacks: `phys_footprint` (matches Activity Monitor) and open-fd enumeration (feeds log fd-sniffing). |
| `crossbeam-channel` (or `std::sync::mpsc`) | sampler → UI messages |
| `arc-swap` (optional) | atomic snapshot swap |
| `serde` + `toml` | config |

Data-layer shape: `netstat2` gives `{port → pid}`; `sysinfo` gives
`{pid → cpu/mem/cwd/argv/parent}`; join + roll the subtree up into a `Target`.
A thin layer over two mature crates — not raw syscalls.

---

## Modules

1. **`sources`** — `netstat2` + `sysinfo` adapters behind `PortSource` / `ProcSource`
   traits. The trait boundary is where macOS vs. Linux diverge later.
2. **`model`** — `Target`, `TargetKey`, `Snapshot`, discovery + subtree rollup,
   anchor fingerprint. Immutable.
3. **`resolve`** — heuristic pipeline + cache + user-override config.
4. **`verbs`** — kill-tree, restart, copy/open, best-effort log discovery.
5. **`ui`** — Ratatui widgets, view state, event loop, sort/hysteresis/navigation.

### Anchor & subtree boundary

The climb from
the socket-holding PID stops at the **package boundary** (nearest project marker —
`package.json`, `Cargo.toml`, `go.mod`, `pyproject.toml`, `Gemfile`, … or `.git` —
= project root `R`); the top-most in-`R` ancestor is the
**anchor**; the subtree = anchor + descendants inside `R`. A process belongs to
at most one Target.

### Type contract — `model` (sketch)

```rust
/// Stable identity used to follow the cursor selection across snapshots.
#[derive(Clone, PartialEq, Eq, Hash)]
enum TargetKey {
    Port(u16),                                            // listener targets
    Command { project: String, label: String, cwd: PathBuf }, // watched targets
}

enum TargetKind { Listener, Watched }

/// Representative process; (pid, start_time) is the reuse-guard fingerprint.
struct Anchor { pid: u32, start_time: u64 }

struct Target {
    key: TargetKey,
    kind: TargetKind,
    ports: Vec<u16>,            // empty for Watched
    anchor: Anchor,
    pids: Vec<u32>,             // in-boundary subtree
    project: String,
    command_label: String,      // e.g. "next dev"
    cwd: PathBuf,
    git_branch: Option<String>,
    cpu_pct: f32,               // EWMA-smoothed, subtree rollup
    mem_bytes: u64,             // subtree rollup
    url: Option<String>,        // None for watched targets
}

/// Immutable, produced by the sampler. Canonical order (by port); the UI
/// applies presentation order (sort + hysteresis) — ordering is NOT baked here.
struct Snapshot { seq: u64, targets: Vec<Target> }
```

### Message contract — the sampler↔UI seam

```rust
// sampler -> UI
enum SamplerMsg {
    Snapshot(Arc<Snapshot>),
    Error(String),              // surfaced in a status line, never panics the UI
}

// UI -> sampler / action thread
enum UiMsg {
    SetFocused(bool),           // drives adaptive idle cadence
    RequestRefresh,             // e.g. right after a verb, to reflect it fast
    Verb { target: TargetKey, verb: Verb },
    Shutdown,
}

enum Verb { Kill, Restart, CopyUrl, Open, ToggleTail }
```

Two subtleties this contract encodes:

- **Ordering is the UI's job, not the sampler's.** Hysteresis and the
  freeze-while-navigating rule depend on view state (active sort, time since last
  keypress), which lives only on the UI thread. So `Snapshot.targets` is in a
  fixed canonical order and the UI re-sorts. This is what makes "no jump under the
  cursor" implementable.
- **Blocking verbs don't run on the UI thread.** `Kill`/`Restart` (signals,
  re-exec) go to an action executor (the sampler thread or a small dedicated
  thread) via `UiMsg::Verb`, so the render loop never stalls. `CopyUrl`/`Open` are
  cheap and can run inline on the UI thread.

### `resolve` — heuristic pipeline (run once per target, cached by PID)

**Language- and framework-agnostic by design.** Nothing here is Node-specific;
package.json is just one manifest among many. Inputs (macOS via
`sysinfo`/`netstat2`, libproc for precise bits):

- **cwd** — gold input; project lives at/above it.
- **argv** — recognize the command via the rule table (below).
- **manifest** — walk up from cwd to the nearest **project marker** and read a
  name from it.
- **git** — walk up for `.git`; repo dir name fallback; branch as a detail.

**Project markers & name parsers** (tried in priority, then git, then dirname):

| Marker | Ecosystem | Name from |
|---|---|---|
| `package.json` | Node | `name` |
| `pyproject.toml` / `setup.py` / `Pipfile` | Python | `[project].name` / `[tool.poetry].name` |
| `Cargo.toml` | Rust | `[package].name` |
| `go.mod` | Go | module path basename |
| `Gemfile` / `*.gemspec` | Ruby | gemspec name / dir |
| `pom.xml` / `build.gradle(.kts)` | JVM | artifactId / dir |
| `mix.exs` | Elixir | app name |
| `composer.json` | PHP | `name` |
| `*.csproj` / `*.sln` | .NET | file stem |
| `.git` (fallback) | any | repo dir name |

**Default command rules** ship for many ecosystems (all user-extensible/-overridable):

- **Node:** `next dev`, `vite`, `nuxt`, `remix`, `astro`, `nodemon`, `node <script>`
- **Python:** `uvicorn`, `gunicorn`, `hypercorn`, `flask run`, `manage.py runserver`,
  `celery`, `streamlit`
- **Ruby:** `rails s`, `puma`, `unicorn`, `sidekiq`
- **Go:** `go run`, `air`, compiled binaries
- **Rust:** `cargo run`, `cargo watch`, the built binary
- **JVM:** `gradle bootRun`, `spring-boot:run`, `java -jar`
- **Elixir:** `mix phx.server` · **.NET:** `dotnet run/watch` · **PHP:** `php artisan serve`, `php -S`
- **Local infra:** `postgres`, `redis-server`, `mysqld`, `mongod`, `memcached`
  (listen on ports; shown with a `·db`/`·cache` category tag, not dimmed)

**Typed, optional URL.** The `url` is rule-driven and carries a *scheme*, not just
http: `http`/`https` (web servers), `postgres://`/`redis://`/`mysql://`
(databases), or **none** (a watcher, a non-networked tool). "Open in browser"
applies only to `http(s)`; "copy URL" copies the connection string for the rest.

**Docker / containers.** When the anchor is `docker-proxy` / `com.docker.*`, the
real process lives in a container. A dedicated resolver queries Docker (host port
→ container) so the row reads `myproject-db-1 · postgres · :5432` instead of
`docker-proxy`. Treated as a recognized special case in `resolve`.

Project-name priority: manifest name → git repo name → `basename(cwd)`.
Command label + URL come from the config rule table. Manual `[[override]]` wins.
Heuristics **will** be wrong sometimes (monorepos, shell-launched procs) — the
override mechanism is the escape hatch, designed in from day one.

**Wrapper see-through — IMPLEMENTED.** The label is resolved by scanning the
target's whole process *subtree* for a recognized tool, so launchers
(`pnpm`/`npm`/`yarn`/`npx`/`node`) resolve to the real command running beneath
them (`pnpm dev` → `vite`). Falls back to the first non-wrapper program, then to
the bare command.

**Cache — IMPLEMENTED (path-keyed).** Project root is cached by `cwd`, project
name by root — so the hot loop doesn't re-walk the tree or re-parse manifests
each tick. Git branch is read fresh each tick (one small file) so it stays live
across `checkout`. (A finer PID+start_time-keyed cache with fingerprint eviction
remains a possible refinement.)

---

## Verbs (OS-support varies — scoped honestly)

| Verb | Approach | Caveat |
|---|---|---|
| **copy URL** | build from config `url` template + port; `pbcopy` | absent for port-less targets |
| **open browser** | same URL → `open` | absent for port-less targets |
| **kill** | kill the **tree/process-group**, SIGTERM → SIGKILL after timeout | guard system/root PIDs; brief undo window instead of confirm dialog for one-keystroke feel |
| **restart** | capture argv + cwd (+ env where possible); kill tree; re-exec | shell-function / complex-env launches won't always restart cleanly — be upfront |
| **tail logs** | best-effort: fd-sniff for `.log`; supervisor logs (pm2/foreman/docker); full capture only for our-launched targets | weakest OS support — can't generally attach to a running process's stdout on macOS |

**Wired:** copy, open, kill (SIGTERM now → SIGKILL after a 4s grace period; the
escalation re-checks each pid's start_time and only signals the *same* process,
so a recycled pid is never hit; `u` undoes within the window), restart (kill
subtree → wait → re-exec captured argv in cwd, off-thread).
**Follow-ups:** env capture for restart, supervisor logs (pm2/foreman).

Navigation: `j/k` move, `g/G` top/bottom, `/` filter, `s` cycle sort
(port/cpu/mem), `q` quit. **Verbs are capital letters** — `K` kill, `R` restart,
`T` tail, `Y` copy-url, `O` open — distinct from lowercase nav so `k` (up) never
collides with kill, and so destructive actions require a deliberate Shift.
(`u` after a kill undoes the pending SIGKILL.)

---

## CLI / agent interface — IMPLEMENTED

The resolution engine is exposed headless for scripts and agents. The TUI is the no-arg default;
subcommands act through the same engine:

```
marina ls [--json]        # the snapshot — table, or a stable JSON contract
marina kill <selector>…   # SIGTERM -> SIGKILL matching targets
marina restart <selector> # re-exec captured argv in cwd
marina url <selector>…    # print matching URLs
```

A **selector** matches by project name (exact/substring), port (`3000`/`:3000`),
or command label. **Killing a project selector stops every target under it** —
this selector resolution is the grouping primitive a future TUI group-kill reuses.
An agent can `ls --json` to see `client-portal → [3000, 5432]`, then
`kill client-portal` to stop it precisely.

## Testing — 47 tests

Three layers:

- **Pure logic** (most of it): resolution (wrapper see-through, watcher detection
  incl. the shell-false-positive regression, URL schemes, manifest name parsing,
  overrides, declared groups), the package-boundary `climb` (boundary / shell /
  cycle), `subtree`/`rollup`/curation, CLI selectors, config parsing, the kill
  `survivors` guard, JSON view mapping.
- **Build pipeline** via injected fake `PortSource`/`ProcSource` — `Sampler::build`
  end to end (resolves a listener to project · tool · port · url; drops a non-dev
  daemon). `Resolver::empty()` keeps it config-independent.
- **UI rendering** via ratatui `TestBackend` (group header, members, collapse,
  filter, empty state, chrome) + interaction (sort, wrap, selection-by-key, undo,
  status expiry).
- **Real-OS integration** (stable): `sysinfo` sees our own process, `netstat2`
  enumerates a freshly-bound listener, `lsof` discovers a `.log` we hold open,
  `tail` follows appended lines, `phys_footprint` of self is non-zero.

Not unit-tested (verified manually / by `--dump`): live side-effects of
kill/restart signalling, and adaptive-cadence timing.

## macOS permission note

cwd/argv/fds are freely readable for **your own** processes; root-owned daemons
need sudo. Since this is dev-process-centric (your user's processes), mostly a
non-issue — rows for system stuff may just be opaque.

---

## Implemented (this build)

- **Config file** (`~/.config/marina/config.toml`): `[[rule]]` (regex →
  label + url template), `[[watch]]`, `[[override]]` (pin by port/cmd). Layered
  over built-in defaults; bad regex / absent file falls back silently.
- **Inline log tail** (`T`): discovery in order of confidence — a `.log` held
  open by the subtree (`lsof`), then `*.log` in the project dir / `logs/`, then a
  pm2 log matching the project — live-tailed in a pane; `Esc` closes. Honest "no
  log file" when nothing's found.
- **Kill undo** (`u`): cancels the pending SIGKILL within the 4s grace window.
- **`/` filter** (project/command/port), `g`/`G` jump-to-top/bottom, **uptime**
  column.
- **Docker resolver**: names host-bound container ports via `docker ps`
  (fail-silent). _Port parser unit-tested; live container rows pending a running
  daemon to verify. Container cpu/mem not captured — shown as `—`._
- **`phys_footprint` memory** via `libproc` (Activity-Monitor-matching), with RSS
  fallback.
- **CLI / agent surface**: `ls`/`kill`/`restart`/`url` with `--json`
  and project/port/command selectors. Project-selector kill = group-kill.
- **TUI grouping**: a project with several targets gets a collapsible header
  (`Enter` folds; `K`/`R` act on the whole group); a lone target stays a plain
  row. Group cpu/mem aggregate in the header; same project-matching as the CLI.
- **Declared groups** (`config [[group]]`): bundle targets that don't share a
  cwd — an app + its database — under one name by port/project/command selectors.
  Applies to listener, watched, and docker targets; works in both TUI and CLI
  (`kill <group>`). Auto-grouping still covers app + watchers for free.

## Still open

- **MCP wrapper** over the CLI (deferred; CLI+JSON covers agents today).
- **env capture** for restart (needs `KERN_PROCARGS2`; restart currently
  re-execs argv in cwd with inherited env — fine for most `.env`/profile setups).
- Docker: live verification + container cpu/mem (`docker stats`).
- Two-cadence split (optional — adaptive single-path suffices today).
- Terminal-focus-based idle backoff; Linux source impls;
  hybrid "expand a Target row to reveal the PID subtree" view.
