//! The sampler: enumerates listeners + processes, joins them, climbs to the
//! package boundary (ADR 0002), rolls up subtrees, and emits an immutable
//! Snapshot. Runs on its own thread; the UI never blocks on this work.
//!
//! Filesystem resolution (project root + name) is cached by path so the hot
//! loop doesn't re-walk the tree every tick. The thread uses an adaptive
//! cadence: ~1s while topology is changing, backing off toward ~5s once the set
//! of targets is stable — so a cockpit left open all day idles near zero CPU.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::model::{Anchor, Snapshot, Target, TargetKey, TargetKind};
use crate::msg::SamplerMsg;
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

type RootCache = HashMap<PathBuf, Option<PathBuf>>;
type NameCache = HashMap<PathBuf, String>;

pub struct Sampler {
    ports: Box<dyn PortSource + Send>,
    procs: Box<dyn ProcSource + Send>,
    resolver: resolve::Resolver,
    ewma: HashMap<u32, f32>, // smoothed CPU keyed by anchor pid
    root_cache: RootCache,   // cwd -> project root
    name_cache: NameCache,   // project root -> project name
    home: Option<PathBuf>,
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
            home: std::env::var_os("HOME").map(PathBuf::from),
            seq: 0,
        }
    }

    pub fn build(&mut self) -> Snapshot {
        self.procs.refresh();

        // Pull caches out as locals so the closure doesn't borrow all of `self`
        // while `self.procs` is borrowed immutably.
        let mut root_cache = std::mem::take(&mut self.root_cache);
        let mut name_cache = std::mem::take(&mut self.name_cache);
        let home = self.home.clone();

        // Phase 1: build raw targets (CPU not yet smoothed).
        let mut targets: Vec<Target> = {
            let procs = self.procs.procs();
            let children = child_map(procs);
            let listeners = self.ports.listeners();

            // Group listener ports by their (boundary-bounded) anchor.
            let mut by_anchor: HashMap<u32, AnchorAgg> = HashMap::new();
            let mut docker_ports: HashSet<u16> = HashSet::new();
            for l in &listeners {
                let Some(p) = procs.get(&l.pid) else { continue };
                // Ports bound by the docker host proxy are resolved via `docker ps`.
                if crate::docker::is_binder(&p.name) {
                    docker_ports.insert(l.port);
                    continue;
                }
                let root = p.cwd.as_deref().and_then(|c| root_of(c, &mut root_cache));
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

            // Listener targets.
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

                let argvs = subtree_argvs(&subtree, procs);
                let (mut label, url) = self.resolver.label_and_url(&argvs, Some(key_port));
                let mut project = project_name(agg.root.as_deref(), &cwd, &mut name_cache);
                self.resolver.apply_override(
                    Some(key_port),
                    &anchor_argv.join(" "),
                    &mut project,
                    &mut label,
                );
                if let Some(g) = self.resolver.group_name(&ports, &project, &label) {
                    project = g;
                }
                let git_branch = agg.root.as_deref().and_then(resolve::git_branch);

                out.push(Target {
                    key: TargetKey::Port(key_port),
                    kind: TargetKind::Listener,
                    ports,
                    anchor: Anchor {
                        pid: anchor,
                        start_time: anchor_p.map(|p| p.start_time).unwrap_or(0),
                    },
                    anchor_argv,
                    pids: subtree,
                    project,
                    command_label: label,
                    cwd,
                    git_branch,
                    cpu_pct: cpu_raw,
                    mem_bytes: mem,
                    url,
                });
            }

            // Watched targets: standalone port-less watchers not already claimed
            // by a listener subtree (subtree absorption — ADR 0001).
            let mut watched: HashMap<(String, String, PathBuf), WatchAgg> = HashMap::new();
            for p in procs.values() {
                if claimed.contains(&p.pid) || resolve::is_shell(&p.name) {
                    continue;
                }
                let Some(label) = self.resolver.watcher_label(&p.argv) else {
                    continue;
                };
                let root = p.cwd.as_deref().and_then(|c| root_of(c, &mut root_cache));
                let cwd = p.cwd.clone().unwrap_or_default();
                let mut project = project_name(root.as_deref(), &cwd, &mut name_cache);
                if let Some(g) = self.resolver.group_name(&[], &project, &label) {
                    project = g;
                }
                let sub = subtree(p.pid, &children);
                let (cpu, mem) = rollup(&sub, procs);
                let agg = watched
                    .entry((project.clone(), label.clone(), cwd.clone()))
                    .or_insert_with(|| WatchAgg {
                        anchor: p.pid,
                        start_time: p.start_time,
                        argv: p.argv.clone(),
                        project,
                        label,
                        cwd,
                        pids: Vec::new(),
                        cpu: 0.0,
                        mem: 0,
                    });
                agg.cpu += cpu;
                agg.mem += mem;
                agg.pids.extend(sub);
            }
            for w in watched.into_values() {
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
                    pids: w.pids,
                    project: w.project,
                    command_label: w.label,
                    cwd: w.cwd,
                    git_branch: None,
                    cpu_pct: w.cpu,
                    mem_bytes: w.mem,
                    url: None,
                });
            }

            // Docker targets: name host-bound container ports via `docker ps`.
            // Container cpu/mem live in the VM and aren't captured (shown as `—`,
            // since pids is empty). Fail-silent: no daemon -> no rows.
            if !docker_ports.is_empty() {
                let dmap = crate::docker::port_map();
                let mut ports: Vec<u16> = docker_ports.into_iter().collect();
                ports.sort_unstable();
                for port in ports {
                    if let Some((name, image)) = dmap.get(&port) {
                        let project = self
                            .resolver
                            .group_name(&[port], name, image)
                            .unwrap_or_else(|| name.clone());
                        out.push(Target {
                            key: TargetKey::Port(port),
                            kind: TargetKind::Listener,
                            ports: vec![port],
                            anchor: Anchor {
                                pid: 0,
                                start_time: 0,
                            },
                            anchor_argv: Vec::new(),
                            pids: Vec::new(),
                            project,
                            command_label: image.clone(),
                            cwd: PathBuf::new(),
                            git_branch: None,
                            cpu_pct: 0.0,
                            mem_bytes: 0,
                            url: resolve::default_url(image, port),
                        });
                    }
                }
            }
            out
        };

        self.root_cache = root_cache;
        self.name_cache = name_cache;

        // Phase 2: smooth CPU (EWMA) keyed by anchor pid.
        for t in &mut targets {
            t.cpu_pct = self.smooth(t.anchor.pid, t.cpu_pct);
        }
        self.prune_ewma(&targets);

        // Canonical order: listeners by port asc, watched after, by project.
        targets.sort_by(|a, b| match (a.ports.first(), b.ports.first()) {
            (Some(x), Some(y)) => x.cmp(y),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => a.project.cmp(&b.project),
        });

        self.seq += 1;
        Snapshot {
            seq: self.seq,
            targets,
        }
    }

    fn smooth(&mut self, anchor: u32, raw: f32) -> f32 {
        let e = self.ewma.entry(anchor).or_insert(raw);
        *e = 0.4 * raw + 0.6 * *e;
        *e
    }

    fn prune_ewma(&mut self, targets: &[Target]) {
        let live: HashSet<u32> = targets.iter().map(|t| t.anchor.pid).collect();
        self.ewma.retain(|pid, _| live.contains(pid));
    }
}

