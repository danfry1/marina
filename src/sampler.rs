//! The sampler: enumerates listeners + processes, joins them, climbs to the
//! package boundary (ADR 0002), rolls up subtrees, and emits an immutable
//! Snapshot. Runs on its own thread; the UI never blocks on this work.
//!
//! Filesystem resolution (project root + name) is cached by path so the hot
//! loop doesn't re-walk the tree every tick; the caches clear whenever the
//! topology changes so renames are picked up. The thread uses an adaptive
//! cadence: ~1s while topology is changing, backing off toward ~5s once the
//! set of targets is stable — and the UI can demand an immediate rebuild over
//! the control channel (e.g. right after a verb).

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crate::model::{Anchor, Snapshot, Target, TargetKey, TargetKind};
use crate::msg::{SamplerCtl, SamplerMsg};
use crate::resolve;
use crate::sources::{Netstat2Ports, PortSource, ProcInfo, ProcSource, SysinfoProcs};

const NON_DEV_PARENTS: &[&str] = &[
    "zsh",
    "-zsh",
    "bash",
    "-bash",
    "sh",
    "fish",
    "tmux",
    "tmux: server",
    "login",
    "sshd",
    "ssh",
    "init",
    "launchd",
    "systemd",
];

/// How long a `docker ps` result is trusted before re-querying — the daemon
/// call is a subprocess and must not run on every 1s tick.
const DOCKER_TTL: Duration = Duration::from_secs(5);

type RootCache = HashMap<PathBuf, Option<PathBuf>>;
type NameCache = HashMap<PathBuf, String>;

pub struct Sampler {
    ports: Box<dyn PortSource + Send>,
    procs: Box<dyn ProcSource + Send>,
    resolver: resolve::Resolver,
    ewma: HashMap<(u32, u64), f32>, // smoothed CPU keyed by anchor fingerprint
    root_cache: RootCache,          // cwd -> project root
    name_cache: NameCache,          // project root -> project name
    docker_cache: Option<(Instant, HashMap<u16, crate::docker::ContainerPort>)>,
    home: Option<PathBuf>,
    last_topology: Vec<TargetKey>,
    /// Did the last `build()` change the set of target keys? Drives cadence.
    pub topology_changed: bool,
    seq: u64,
}

impl Sampler {
    pub fn new() -> Self {
        Sampler {
            ports: Box::new(Netstat2Ports),
            procs: Box::new(SysinfoProcs::new()),
            resolver: resolve::Resolver::load(),
            ewma: HashMap::new(),
            root_cache: HashMap::new(),
            name_cache: HashMap::new(),
            docker_cache: None,
            home: std::env::var_os("HOME").map(PathBuf::from),
            last_topology: Vec::new(),
            topology_changed: false,
            seq: 0,
        }
    }

