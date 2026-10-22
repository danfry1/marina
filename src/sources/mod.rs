//! Data sources behind traits — the boundary where macOS vs. Linux diverge.
//! macOS: `netstat2` (ports) + `sysinfo` (process facts).

use std::collections::HashMap;
use std::path::PathBuf;

use netstat2::{get_sockets_info, AddressFamilyFlags, ProtocolFlags, ProtocolSocketInfo, TcpState};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

/// A listening TCP socket mapped to the PID that holds it.
pub struct Listener {
    pub port: u16,
    pub pid: u32,
    /// Bound to a non-loopback address (0.0.0.0 / ::) — reachable from the LAN.
    pub exposed: bool,
}

/// Per-process facts needed for rollup + resolution.
#[derive(Clone)]
pub struct ProcInfo {
    pub pid: u32,
    pub ppid: Option<u32>,
    pub name: String,
    pub argv: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub cpu_pct: f32,
    pub mem_bytes: u64,
    pub start_time: u64,
    /// Allowlisted agent markers from the process env (never the raw env).
    pub agent: Option<crate::launcher::AgentEnv>,
}

/// Listening socket -> PID. macOS via `netstat2`; Linux impl later.
/// An `Err` is surfaced in the UI — never silently rendered as "no targets".
pub trait PortSource {
    fn listeners(&mut self) -> Result<Vec<Listener>, String>;
}

/// Process facts: cpu/mem/cwd/argv/parent. macOS via `sysinfo`, which also
/// handles the cumulative-CPU delta math across successive refreshes.
pub trait ProcSource {
    fn refresh(&mut self);
    fn procs(&self) -> &HashMap<u32, ProcInfo>;
}

// --- netstat2 ---------------------------------------------------------------

pub struct Netstat2Ports;

impl PortSource for Netstat2Ports {
    fn listeners(&mut self) -> Result<Vec<Listener>, String> {
        let af = AddressFamilyFlags::IPV4 | AddressFamilyFlags::IPV6;
        let proto = ProtocolFlags::TCP;
        let sockets = get_sockets_info(af, proto).map_err(|e| format!("port scan failed: {e}"))?;
        let mut out = Vec::new();
        for si in sockets {
            if let ProtocolSocketInfo::Tcp(tcp) = si.protocol_socket_info {
                if tcp.state == TcpState::Listen {
                    for pid in si.associated_pids {
                        out.push(Listener {
                            port: tcp.local_port,
                            pid,
                            exposed: tcp.local_addr.is_unspecified(),
                        });
                    }
                }
            }
        }
        Ok(out)
    }
}

// --- sysinfo ----------------------------------------------------------------

pub struct SysinfoProcs {
    sys: System,
    map: HashMap<u32, ProcInfo>,
}

impl SysinfoProcs {
    pub fn new() -> Self {
        let mut s = SysinfoProcs {
            sys: System::new(),
            map: HashMap::new(),
        };
        s.refresh(); // baseline so the first CPU delta is meaningful
        s
    }
}

impl Default for SysinfoProcs {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcSource for SysinfoProcs {
    fn refresh(&mut self) {
        self.sys.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::everything(),
        );
        let mut map = HashMap::with_capacity(self.sys.processes().len());
        for (pid, p) in self.sys.processes() {
            let pid = pid.as_u32();
            map.insert(
                pid,
                ProcInfo {
                    pid,
                    ppid: p.parent().map(Pid::as_u32),
                    name: p.name().to_string_lossy().into_owned(),
                    argv: p
                        .cmd()
                        .iter()
                        .map(|s| s.to_string_lossy().into_owned())
                        .collect(),
                    cwd: p.cwd().map(|c| c.to_path_buf()),
                    cpu_pct: p.cpu_usage(),
                    mem_bytes: p.memory(),
                    start_time: p.start_time(),
                    agent: crate::launcher::AgentEnv::from_environ(p.environ()),
                },
            );
        }
        self.map = map;
    }