impl Default for Sampler {
    fn default() -> Self {
        Self::new()
    }
}

/// Topology fingerprint — the set of target keys. Used to detect "nothing
/// structurally changed" so the cadence can back off.
fn topology(snap: &Snapshot) -> Vec<TargetKey> {
    snap.targets.iter().map(|t| t.key.clone()).collect()
}

/// Spawn the sampler thread; returns the channel the UI drains.
/// Adaptive cadence: 1s while topology changes, doubling toward a 5s cap once
/// stable. The thread also wakes immediately if the UI requests it (drop the
/// returned sender to stop the thread).
pub fn spawn() -> Receiver<SamplerMsg> {
    const FAST: Duration = Duration::from_millis(1000);
    const MAX: Duration = Duration::from_millis(5000);

    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut sampler = Sampler::new();
        let mut prev_topology: Vec<TargetKey> = Vec::new();
        let mut delay = FAST;
        loop {
            let snap = sampler.build();
            let topo = topology(&snap);
            if topo == prev_topology {
                delay = (delay * 2).min(MAX); // stable -> back off
            } else {
                delay = FAST; // changed -> stay responsive
                prev_topology = topo;
            }
            if tx.send(SamplerMsg::Snapshot(Arc::new(snap))).is_err() {
                break; // UI gone
            }
            thread::sleep(delay);
        }
    });
    rx
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
    pids: Vec<u32>,
    cpu: f32,
    mem: u64,
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

fn root_of(cwd: &Path, cache: &mut RootCache) -> Option<PathBuf> {
    if let Some(r) = cache.get(cwd) {
        return r.clone();
    }
    let r = resolve::project_root(cwd);
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

    struct FakePorts(Vec<crate::sources::Listener>);
    impl PortSource for FakePorts {
        fn listeners(&mut self) -> Vec<crate::sources::Listener> {
            self.0
                .iter()
                .map(|l| crate::sources::Listener {
                    port: l.port,
                    pid: l.pid,
                })
                .collect()
        }
    }
    struct FakeProcs(HashMap<u32, ProcInfo>);
    impl ProcSource for FakeProcs {
        fn refresh(&mut self) {}
        fn procs(&self) -> &HashMap<u32, ProcInfo> {
            &self.0
        }
    }

    fn sampler_with(listeners: Vec<crate::sources::Listener>, procs: Vec<ProcInfo>) -> Sampler {
        Sampler {
            ports: Box::new(FakePorts(listeners)),
            procs: Box::new(FakeProcs(map(procs))),
            resolver: resolve::Resolver::empty(),
            ewma: HashMap::new(),
            root_cache: HashMap::new(),
            name_cache: HashMap::new(),
            home: Some(PathBuf::from("/Users/me")),
            seq: 0,
        }
    }

    #[test]
    fn build_resolves_a_listener_end_to_end() {
        use crate::sources::Listener;
        // a vite server under $HOME (no manifest on disk -> project = cwd basename)
        let procs = vec![proc(
            200,
            Some(1),
            "node",
            "/Users/me/web",
            &["node", "/x/.bin/vite", "dev"],
        )];
        let mut s = sampler_with(
            vec![Listener {
                port: 3000,
                pid: 200,
            }],
            procs,
        );
        let snap = s.build();
        assert_eq!(snap.targets.len(), 1);
        let t = &snap.targets[0];
        assert_eq!(t.ports, vec![3000]);
        assert_eq!(t.command_label, "vite"); // subtree label resolution
        assert_eq!(t.project, "web"); // cwd basename fallback
        assert!(t.url.as_ref().map(|u| u.value.as_str()) == Some("http://localhost:3000"));
    }

    #[test]
    fn build_drops_non_dev_listeners() {
        use crate::sources::Listener;
        // cwd "/" with no project root, not under $HOME -> filtered out
        let procs = vec![proc(50, Some(1), "rapportd", "/", &["/usr/sbin/rapportd"])];
        let mut s = sampler_with(
            vec![Listener {
                port: 50555,
                pid: 50,
            }],
            procs,
        );
        assert_eq!(s.build().targets.len(), 0);
    }
}