    pub fn build(&mut self) -> Snapshot {
        self.procs.refresh();

        // A port-scan failure must surface, never render as "no dev processes".
        let (listeners, error) = match self.ports.listeners() {
            Ok(l) => (l, None),
            Err(e) => (Vec::new(), Some(e)),
        };

        // Pull caches out as locals so the closure doesn't borrow all of `self`
        // while `self.procs` is borrowed immutably.
        let mut root_cache = std::mem::take(&mut self.root_cache);
        let mut name_cache = std::mem::take(&mut self.name_cache);
        let home = self.home.clone();

        // Docker host-proxy ports found this pass (resolved after the borrow ends).
        let mut docker_ports: HashSet<u16> = HashSet::new();

        // Phase 1: build raw targets (CPU not yet smoothed).
        let mut targets: Vec<Target> = {
            let procs = self.procs.procs();
            let children = child_map(procs);
            // Never list the session marina runs inside (its own ancestor chain:
            // shell, terminal/ttyd, sshd, …) — you'd never want to kill that.
            let own = ancestors(std::process::id(), procs);
            // Any listener on a non-loopback address is LAN-reachable.
            let mut exposed_ports: HashMap<u16, bool> = HashMap::new();
            for l in &listeners {
                *exposed_ports.entry(l.port).or_default() |= l.exposed;
            }

            // Group listener ports by their (boundary-bounded) anchor.
            let mut by_anchor: HashMap<u32, AnchorAgg> = HashMap::new();
            for l in &listeners {
                let Some(p) = procs.get(&l.pid) else { continue };
                if own.contains(&l.pid) {
                    continue; // marina's own session, not a target
                }
                // Ports bound by the docker host proxy are resolved via `docker ps`.
                if crate::docker::is_binder(&p.name) {
                    docker_ports.insert(l.port);
                    continue;
                }
                let root = p
                    .cwd
                    .as_deref()
                    .and_then(|c| root_of(c, &mut root_cache, home.as_deref()));
                if !is_dev_target(p.cwd.as_deref(), root.as_deref(), home.as_deref()) {
                    continue;
                }
                let anchor = climb(l.pid, procs, root.as_deref());
                let agg = by_anchor.entry(anchor).or_insert_with(|| AnchorAgg {
                    ports: Vec::new(),
                    root: root.clone(),
                });
                agg.ports.push(l.port);
                if agg.root.is_none() {
                    agg.root = root;
                }
            }

            let mut claimed: HashSet<u32> = HashSet::new();
            let mut out: Vec<Target> = Vec::new();

            // Listener targets. Two anchors can share a port (SO_REUSEPORT,
            // split v4/v6 binders) — merged below so TargetKey stays unique.
            let mut by_port: HashMap<u16, usize> = HashMap::new();
            for (anchor, agg) in by_anchor {
                let subtree = subtree(anchor, &children);
                claimed.extend(subtree.iter().copied());
                let (cpu_raw, mem) = rollup(&subtree, procs);
                let anchor_p = procs.get(&anchor);
                let anchor_argv = anchor_p.map(|p| p.argv.clone()).unwrap_or_default();
                let cwd = anchor_p.and_then(|p| p.cwd.clone()).unwrap_or_default();

                let mut ports = agg.ports;
                ports.sort_unstable();
                ports.dedup();
                let key_port = *ports.first().expect("listener target has >=1 port");

                let argv_joined = anchor_argv.join(" ");
                let argvs = subtree_argvs(&subtree, procs);
                let (mut label, url) = self.resolver.label_and_url(&argvs, Some(key_port));
                if self.resolver.is_ignored(&ports, &argv_joined)
                    || self.resolver.is_ignored(&ports, &label)
                {
                    continue; // user said: never show this (subtree stays claimed)
                }
                let mut project = project_name(agg.root.as_deref(), &cwd, &mut name_cache);
                self.resolver.apply_override(
                    Some(key_port),
                    &argv_joined,
                    &mut project,
                    &mut label,
                );
                if let Some(g) = self.resolver.group_name(&ports, &project, &label) {
                    project = g;
                }
                let git_branch = agg.root.as_deref().and_then(resolve::git_branch);
                let exposed = ports
                    .iter()
                    .any(|p| exposed_ports.get(p).copied().unwrap_or(false));

                let target = Target {
                    key: TargetKey::Port(key_port),
                    kind: TargetKind::Listener,
                    ports,
                    anchor: Anchor {
                        pid: anchor,
                        start_time: anchor_p.map(|p| p.start_time).unwrap_or(0),
                    },
                    anchor_argv,
                    pid_starts: pid_starts(&subtree, procs),
                    pids: subtree,
                    project,
                    command_label: label,
                    cwd,
                    git_branch,
                    cpu_pct: cpu_raw,
                    mem_bytes: mem,
                    url,
                    exposed,
                    container: None,
                };
                match by_port.get(&key_port) {
                    Some(&i) => merge_into(&mut out[i], target),
                    None => {
                        by_port.insert(key_port, out.len());
                        out.push(target);
                    }
                }
            }

            // Watched targets: standalone port-less watchers not already claimed
            // by a listener subtree (subtree absorption — ADR 0001). Pids are a
            // set: a watcher that is a descendant of another with the same
            // identity must not be double-counted.
            let mut watched: HashMap<(String, String, PathBuf), WatchAgg> = HashMap::new();
            for p in procs.values() {
                if claimed.contains(&p.pid) || resolve::is_shell(&p.name) || own.contains(&p.pid) {
                    continue;
                }
                let Some(label) = self.resolver.watcher_label(&p.argv) else {
                    continue;
                };
                if self.resolver.is_ignored(&[], &p.argv.join(" ")) {
                    continue;
                }
                let root = p
                    .cwd
                    .as_deref()
                    .and_then(|c| root_of(c, &mut root_cache, home.as_deref()));
                let cwd = p.cwd.clone().unwrap_or_default();
                let mut project = project_name(root.as_deref(), &cwd, &mut name_cache);
                if let Some(g) = self.resolver.group_name(&[], &project, &label) {
                    project = g;
                }
                let sub = subtree(p.pid, &children);
                let agg = watched
                    .entry((project.clone(), label.clone(), cwd.clone()))
                    .or_insert_with(|| WatchAgg {
                        anchor: p.pid,
                        start_time: p.start_time,
                        argv: p.argv.clone(),
                        project,
                        label,
                        cwd,
                        pids: HashSet::new(),
                    });
                agg.pids.extend(sub);
            }
            for w in watched.into_values() {
                let pids: Vec<u32> = w.pids.into_iter().collect();
                let (cpu, mem) = rollup(&pids, procs);
                out.push(Target {
                    key: TargetKey::Command {
                        project: w.project.clone(),
                        label: w.label.clone(),
                        cwd: w.cwd.clone(),
                    },
                    kind: TargetKind::Watched,
                    ports: Vec::new(),
                    anchor: Anchor {
                        pid: w.anchor,
                        start_time: w.start_time,
                    },
                    anchor_argv: w.argv,
                    pid_starts: pid_starts(&pids, procs),
                    pids,
                    project: w.project,
                    command_label: w.label,
                    cwd: w.cwd,
                    git_branch: None,
                    cpu_pct: cpu,
                    mem_bytes: mem,
                    url: None,
                    exposed: false,
                    container: None,
                });
            }
            out
        };

        self.root_cache = root_cache;
        self.name_cache = name_cache;

        // Docker targets: name host-bound container ports via `docker ps`
        // (cached — a subprocess must not run on every tick). Container cpu/mem
        // live in the VM and aren't captured (shown as `—`, since pids is
        // empty). Verbs go through `docker stop/restart/logs`.
        if !docker_ports.is_empty() {
            let dmap = self.docker_map().clone(); // small; frees &mut self
            let mut ports: Vec<u16> = docker_ports.into_iter().collect();
            ports.sort_unstable();
            for port in ports {
                if let Some(c) = dmap.get(&port) {
                    if self.resolver.is_ignored(&[port], &c.image)
                        || self.resolver.is_ignored(&[port], &c.name)
                    {
                        continue;
                    }
                    let project = self
                        .resolver
                        .group_name(&[port], &c.name, &c.image)
                        .unwrap_or_else(|| c.name.clone());
                    targets.push(Target {
                        key: TargetKey::Port(port),
                        kind: TargetKind::Listener,
                        ports: vec![port],
                        anchor: Anchor {
                            pid: 0,
                            start_time: 0,
                        },
                        anchor_argv: Vec::new(),
                        pid_starts: Vec::new(),
                        pids: Vec::new(),
                        project,
                        command_label: c.image.clone(),
                        cwd: PathBuf::new(),
                        git_branch: None,
                        cpu_pct: 0.0,
                        mem_bytes: 0,
                        url: resolve::default_url(&c.image, port),
                        exposed: c.exposed,
                        container: Some(c.name.clone()),
                    });
                }
            }
        }

        // Phase 2: smooth CPU (EWMA) keyed by the anchor *fingerprint*, so a
        // recycled pid never inherits a dead process's smoothing history.
        for t in &mut targets {
            t.cpu_pct = self.smooth(&t.anchor, t.cpu_pct);
        }
        self.prune_ewma(&targets);

        // Canonical order: listeners by port asc, watched after, by project.
        targets.sort_by(|a, b| match (a.ports.first(), b.ports.first()) {
            (Some(x), Some(y)) => x.cmp(y),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => a.project.cmp(&b.project),
        });

        // Track topology; on change, drop the path caches so a renamed project
        // or moved root is re-resolved rather than served stale forever.
        let topo: Vec<TargetKey> = targets.iter().map(|t| t.key.clone()).collect();
        self.topology_changed = topo != self.last_topology;
        if self.topology_changed {
            self.root_cache.clear();
            self.name_cache.clear();
            self.last_topology = topo;
        }

        self.seq += 1;
        Snapshot {
            seq: self.seq,
            targets,
            error,
        }
    }