    fn procs(&self) -> &HashMap<u32, ProcInfo> {
        &self.map
    }
}

/// macOS `phys_footprint` for a pid — the memory figure Activity Monitor shows,
/// which tracks real pressure better than RSS. `None` if unavailable (e.g. a
/// process owned by another user); callers fall back to the RSS-ish value.
#[cfg(target_os = "macos")]
pub fn phys_footprint(pid: u32) -> Option<u64> {
    use libproc::libproc::pid_rusage::{pidrusage, RUsageInfoV2};
    pidrusage::<RUsageInfoV2>(pid as i32)
        .ok()
        .map(|ri| ri.ri_phys_footprint)
}

#[cfg(not(target_os = "macos"))]
pub fn phys_footprint(_pid: u32) -> Option<u64> {
    None
}

/// Last output to / input from a terminal, as unix seconds — the tty device's
/// mtime (written to) and atime (read from: keystrokes). The same signal `w`
/// uses for its IDLE column; no screen contents are read.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TtyActivity {
    pub output: u64,
    pub input: u64,
}

/// Terminal activity for `pid`'s controlling tty, if it has one.
pub fn tty_activity(pid: u32) -> Option<TtyActivity> {
    use std::time::UNIX_EPOCH;
    let meta = std::fs::metadata(tty_path(pid)?).ok()?;
    let secs = |t: std::io::Result<std::time::SystemTime>| {
        t.ok()?.duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs())
    };
    Some(TtyActivity {
        output: secs(meta.modified())?,
        input: secs(meta.accessed())?,
    })
}

#[cfg(target_os = "macos")]
fn tty_path(pid: u32) -> Option<PathBuf> {
    use libproc::libproc::{bsd_info::BSDInfo, proc_pid::pidinfo};
    const NODEV: u32 = u32::MAX;
    let dev = pidinfo::<BSDInfo>(pid as i32, 0).ok()?.e_tdev;
    if dev == NODEV {
        return None;
    }
    // SAFETY: devname returns a pointer to a static buffer (or null); it is
    // copied out immediately and never freed.
    let name = unsafe { libc::devname(dev as libc::dev_t, libc::S_IFCHR) };
    if name.is_null() {
        return None;
    }
    let name = unsafe { std::ffi::CStr::from_ptr(name) }.to_str().ok()?;
    (name.starts_with("tty") && name != "tty??").then(|| PathBuf::from("/dev").join(name))
}

#[cfg(target_os = "linux")]
fn tty_path(pid: u32) -> Option<PathBuf> {
    (0..3).find_map(|fd| {
        let p = std::fs::read_link(format!("/proc/{pid}/fd/{fd}")).ok()?;
        let s = p.to_str()?;
        (s.starts_with("/dev/pts/") || s.starts_with("/dev/tty")).then_some(p)
    })
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn tty_path(_pid: u32) -> Option<PathBuf> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sysinfo_sees_our_own_process() {
        let mut p = SysinfoProcs::new();
        p.refresh();
        let me = p
            .procs()
            .get(&std::process::id())
            .expect("our pid should exist");
        assert!(!me.name.is_empty());
        assert!(me.start_time > 0);
        assert!(!me.argv.is_empty());
    }

    #[test]
    fn netstat_lists_a_bound_listener() {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let found = Netstat2Ports.listeners().expect("port scan should work");
        let ours = found.iter().find(|l| l.port == port);
        assert!(
            ours.is_some(),
            "expected our listener on :{port} to be enumerated"
        );
        // bound to 127.0.0.1, so it must not be flagged LAN-exposed
        assert!(!ours.unwrap().exposed);
        drop(listener);
    }

    #[test]
    fn a_missing_process_has_no_tty_activity() {
        assert_eq!(tty_activity(u32::MAX - 1), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn phys_footprint_of_self_is_nonzero() {
        assert!(phys_footprint(std::process::id()).unwrap_or(0) > 0);
    }
}
