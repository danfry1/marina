//! `marina port`: a stable, collision-free dev port per project directory.
//!
//! Parallel agents in parallel worktrees all reach for `:3000`. `marina port`
//! hands each project root (each worktree is its own root; each package in a
//! monorepo too) its own port, the same one every time, so
//! `pnpm dev --port $(marina port)` just works everywhere at once.
//!
//! - **Stable**: the first pick is a hash of the key (root + optional service
//!   name), and the assignment is remembered in
//!   `$XDG_STATE_HOME/marina/ports.json` — re-running returns the same port.
//! - **Collision-free**: assignments are made under an exclusive file lock, so
//!   two agents asking at once never get the same port; a port held by some
//!   *other* project's process is skipped (and an assignment that has been
//!   taken over moves). A port held by this project's own server is fine —
//!   that's the server the port was for.
//! - **Self-cleaning**: assignments whose directory no longer exists (a removed
//!   worktree) are dropped on the next call.
//!
//! The allocation itself ([`allocate`]) is pure — I/O, locking and the
//! "is this port busy?" probe are injected by the CLI.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::ops::RangeInclusive;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Default range: clear of the ubiquitous 3000 (left to the main checkout /
/// whoever runs a server by hand) and of most well-known service ports.
pub const DEFAULT_RANGE: RangeInclusive<u16> = 3100..=3999;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Lease {
    pub port: u16,
    /// Project root (a worktree, or a package within one).
    pub root: PathBuf,
    /// Optional service name for several ports per root (`web`, `api`).
    pub name: Option<String>,
    pub assigned_at: u64,
    pub last_used: u64,
}