    fn docker_map(&mut self) -> &HashMap<u16, crate::docker::ContainerPort> {
        let stale = self
            .docker_cache
            .as_ref()
            .is_none_or(|(at, _)| at.elapsed() > DOCKER_TTL);
        if stale {
            self.docker_cache = Some((Instant::now(), crate::docker::port_map()));
        }
        &self.docker_cache.as_ref().expect("just set").1
    }

    fn smooth(&mut self, anchor: &Anchor, raw: f32) -> f32 {
        let e = self
            .ewma
            .entry((anchor.pid, anchor.start_time))
            .or_insert(raw);
        *e = 0.4 * raw + 0.6 * *e;
        *e
    }

    fn prune_ewma(&mut self, targets: &[Target]) {
        let live: HashSet<(u32, u64)> = targets
            .iter()
            .map(|t| (t.anchor.pid, t.anchor.start_time))
            .collect();
        self.ewma.retain(|k, _| live.contains(k));
    }
}

impl Default for Sampler {
    fn default() -> Self {
        Self::new()
    }
}

/// Fold `extra` into `base` when two anchors share a port (SO_REUSEPORT /
/// split v4+v6). Rollups add; identity fields keep the first anchor's.
fn merge_into(base: &mut Target, extra: Target) {
    base.ports.extend(extra.ports);
    base.ports.sort_unstable();
    base.ports.dedup();
    for p in extra.pids {
        if !base.pids.contains(&p) {
            base.pids.push(p);
        }
    }
    for ps in extra.pid_starts {
        if !base.pid_starts.contains(&ps) {
            base.pid_starts.push(ps);
        }
    }
    base.cpu_pct += extra.cpu_pct;
    base.mem_bytes += extra.mem_bytes;
    base.exposed |= extra.exposed;
}

