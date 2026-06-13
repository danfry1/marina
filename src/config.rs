//! User config at `~/.config/marina/config.toml`. Optional — absent or
//! malformed config falls back to built-in defaults (a warning goes to stderr).
//!
//! ```toml
//! [[rule]]              # classify a command -> label (+ optional url)
//! match_cmd = "next( |$)|next dev"
//! label     = "next dev"
//! url       = "http://localhost:{port}"
//!
//! [[watch]]             # port-less workloads to surface as their own targets
//! match_cmd = "tsc.*--watch|jest|vitest"
//! label     = "watcher"
//!
//! [[override]]          # pin a stubborn target regardless of heuristics
//! match_port = 3000
//! match_cmd  = "node"
//! project    = "client-portal"
//!
//! [[group]]             # bundle targets that don't share a cwd (app + its db)
//! name    = "client-portal"
//! members = [3000, 5432, "worker"]   # ports, project names, or command labels
//! ```

use std::path::PathBuf;

use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
pub struct ConfigFile {
    #[serde(default)]
    pub rule: Vec<RuleCfg>,
    #[serde(default)]
    pub watch: Vec<WatchCfg>,
    #[serde(default, rename = "override")]
    pub overrides: Vec<OverrideCfg>,
    #[serde(default)]
    pub group: Vec<GroupCfg>,
}

#[derive(Debug, Deserialize)]
pub struct RuleCfg {
    pub match_cmd: String,
    pub label: String,
    pub url: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct WatchCfg {
    pub match_cmd: String,
    pub label: String,
}

#[derive(Debug, Deserialize)]
pub struct OverrideCfg {
    pub match_port: Option<u16>,
    pub match_cmd: Option<String>,
    pub project: Option<String>,
    pub label: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct GroupCfg {
    pub name: String,
    /// Members are selectors: a port (`3000`), a project name, or a command label.
    #[serde(default)]
    pub members: Vec<toml::Value>,
}

pub fn config_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| {
        PathBuf::from(h)
            .join(".config")
            .join("marina")
            .join("config.toml")
    })
}

/// Load + parse the config file. Returns an empty config (and warns) on a parse
/// error, and a silent empty config when the file is simply absent.
pub fn load() -> ConfigFile {
    let Some(path) = config_path() else {
        return ConfigFile::default();
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return ConfigFile::default();
    };
    match toml::from_str(&text) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("marina: ignoring {}: {e}", path.display());
            ConfigFile::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_sections() {
        let cfg: ConfigFile = toml::from_str(
            r#"
            [[rule]]
            match_cmd = "next dev"
            label = "next dev"
            url = "http://localhost:{port}"

            [[watch]]
            match_cmd = "jest"
            label = "jest"

            [[override]]
            match_port = 3000
            project = "client-portal"

            [[group]]
            name = "client-portal"
            members = [3000, 5432, "worker"]
            "#,
        )
        .unwrap();
        assert_eq!(cfg.rule.len(), 1);
        assert_eq!(cfg.rule[0].url.as_deref(), Some("http://localhost:{port}"));
        assert_eq!(cfg.watch.len(), 1);
        assert_eq!(cfg.overrides[0].match_port, Some(3000));
        assert_eq!(cfg.group[0].name, "client-portal");
        assert_eq!(cfg.group[0].members.len(), 3);
    }

    #[test]
    fn empty_config_is_default() {
        let cfg: ConfigFile = toml::from_str("").unwrap();
        assert!(cfg.rule.is_empty() && cfg.group.is_empty());
    }
}