impl Lease {
    fn is_for(&self, root: &Path, name: Option<&str>) -> bool {
        self.root == root && self.name.as_deref() == name
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Leases {
    #[serde(default)]
    pub leases: Vec<Lease>,
}

#[derive(Debug, PartialEq)]
pub enum Outcome {
    /// Already assigned and still usable.
    Existing,
    /// Newly assigned.
    New,
    /// The old port was taken by another project's process; moved.
    Moved { from: u16 },
}

#[derive(Debug, PartialEq)]
pub enum AllocError {
    /// Every port in the range is assigned or busy.
    Exhausted,
}

/// FNV-1a — stable across Rust versions and machines (std's hasher is not),
/// so a key's first-choice port never changes.
fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

fn key_of(root: &Path, name: Option<&str>) -> String {
    match name {
        Some(n) => format!("{}#{n}", root.display()),
        None => root.display().to_string(),
    }
}

/// Assign (or recall) the port for `root` + `name`.
///
/// `foreign_busy(p)`: a process *not* belonging to `root` is listening on `p`.
/// `root_exists(r)`: the directory is still there (else its lease is dropped).
pub fn allocate(
    leases: &mut Leases,
    root: &Path,
    name: Option<&str>,
    range: RangeInclusive<u16>,
    now: u64,
    foreign_busy: impl Fn(u16) -> bool,
    root_exists: impl Fn(&Path) -> bool,
) -> Result<(u16, Outcome), AllocError> {
    leases.leases.retain(|l| root_exists(&l.root));

    let mut moved_from = None;
    if let Some(i) = leases.leases.iter().position(|l| l.is_for(root, name)) {
        let l = &mut leases.leases[i];
        if range.contains(&l.port) && !foreign_busy(l.port) {
            l.last_used = now;
            return Ok((l.port, Outcome::Existing));
        }
        // taken over by someone else (or outside a changed range): move
        moved_from = Some(l.port);
        leases.leases.remove(i);
    }

    let (lo, hi) = (*range.start(), *range.end());
    if lo > hi {
        return Err(AllocError::Exhausted);
    }
    let len = u64::from(hi - lo) + 1;
    let first = fnv1a(&key_of(root, name)) % len;
    for i in 0..len {
        let port = lo + ((first + i) % len) as u16;
        if Some(port) == moved_from
            || leases.leases.iter().any(|l| l.port == port)
            || foreign_busy(port)
        {
            continue;
        }
        leases.leases.push(Lease {
            port,
            root: root.to_path_buf(),
            name: name.map(str::to_string),
            assigned_at: now,
            last_used: now,
        });
        let outcome = match moved_from {
            Some(from) => Outcome::Moved { from },
            None => Outcome::New,
        };
        return Ok((port, outcome));
    }
    Err(AllocError::Exhausted)
}

/// Drop the lease for `root` + `name`. Returns the port it held.
pub fn release(leases: &mut Leases, root: &Path, name: Option<&str>) -> Option<u16> {
    let i = leases.leases.iter().position(|l| l.is_for(root, name))?;
    Some(leases.leases.remove(i).port)
}

/// A service name for `marina port <name>`: short, `[A-Za-z0-9_.-]`.
pub fn valid_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 32
        && n.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

// --- storage ----------------------------------------------------------------

/// The lease file, held open under an exclusive `flock` for the whole
/// read-modify-write — concurrent `marina port` calls serialize here.
pub struct Store {
    path: PathBuf,
    _lock: File,
}

impl Store {
    pub fn open() -> std::io::Result<Store> {
        let dir = crate::logs::state_dir().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "no $HOME / $XDG_STATE_HOME")
        })?;
        Store::open_in(&dir)
    }

    pub fn open_in(dir: &Path) -> std::io::Result<Store> {
        fs::create_dir_all(dir)?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join("ports.lock"))?;
        // Blocks until any other marina holding the lock is done.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Store {
            path: dir.join("ports.json"),
            _lock: lock,
        })
    }

    /// Absent or unreadable file → no leases (a corrupt file must not wedge
    /// the command; it is rewritten on the next save).
    pub fn load(&self) -> Leases {
        fs::read_to_string(&self.path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// Atomic replace: write a sibling temp file, then rename over.
    pub fn save(&self, leases: &Leases) -> std::io::Result<()> {
        let tmp = self.path.with_extension("json.tmp");
        let mut f = File::create(&tmp)?;
        f.write_all(serde_json::to_string_pretty(leases)?.as_bytes())?;
        f.sync_all()?;
        fs::rename(&tmp, &self.path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const R: RangeInclusive<u16> = 3100..=3999;
    fn none(_: u16) -> bool {
        false
    }
    fn exists(_: &Path) -> bool {
        true
    }

    #[test]
    fn same_key_gets_the_same_port_every_time() {
        let mut l = Leases::default();
        let root = Path::new("/w/app-feat-a");
        let (p1, o1) = allocate(&mut l, root, None, R, 1, none, exists).unwrap();
        let (p2, o2) = allocate(&mut l, root, None, R, 2, none, exists).unwrap();
        assert_eq!(o1, Outcome::New);
        assert_eq!((p2, o2), (p1, Outcome::Existing));
        assert!(R.contains(&p1));
        assert_eq!(l.leases[0].last_used, 2);
        // the first choice is a pure function of the key (stable across runs)
        let mut fresh = Leases::default();
        assert_eq!(
            allocate(&mut fresh, root, None, R, 9, none, exists)
                .unwrap()
                .0,
            p1
        );
    }

    #[test]
    fn worktrees_and_service_names_never_share_a_port() {
        let mut l = Leases::default();
        let mut seen = std::collections::HashSet::new();
        for i in 0..200 {
            let root = PathBuf::from(format!("/w/wt-{i}"));
            for name in [None, Some("api")] {
                let (p, _) = allocate(&mut l, &root, name, R, 0, none, exists).unwrap();
                assert!(seen.insert(p), "port {p} handed out twice");
            }
        }
    }

    #[test]
    fn a_port_held_by_another_project_is_skipped_or_moved() {
        let mut l = Leases::default();
        let root = Path::new("/w/a");
        let (p, _) = allocate(&mut l, root, None, R, 0, none, exists).unwrap();
        // someone else now listens on our port -> move, and say from where
        let (q, o) = allocate(&mut l, root, None, R, 1, |x| x == p, exists).unwrap();
        assert_ne!(q, p);
        assert_eq!(o, Outcome::Moved { from: p });
        assert_eq!(l.leases.len(), 1);
    }

    #[test]
    fn leases_for_deleted_worktrees_are_dropped() {
        let mut l = Leases::default();
        allocate(&mut l, Path::new("/gone"), None, R, 0, none, exists).unwrap();
        allocate(&mut l, Path::new("/here"), None, R, 0, none, |r| {
            r == Path::new("/here")
        })
        .unwrap();
        assert_eq!(l.leases.len(), 1);
        assert_eq!(l.leases[0].root, Path::new("/here"));
    }

    #[test]
    fn a_full_range_is_an_error_not_a_duplicate() {
        let mut l = Leases::default();
        let tiny = 3100..=3101;
        allocate(&mut l, Path::new("/a"), None, tiny.clone(), 0, none, exists).unwrap();
        allocate(&mut l, Path::new("/b"), None, tiny.clone(), 0, none, exists).unwrap();
        assert_eq!(
            allocate(&mut l, Path::new("/c"), None, tiny, 0, none, exists),
            Err(AllocError::Exhausted)
        );
    }

    #[test]
    fn release_frees_the_port_for_reuse() {
        let mut l = Leases::default();
        let (p, _) = allocate(&mut l, Path::new("/a"), Some("web"), R, 0, none, exists).unwrap();
        assert_eq!(release(&mut l, Path::new("/a"), Some("web")), Some(p));
        assert_eq!(release(&mut l, Path::new("/a"), Some("web")), None);
        assert!(l.leases.is_empty());
    }

    #[test]
    fn names_are_validated() {
        assert!(valid_name("api") && valid_name("web-2") && valid_name("svc.v1"));
        assert!(!valid_name("") && !valid_name("a/b") && !valid_name("$(x)"));
    }

    #[test]
    fn store_round_trips_and_serializes_concurrent_callers() {
        let dir = std::env::temp_dir().join(format!("marina-ports-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        // 8 threads each allocate for their own root under the lock
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let dir = dir.clone();
                std::thread::spawn(move || {
                    let store = Store::open_in(&dir).unwrap();
                    let mut l = store.load();
                    let root = PathBuf::from(format!("/w/t{i}"));
                    let (p, _) = allocate(&mut l, &root, None, R, 0, none, exists).unwrap();
                    store.save(&l).unwrap();
                    p
                })
            })
            .collect();
        let mut ports: Vec<u16> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let store = Store::open_in(&dir).unwrap();
        assert_eq!(store.load().leases.len(), 8, "no lost updates");
        ports.sort_unstable();
        ports.dedup();
        assert_eq!(ports.len(), 8, "no duplicates under concurrency");
        drop(store);
        let _ = fs::remove_dir_all(&dir);
    }
}