/// Spawn the sampler thread; returns the snapshot channel the UI drains and a
/// control sender: `SamplerCtl::Refresh` forces an immediate rebuild, and
/// dropping the sender shuts the thread down. Adaptive cadence: 1s while
/// topology changes, doubling toward a 5s cap once stable.
pub fn spawn() -> (Receiver<SamplerMsg>, Sender<SamplerCtl>) {
    const FAST: Duration = Duration::from_millis(1000);
    const MAX: Duration = Duration::from_millis(5000);

    let (tx, rx) = mpsc::channel();
    let (ctl_tx, ctl_rx) = mpsc::channel::<SamplerCtl>();
    thread::spawn(move || {
        let mut sampler = Sampler::new();
        let mut delay = FAST;
        loop {
            let snap = sampler.build();
            delay = if sampler.topology_changed {
                FAST // changed -> stay responsive
            } else {
                (delay * 2).min(MAX) // stable -> back off
            };
            if tx.send(SamplerMsg::Snapshot(Arc::new(snap))).is_err() {
                break; // UI gone
            }
            match ctl_rx.recv_timeout(delay) {
                Ok(SamplerCtl::Refresh) => {
                    delay = FAST;
                    // collapse a burst of refresh requests into one rebuild
                    while ctl_rx.try_recv().is_ok() {}
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break, // UI shut down
            }
        }
    });
    (rx, ctl_tx)
}

struct AnchorAgg {
    ports: Vec<u16>,
    root: Option<PathBuf>,
}

struct WatchAgg {
    anchor: u32,
    start_time: u64,
    argv: Vec<String>,
    project: String,
    label: String,
    cwd: PathBuf,
    pids: HashSet<u32>,
}

/// Dev-centric curation: keep a listener only if it has a project root, or its
/// cwd is under $HOME. Drops root-owned system daemons (cwd `/`, `/var`).
fn is_dev_target(cwd: Option<&Path>, root: Option<&Path>, home: Option<&Path>) -> bool {
    if root.is_some() {
        return true;
    }
    match (cwd, home) {
        (Some(c), Some(h)) => c.starts_with(h),
        _ => false,
    }
}

fn root_of(cwd: &Path, cache: &mut RootCache, home: Option<&Path>) -> Option<PathBuf> {
    if let Some(r) = cache.get(cwd) {
        return r.clone();
    }
    let r = resolve::project_root(cwd, home);
    cache.insert(cwd.to_path_buf(), r.clone());
    r
}

fn project_name(root: Option<&Path>, cwd: &Path, cache: &mut NameCache) -> String {
    match root {
        Some(r) => {
            if let Some(n) = cache.get(r) {
                return n.clone();
            }
            let n = resolve::project_name(r);
            cache.insert(r.to_path_buf(), n.clone());
            n
        }
        None => cwd
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "?".into()),
    }
}

