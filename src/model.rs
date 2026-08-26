//! The immutable data model. See DESIGN.md "Type contract" and CONTEXT.md.

use std::path::PathBuf;

/// Stable identity used to follow the cursor selection across snapshots.
/// Port-primary for listeners (with the anchor-fingerprint reuse guard);
/// command-based for port-less watched targets.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum TargetKey {
    Port(u16),
    Command {
        project: String,
        label: String,
        cwd: PathBuf,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TargetKind {
    Listener,
    Watched,
}

/// Representative process. `(pid, start_time)` is the reuse-guard fingerprint.
#[derive(Clone)]
pub struct Anchor {
    pub pid: u32,
    pub start_time: u64,
}

/// One row in the cockpit: a developer workload acted on by verbs.
#[derive(Clone)]
pub struct Target {
    pub key: TargetKey,
    pub kind: TargetKind,
    pub ports: Vec<u16>, // empty for Watched
    pub anchor: Anchor,
    pub anchor_argv: Vec<String>, // captured for `restart`
    pub pids: Vec<u32>,           // in-boundary subtree
    /// `(pid, start_time)` for every subtree pid — the fingerprint verbs check
    /// before signalling, so a recycled pid number is never hit.
    pub pid_starts: Vec<(u32, u64)>,
    pub project: String,
    pub command_label: String,
    pub cwd: PathBuf,
    pub git_branch: Option<String>,
    pub cpu_pct: f32,   // EWMA-smoothed, subtree rollup
    pub mem_bytes: u64, // subtree rollup
    pub url: Option<Url>,
    /// Listening on a non-loopback address (0.0.0.0 / ::) — reachable from the LAN.
    pub exposed: bool,
    /// Docker container name when this target is a published container port.
    /// Verbs act via `docker stop/restart/logs` instead of signals.
    pub container: Option<String>,
}

/// Typed, optional URL — the verb layer decides what "open" / "copy" mean per scheme.
#[derive(Clone)]
pub struct Url {
    pub scheme: UrlScheme,
    pub value: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum UrlScheme {
    Http,
    Https,
    Postgres,
    Redis,
    Mysql,
    Other,
}

impl UrlScheme {
    /// "Open in browser" only applies to web schemes.
    pub fn is_web(self) -> bool {
        matches!(self, UrlScheme::Http | UrlScheme::Https)
    }
}

/// Immutable, produced by the sampler. Held in a canonical order (by port);
/// the UI applies presentation order (sort + hysteresis) — ordering is NOT
/// baked in here. That separation is what makes "no jump under the cursor" work.
pub struct Snapshot {
    pub seq: u64,
    pub targets: Vec<Target>,
    /// A data-source failure (e.g. the port scan errored). Shown persistently
    /// in the UI — an empty cockpit must never silently mean "enumeration broke".
    pub error: Option<String>,
}

impl Snapshot {
    pub fn empty() -> Self {
        Snapshot {
            seq: 0,
            targets: Vec::new(),
            error: None,
        }
    }

    /// Fixture data for tests (UI rendering, CLI selectors). Deliberately
    /// multi-ecosystem, not just Node, with one multi-target project.
    #[cfg(test)]
    pub fn sample() -> Self {
        let t = |project: &str,
                 label: &str,
                 port: Option<u16>,
                 cpu: f32,
                 mem_mb: u64,
                 url: Option<Url>,
                 kind: TargetKind| Target {
            key: match port {
                Some(p) => TargetKey::Port(p),
                None => TargetKey::Command {
                    project: project.into(),
                    label: label.into(),
                    cwd: PathBuf::from("/tmp"),
                },
            },
            kind,
            ports: port.into_iter().collect(),
            anchor: Anchor {
                pid: 0,
                start_time: 0,
            },
            anchor_argv: vec![],
            pids: vec![],
            pid_starts: vec![],
            project: project.into(),
            command_label: label.into(),
            cwd: PathBuf::from("/Users/dev").join(project),
            git_branch: Some("main".into()),
            cpu_pct: cpu,
            mem_bytes: mem_mb * 1024 * 1024,
            url,
            exposed: false,
            container: None,
        };
        let http = |p: u16| {
            Some(Url {
                scheme: UrlScheme::Http,
                value: format!("http://localhost:{p}"),
            })
        };
        let mut targets = vec![
            t(
                "client-portal",
                "next dev",
                Some(3000),
                3.4,
                340,
                http(3000),
                TargetKind::Listener,
            ),
            t(
                "billing-api",
                "uvicorn",
                Some(8000),
                1.1,
                96,
                http(8000),
                TargetKind::Listener,
            ),
            t(
                "worker",
                "celery",
                Some(5555),
                0.4,
                72,
                http(5555),
                TargetKind::Listener,
            ),
            t(
                "client-portal",
                "postgres",
                Some(5432),
                0.2,
                410,
                Some(Url {
                    scheme: UrlScheme::Postgres,
                    value: "postgres://localhost:5432".into(),
                }),
                TargetKind::Listener,
            ),
            t(
                "design-system",
                "tsc --watch",
                None,
                0.8,
                188,
                None,
                TargetKind::Watched,
            ),
        ];
        targets[1].exposed = true; // billing-api on 0.0.0.0 — exercises the LAN badge
        Snapshot {
            seq: 0,
            targets,
            error: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_scheme_web_detection() {
        assert!(UrlScheme::Http.is_web());
        assert!(UrlScheme::Https.is_web());
        assert!(!UrlScheme::Postgres.is_web());
        assert!(!UrlScheme::Redis.is_web());
    }

    #[test]
    fn sample_has_a_multi_target_project() {
        let snap = Snapshot::sample();
        let portal = snap
            .targets
            .iter()
            .filter(|t| t.project == "client-portal")
            .count();
        assert_eq!(portal, 2); // next dev + postgres — exercises grouping
    }
}
