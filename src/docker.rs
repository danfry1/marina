//! Docker container resolution. macOS binds published container ports via a
//! host-side proxy (`com.docker.backend` / `docker-proxy` / OrbStack), so the
//! socket-holding process is uninformative. We ask `docker ps` to map a host
//! port to its container so the row reads `myproject-db-1 · postgres · :5432`.
//!
//! Everything here is fail-silent: no docker, daemon down, or no matching
//! container → an empty map → no docker rows (and no regression). Container
//! cpu/mem isn't captured (it lives in the VM); those columns show `—`.

use std::collections::HashMap;
use std::process::Command;

/// Process names that hold published-container ports on the host.
pub const BINDERS: &[&str] = &[
    "docker-proxy",
    "com.docker.backend",
    "com.docker.vpnkit",
    "vpnkit",
];

pub fn is_binder(proc_name: &str) -> bool {
    BINDERS.iter().any(|b| proc_name.contains(b))
}

/// host port -> (container name, image name). Empty on any failure.
pub fn port_map() -> HashMap<u16, (String, String)> {
    let mut map = HashMap::new();
    let Ok(out) = Command::new("docker")
        .args(["ps", "--format", "{{.Names}}\t{{.Ports}}\t{{.Image}}"])
        .output()
    else {
        return map;
    };
    if !out.status.success() {
        return map;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        let mut f = line.splitn(3, '\t');
        let (Some(name), Some(ports), Some(image)) = (f.next(), f.next(), f.next()) else {
            continue;
        };
        for port in parse_published_ports(ports) {
            map.entry(port)
                .or_insert_with(|| (name.to_string(), image_name(image)));
        }
    }
    map
}

/// Host ports from a docker `Ports` field, e.g.
/// `0.0.0.0:5432->5432/tcp, :::5432->5432/tcp` -> `[5432, 5432]`.
fn parse_published_ports(ports: &str) -> Vec<u16> {
    let mut out = Vec::new();
    for seg in ports.split(',') {
        let seg = seg.trim();
        if let Some(arrow) = seg.find("->") {
            let host = &seg[..arrow];
            if let Some(colon) = host.rfind(':') {
                if let Ok(p) = host[colon + 1..].parse::<u16>() {
                    out.push(p);
                }
            }
        }
    }
    out
}

/// `postgres:16` -> `postgres`; `reg.io:5000/team/app:1.2` -> `app`.
fn image_name(img: &str) -> String {
    let last = img.rsplit('/').next().unwrap_or(img);
    last.split(':').next().unwrap_or(last).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_published_ports() {
        assert_eq!(
            parse_published_ports("0.0.0.0:5432->5432/tcp, :::5432->5432/tcp"),
            vec![5432, 5432]
        );
        assert_eq!(parse_published_ports("0.0.0.0:8080->80/tcp"), vec![8080]);
        assert_eq!(parse_published_ports(""), Vec::<u16>::new());
        // unpublished (no host mapping) -> nothing
        assert_eq!(parse_published_ports("5432/tcp"), Vec::<u16>::new());
    }

    #[test]
    fn image_names() {
        assert_eq!(image_name("postgres:16"), "postgres");
        assert_eq!(image_name("reg.io:5000/team/app:1.2"), "app");
        assert_eq!(image_name("redis"), "redis");
    }

    #[test]
    fn detects_docker_binders() {
        assert!(is_binder("com.docker.backend"));
        assert!(is_binder("docker-proxy"));
        assert!(is_binder("com.docker.vpnkit"));
        assert!(!is_binder("node"));
        assert!(!is_binder("postgres"));
    }
}