fn subtree_argvs(pids: &[u32], procs: &HashMap<u32, ProcInfo>) -> Vec<Vec<String>> {
    pids.iter()
        .filter_map(|pid| procs.get(pid).map(|p| p.argv.clone()))
        .collect()
}

/// `(pid, start_time)` fingerprints for a subtree — what verbs verify before
/// signalling, so a recycled pid number is never hit.
fn pid_starts(pids: &[u32], procs: &HashMap<u32, ProcInfo>) -> Vec<(u32, u64)> {
    pids.iter()
        .filter_map(|pid| procs.get(pid).map(|p| (p.pid, p.start_time)))
        .collect()
}

/// `start` plus its ancestor pids, walking up `ppid` (cycle-guarded).
fn ancestors(start: u32, procs: &HashMap<u32, ProcInfo>) -> HashSet<u32> {
    let mut set = HashSet::new();
    let mut cur = start;
    while set.insert(cur) {
        match procs.get(&cur).and_then(|p| p.ppid) {
            Some(pp) if pp != 0 => cur = pp,
            _ => break,
        }
    }
    set
}

fn child_map(procs: &HashMap<u32, ProcInfo>) -> HashMap<u32, Vec<u32>> {
    let mut m: HashMap<u32, Vec<u32>> = HashMap::new();
    for p in procs.values() {
        if let Some(pp) = p.ppid {
            m.entry(pp).or_default().push(p.pid);
        }
    }
    m
}

/// Climb from the socket-holding PID to the top-most ancestor still inside the
/// project root, stopping at shells / pid 1 / a cwd that leaves the root.
fn climb(start: u32, procs: &HashMap<u32, ProcInfo>, root: Option<&Path>) -> u32 {
    let Some(root) = root else { return start };
    let mut cur = start;
    let mut visited: HashSet<u32> = HashSet::new();
    loop {
        if !visited.insert(cur) {
            break; // cycle guard
        }
        let Some(p) = procs.get(&cur) else { break };
        let Some(pp) = p.ppid else { break };
        if pp == 0 || pp == 1 {
            break;
        }
        let Some(parent) = procs.get(&pp) else { break };
        if NON_DEV_PARENTS.contains(&parent.name.as_str()) {
            break;
        }
        match parent.cwd.as_deref() {
            Some(c) if c.starts_with(root) => cur = pp,
            _ => break,
        }
    }
    cur
}

