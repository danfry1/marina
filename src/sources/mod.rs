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
}

/// Listening socket -> PID. macOS via `netstat2`; Linux impl later.
pub trait PortSource {
    fn listeners(&mut self) -> Vec<Listener>;
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
    fn listeners(&mut self) -> Vec<Listener> {
        let af = AddressFamilyFlags::IPV4 | AddressFamilyFlags::IPV6;
        let proto = ProtocolFlags::TCP;
        let mut out = Vec::new();
        if let Ok(sockets) = get_sockets_info(af, proto) {
            for si in sockets {
                if let ProtocolSocketInfo::Tcp(tcp) = si.protocol_socket_info {
                    if tcp.state == TcpState::Listen {
                        for pid in si.associated_pids {
                            out.push(Listener {
                                port: tcp.local_port,
                                pid,
                            });
                        }
                    }
                }
            }
        }
        out
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
        let found = Netstat2Ports.listeners();
        assert!(
            found.iter().any(|l| l.port == port),
            "expected our listener on :{port} to be enumerated"
        );
        drop(listener);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn phys_footprint_of_self_is_nonzero() {
        assert!(phys_footprint(std::process::id()).unwrap_or(0) > 0);
    }
}