/// Anchor + all descendants.
fn subtree(anchor: u32, children: &HashMap<u32, Vec<u32>>) -> Vec<u32> {
    let mut seen: HashSet<u32> = HashSet::new();
    seen.insert(anchor);
    let mut out = vec![anchor];
    let mut stack = vec![anchor];
    while let Some(n) = stack.pop() {
        if let Some(kids) = children.get(&n) {
            for &k in kids {
                if seen.insert(k) {
                    // skip already-seen pids (cycle / shared child guard)
                    out.push(k);
                    stack.push(k);
                }
            }
        }
    }
    out
}

fn rollup(pids: &[u32], procs: &HashMap<u32, ProcInfo>) -> (f32, u64) {
    let mut cpu = 0.0;
    let mut mem = 0;
    for pid in pids {
        if let Some(p) = procs.get(pid) {
            cpu += p.cpu_pct;
            // Prefer phys_footprint (matches Activity Monitor); fall back to RSS.
            mem += crate::sources::phys_footprint(*pid).unwrap_or(p.mem_bytes);
        }
    }
    (cpu, mem)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::Listener;

    fn proc(pid: u32, ppid: Option<u32>, name: &str, cwd: &str, argv: &[&str]) -> ProcInfo {
        ProcInfo {
            pid,
            ppid,
            name: name.into(),
            argv: argv.iter().map(|s| s.to_string()).collect(),
            cwd: Some(PathBuf::from(cwd)),
            cpu_pct: 0.0,
            mem_bytes: 0,
            start_time: 0,
        }
    }
    fn map(ps: Vec<ProcInfo>) -> HashMap<u32, ProcInfo> {
        ps.into_iter().map(|p| (p.pid, p)).collect()
    }

    #[test]
    fn climb_stops_at_the_package_boundary() {
        // worker(300) -> next dev(200) in apps/web -> turbo(100) at repo root.
        let procs = map(vec![
            proc(100, Some(1), "turbo", "/repo", &["turbo", "run", "dev"]),
            proc(
                200,
                Some(100),
                "node",
                "/repo/apps/web",
                &["node", "next", "dev"],
            ),
            proc(
                300,
                Some(200),
                "node",
                "/repo/apps/web",
                &["node", "worker"],
            ),
        ]);
        // anchors at next dev, NOT at the shared turbo parent (cwd leaves the root)
        assert_eq!(climb(300, &procs, Some(Path::new("/repo/apps/web"))), 200);
    }

    #[test]
    fn climb_stops_at_a_shell_parent() {
        let procs = map(vec![
            proc(700, Some(1), "zsh", "/repo/apps/web", &["-zsh"]),
            proc(
                600,
                Some(700),
                "node",
                "/repo/apps/web",
                &["node", "next", "dev"],
            ),
        ]);
        assert_eq!(climb(600, &procs, Some(Path::new("/repo/apps/web"))), 600);
    }

    #[test]
    fn climb_terminates_on_a_cycle() {
        let procs = map(vec![
            proc(10, Some(20), "node", "/r", &["node"]),
            proc(20, Some(10), "node", "/r", &["node"]),
        ]);
        let a = climb(10, &procs, Some(Path::new("/r"))); // must not hang
        assert!(a == 10 || a == 20);
    }

    #[test]
    fn subtree_collects_descendants_without_dupes() {
        let procs = map(vec![
            proc(1, None, "a", "/r", &[]),
            proc(2, Some(1), "b", "/r", &[]),
            proc(3, Some(2), "c", "/r", &[]),
            proc(4, Some(1), "d", "/r", &[]),
        ]);
        let mut sub = subtree(1, &child_map(&procs));
        sub.sort_unstable();
        assert_eq!(sub, vec![1, 2, 3, 4]);
    }

    #[test]
    fn rollup_sums_only_present_pids() {
        let mut a = proc(1, None, "a", "/r", &[]);
        a.cpu_pct = 1.0;
        a.mem_bytes = 100;
        let mut b = proc(2, Some(1), "b", "/r", &[]);
        b.cpu_pct = 2.0;
        b.mem_bytes = 200;
        let procs = map(vec![a, b]);
        let (cpu, mem) = rollup(&[1, 2, 999], &procs); // 999 absent
        assert_eq!(cpu, 3.0);
        assert_eq!(mem, 300);
    }

    #[test]
    fn curation_keeps_dev_drops_system() {
        let home = Some(Path::new("/Users/me"));
        assert!(is_dev_target(
            Some(Path::new("/Users/me/dev/x")),
            None,
            home
        )); // under $HOME
        assert!(is_dev_target(
            Some(Path::new("/opt/x")),
            Some(Path::new("/opt/x")),
            home
        )); // has root
        assert!(!is_dev_target(Some(Path::new("/")), None, home)); // system daemon
    }

    // --- full build() pipeline via fake sources -----------------------------

    struct FakePorts(Vec<Listener>);
    impl PortSource for FakePorts {
        fn listeners(&mut self) -> Result<Vec<Listener>, String> {
            Ok(self
                .0
                .iter()
                .map(|l| Listener {
                    port: l.port,
                    pid: l.pid,
                    exposed: l.exposed,
                })
                .collect())
        }
    }
    struct FailingPorts;
    impl PortSource for FailingPorts {
        fn listeners(&mut self) -> Result<Vec<Listener>, String> {
            Err("boom".into())
        }
    }
    struct FakeProcs(HashMap<u32, ProcInfo>);
    impl ProcSource for FakeProcs {
        fn refresh(&mut self) {}
        fn procs(&self) -> &HashMap<u32, ProcInfo> {
            &self.0
        }
    }

    fn listener(port: u16, pid: u32) -> Listener {
        Listener {
            port,
            pid,
            exposed: false,
        }
    }

    fn sampler_with(listeners: Vec<Listener>, procs: Vec<ProcInfo>) -> Sampler {
        Sampler {
            ports: Box::new(FakePorts(listeners)),
            procs: Box::new(FakeProcs(map(procs))),
            resolver: resolve::Resolver::empty(),
            ewma: HashMap::new(),
            root_cache: HashMap::new(),
            name_cache: HashMap::new(),
            docker_cache: Some((Instant::now(), HashMap::new())), // never shell out in tests
            home: Some(PathBuf::from("/Users/me")),
            last_topology: Vec::new(),
            topology_changed: false,
            seq: 0,
        }
    }

    #[test]
    fn build_resolves_a_listener_end_to_end() {
        // a vite server under $HOME (no manifest on disk -> project = cwd basename)
        let procs = vec![proc(
            200,
            Some(1),
            "node",
            "/Users/me/web",
            &["node", "/x/.bin/vite", "dev"],
        )];
        let mut s = sampler_with(vec![listener(3000, 200)], procs);
        let snap = s.build();
        assert_eq!(snap.targets.len(), 1);
        let t = &snap.targets[0];
        assert_eq!(t.ports, vec![3000]);
        assert_eq!(t.command_label, "vite"); // subtree label resolution
        assert_eq!(t.project, "web"); // cwd basename fallback
        assert!(t.url.as_ref().map(|u| u.value.as_str()) == Some("http://localhost:3000"));
        assert_eq!(t.pid_starts, vec![(200, 0)]); // kill fingerprint captured
        assert!(snap.error.is_none());
    }

    #[test]
    fn build_surfaces_a_port_scan_failure() {
        let mut s = sampler_with(vec![], vec![]);
        s.ports = Box::new(FailingPorts);
        let snap = s.build();
        assert_eq!(snap.error.as_deref(), Some("boom"));
    }

    #[test]
    fn build_merges_two_anchors_on_one_port() {
        // SO_REUSEPORT: two separate anchor processes both hold :4000.
        let procs = vec![
            proc(200, Some(1), "node", "/Users/me/a", &["node", "s.js"]),
            proc(300, Some(1), "node", "/Users/me/a", &["node", "s.js"]),
        ];
        let mut s = sampler_with(vec![listener(4000, 200), listener(4000, 300)], procs);
        let snap = s.build();
        assert_eq!(
            snap.targets.len(),
            1,
            "one row per port, not one per anchor"
        );
        let t = &snap.targets[0];
        let mut pids = t.pids.clone();
        pids.sort_unstable();
        assert_eq!(pids, vec![200, 300]);
    }

    #[test]
    fn build_marks_lan_exposed_listeners() {
        let procs = vec![proc(
            200,
            Some(1),
            "node",
            "/Users/me/web",
            &["node", "s.js"],
        )];
        let mut s = sampler_with(
            vec![Listener {
                port: 3000,
                pid: 200,
                exposed: true,
            }],
            procs,
        );
        assert!(s.build().targets[0].exposed);
    }

    #[test]
    fn nested_watchers_do_not_double_count() {
        // nodemon spawns a nodemon child: same (project, label, cwd) identity —
        // the child's pids must not be counted twice. (Pids far above any real
        // pid, so the phys_footprint lookup in rollup can't hit a live process.)
        let mut parent = proc(
            900_500,
            Some(1),
            "node",
            "/Users/me/lib",
            &["node", "/x/.bin/nodemon"],
        );
        parent.mem_bytes = 100;
        let mut child = proc(
            900_501,
            Some(900_500),
            "node",
            "/Users/me/lib",
            &["node", "/x/.bin/nodemon"],
        );
        child.mem_bytes = 40;
        let mut s = sampler_with(vec![], vec![parent, child]);
        let snap = s.build();
        assert_eq!(snap.targets.len(), 1);
        let t = &snap.targets[0];
        assert_eq!(t.pids.len(), 2, "both pids, once each");
        assert_eq!(t.mem_bytes, 140, "memory counted once per pid");
    }

    #[test]
    fn ancestors_walks_the_parent_chain() {
        let procs = map(vec![
            proc(1, None, "launchd", "/", &[]),
            proc(50, Some(1), "sshd", "/", &[]),
            proc(60, Some(50), "zsh", "/Users/me", &[]),
            proc(70, Some(60), "marina", "/Users/me", &[]),
        ]);
        let set = ancestors(70, &procs);
        assert!(set.contains(&70) && set.contains(&60) && set.contains(&50) && set.contains(&1));
        assert!(!set.contains(&999));
    }

    #[test]
    fn build_excludes_marinas_own_session() {
        // a listener whose pid is marina's own pid (i.e. our session) is dropped
        let me = std::process::id();
        let procs = vec![proc(
            me,
            Some(1),
            "node",
            "/Users/me/web",
            &["node", "server.js"],
        )];
        let mut s = sampler_with(vec![listener(4000, me)], procs);
        assert_eq!(s.build().targets.len(), 0);
    }

    #[test]
    fn build_drops_non_dev_listeners() {
        // cwd "/" with no project root, not under $HOME -> filtered out
        let procs = vec![proc(50, Some(1), "rapportd", "/", &["/usr/sbin/rapportd"])];
        let mut s = sampler_with(vec![listener(50555, 50)], procs);
        assert_eq!(s.build().targets.len(), 0);
    }

    #[test]
    fn topology_change_is_flagged_then_settles() {
        let procs = vec![proc(
            200,
            Some(1),
            "node",
            "/Users/me/web",
            &["node", "s.js"],
        )];
        let mut s = sampler_with(vec![listener(3000, 200)], procs);
        s.build();
        assert!(s.topology_changed, "first build changes topology");
        s.build();
        assert!(!s.topology_changed, "same targets -> stable");
    }
}
