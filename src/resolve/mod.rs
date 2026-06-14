//! Language- and framework-agnostic resolution: raw process -> human fields
//! (project, command label, typed URL). See DESIGN.md "`resolve`".
//!
//! Built-in defaults live here as free functions; `Resolver` (below) layers the
//! user's `config.toml` rules on top. Filesystem resolution (project root/name)
//! is cached in the sampler. Pipeline: project root -> name -> command label
//! (subtree-aware, sees through pnpm/npm/node wrappers) -> typed URL.

use std::fs;
use std::path::{Path, PathBuf};

use regex::Regex;

use crate::config;
use crate::model::{Url, UrlScheme};

const MARKERS: &[&str] = &[
    "package.json",
    "Cargo.toml",
    "go.mod",
    "pyproject.toml",
    "setup.py",
    "Pipfile",
    "Gemfile",
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
    "mix.exs",
    "composer.json",
];

/// Nearest project marker above `start` (any manifest, or `.git` as fallback).
pub fn project_root(start: &Path) -> Option<PathBuf> {
    let mut dir = Some(start);
    while let Some(d) = dir {
        if MARKERS.iter().any(|m| d.join(m).exists()) || d.join(".git").exists() {
            return Some(d.to_path_buf());
        }
        dir = d.parent();
    }
    None
}

/// manifest name -> dir basename.
pub fn project_name(root: &Path) -> String {
    name_from_manifest(root)
        .or_else(|| root.file_name().map(|s| s.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "?".into())
}

fn name_from_manifest(root: &Path) -> Option<String> {
    if let Ok(s) = fs::read_to_string(root.join("package.json")) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&s) {
            if let Some(n) = v.get("name").and_then(|x| x.as_str()) {
                return Some(n.to_string());
            }
        }
    }
    if let Ok(s) = fs::read_to_string(root.join("Cargo.toml")) {
        if let Ok(v) = s.parse::<toml::Value>() {
            if let Some(n) = v
                .get("package")
                .and_then(|p| p.get("name"))
                .and_then(|x| x.as_str())
            {
                return Some(n.to_string());
            }
        }
    }
    if let Ok(s) = fs::read_to_string(root.join("pyproject.toml")) {
        if let Ok(v) = s.parse::<toml::Value>() {
            if let Some(n) = v
                .get("project")
                .and_then(|p| p.get("name"))
                .and_then(|x| x.as_str())
                .or_else(|| {
                    v.get("tool")
                        .and_then(|t| t.get("poetry"))
                        .and_then(|p| p.get("name"))
                        .and_then(|x| x.as_str())
                })
            {
                return Some(n.to_string());
            }
        }
    }
    if let Ok(s) = fs::read_to_string(root.join("go.mod")) {
        for line in s.lines() {
            if let Some(rest) = line.trim().strip_prefix("module ") {
                let rest = rest.trim();
                return Some(rest.rsplit('/').next().unwrap_or(rest).to_string());
            }
        }
    }
    None
}

/// Multi-token commands matched against the joined argv.
const MULTIWORD: &[(&str, &str)] = &[
    ("next dev", "next dev"),
    ("manage.py runserver", "django runserver"),
    ("mix phx.server", "phoenix"),
    ("artisan serve", "php artisan serve"),
    ("dotnet watch", "dotnet watch"),
    ("dotnet run", "dotnet run"),
    ("cargo run", "cargo run"),
    ("rails server", "rails s"),
    ("rails s", "rails s"),
];

/// Recognized dev tools by program basename (a `.js`/`.cjs`/`.mjs` stem counts).
const TOOLS: &[(&str, &str)] = &[
    ("vite", "vite"),
    ("next", "next dev"),
    ("nuxt", "nuxt"),
    ("astro", "astro"),
    ("remix", "remix"),
    ("webpack", "webpack"),
    ("esbuild", "esbuild"),
    ("parcel", "parcel"),
    ("storybook", "storybook"),
    ("uvicorn", "uvicorn"),
    ("gunicorn", "gunicorn"),
    ("hypercorn", "hypercorn"),
    ("flask", "flask"),
    ("celery", "celery"),
    ("streamlit", "streamlit"),
    ("puma", "puma"),
    ("unicorn", "unicorn"),
    ("sidekiq", "sidekiq"),
    ("postgres", "postgres"),
    ("postmaster", "postgres"),
    ("redis-server", "redis"),
    ("mysqld", "mysql"),
    ("mongod", "mongodb"),
    ("memcached", "memcached"),
    ("nodemon", "nodemon"),
];

/// Launchers that are not themselves the interesting command — we look past them
/// (usually into the process subtree) to find the real tool.
const WRAPPERS: &[&str] = &[
    "pnpm",
    "npm",
    "npx",
    "yarn",
    "bun",
    "node",
    "deno",
    "ts-node",
    "tsx",
    "dotenv",
    "dotenvx",
    "cross-env",
    "concurrently",
    "turbo",
    "nx",
    "make",
    "env",
];

fn strip_js(b: &str) -> &str {
    b.strip_suffix(".js")
        .or_else(|| b.strip_suffix(".cjs"))
        .or_else(|| b.strip_suffix(".mjs"))
        .unwrap_or(b)
}

fn is_wrapper(name: &str) -> bool {
    WRAPPERS.contains(&strip_js(name))
}

/// A recognized dev tool from one process's argv, if any.
pub fn known_tool(argv: &[String]) -> Option<String> {
    let joined = argv.join(" ");
    for (needle, label) in MULTIWORD {
        if joined.contains(needle) {
            return Some((*label).into());
        }
    }
    for el in argv {
        let b = basename(el);
        let stem = strip_js(&b);
        if let Some((_, label)) = TOOLS.iter().find(|(n, _)| *n == stem) {
            return Some((*label).into());
        }
    }
    None
}

/// Friendly label for a target, scanning its whole subtree so wrapper launchers
/// (pnpm/npm/yarn/npx/node) resolve to the real tool running underneath.
pub fn label_from_subtree(argvs: &[Vec<String>]) -> String {
    // 1. A recognized tool anywhere in the subtree wins.
    for argv in argvs {
        if let Some(label) = known_tool(argv) {
            return label;
        }
    }
    // 2. Otherwise the first non-wrapper, non-shell program.
    for argv in argvs {
        if let Some(prog) = argv.first() {
            let b = basename(prog);
            if !is_wrapper(&b) && !is_shell(&b) {
                return single_label(argv);
            }
        }
    }
    // 3. Fall back to the first process.
    argvs
        .first()
        .map(|a| single_label(a))
        .unwrap_or_else(|| "?".into())
}

/// Label for a single process when nothing better is recognized.
fn single_label(argv: &[String]) -> String {
    if let (Some(first), Some(second)) = (argv.first(), argv.get(1)) {
        let fb = basename(first);
        if (fb == "node" || is_wrapper(&fb)) && !second.starts_with('-') {
            return format!("{} {}", fb, basename(second));
        }
    }
    argv.first()
        .map(|a| basename(a))
        .unwrap_or_else(|| "?".into())
}

/// Port-less watcher detection. Returns a label when argv looks like a watcher.
///
/// Matches on standalone argv tokens / program basenames — NOT substrings of the
/// joined command line. A shell running `-c "<script mentioning --watch>"` keeps
/// the whole script in one argv element, so it won't be misread as a watcher.
pub fn watcher_label(argv: &[String]) -> Option<String> {
    let prog = |needle: &str| argv.iter().any(|a| basename(a) == needle);
    let token = |needle: &str| argv.iter().any(|a| a == needle);
    let watch_flag = token("--watch") || token("-w");

    if prog("tsc") && watch_flag {
        return Some("tsc --watch".into());
    }
    if prog("vitest") {
        return Some("vitest".into());
    }
    if prog("jest") {
        return Some("jest".into());
    }
    if prog("cargo-watch") || (prog("cargo") && token("watch")) {
        return Some("cargo watch".into());
    }
    if prog("tailwindcss") && watch_flag {
        return Some("tailwind --watch".into());
    }
    if prog("nodemon") {
        return Some("nodemon".into());
    }
    if watch_flag {
        return Some("watch".into());
    }
    None
}

/// Common interactive shells — never a watcher or a meaningful command anchor.
pub fn is_shell(name: &str) -> bool {
    matches!(
        name.trim_start_matches('-'),
        "zsh" | "bash" | "sh" | "fish" | "dash" | "ksh" | "tcsh"
    )
}

/// Typed default URL for a listener target.
pub fn default_url(label: &str, port: u16) -> Option<Url> {
    let (scheme, value) = match label {
        "postgres" => (UrlScheme::Postgres, format!("postgres://localhost:{port}")),
        "redis" => (UrlScheme::Redis, format!("redis://localhost:{port}")),
        "mysql" => (UrlScheme::Mysql, format!("mysql://localhost:{port}")),
        "mongodb" => (UrlScheme::Other, format!("mongodb://localhost:{port}")),
        _ => (UrlScheme::Http, format!("http://localhost:{port}")),
    };
    Some(Url { scheme, value })
}

/// Current branch from `<root>/.git/HEAD`, if `root` is a git worktree.
pub fn git_branch(root: &Path) -> Option<String> {
    let head = fs::read_to_string(root.join(".git/HEAD")).ok()?;
    head.trim()
        .strip_prefix("ref: refs/heads/")
        .map(|s| s.to_string())
}

fn basename(s: &str) -> String {
    s.rsplit(['/', '\\']).next().unwrap_or(s).to_string()
}

/// Compiled config rules layered over the built-in defaults. Config rules win;
/// built-ins (the subtree-aware `label_from_subtree` / `watcher_label`) are the
/// fallback. `Send` so it can move into the sampler thread.
pub struct Resolver {
    cmd_rules: Vec<CmdRule>,
    watch_rules: Vec<WatchRule>,
    overrides: Vec<OverrideRule>,
    groups: Vec<GroupRule>,
}

struct GroupRule {
    name: String,
    selectors: Vec<GroupSelector>,
}

enum GroupSelector {
    Port(u16),
    Text(String), // lowercased; matched against project or command label
}

impl GroupSelector {
    fn matches(&self, ports: &[u16], project: &str, label: &str) -> bool {
        match self {
            GroupSelector::Port(p) => ports.contains(p),
            GroupSelector::Text(t) => {
                project.to_lowercase().contains(t) || label.to_lowercase().contains(t)
            }
        }
    }
}

fn parse_member(v: toml::Value) -> Option<GroupSelector> {
    match v {
        toml::Value::Integer(n) if (0..=65535).contains(&n) => Some(GroupSelector::Port(n as u16)),
        toml::Value::String(s) => {
            let trimmed = s.trim_start_matches(':');
            match trimmed.parse::<u16>() {
                Ok(p) => Some(GroupSelector::Port(p)),
                Err(_) => Some(GroupSelector::Text(s.to_lowercase())),
            }
        }
        _ => None,
    }
}

struct CmdRule {
    re: Regex,
    label: String,
    url: Option<String>,
}

struct WatchRule {
    re: Regex,
    label: String,
}

struct OverrideRule {
    port: Option<u16>,
    cmd: Option<Regex>,
    project: Option<String>,
    label: Option<String>,
}

impl Resolver {
    /// A resolver with no config rules (built-in defaults only). Used as a
    /// config-free baseline and in tests.
    pub fn empty() -> Self {
        Resolver {
            cmd_rules: vec![],
            watch_rules: vec![],
            overrides: vec![],
            groups: vec![],
        }
    }

    /// Load + compile the user config (built-ins always remain as fallback).
    pub fn load() -> Self {
        let cfg = config::load();
        let compile = |s: &str| match Regex::new(s) {
            Ok(re) => Some(re),
            Err(e) => {
                eprintln!("marina: ignoring bad regex {s:?}: {e}");
                None
            }
        };
        Resolver {
            cmd_rules: cfg
                .rule
                .into_iter()
                .filter_map(|r| {
                    compile(&r.match_cmd).map(|re| CmdRule {
                        re,
                        label: r.label,
                        url: r.url,
                    })
                })
                .collect(),
            watch_rules: cfg
                .watch
                .into_iter()
                .filter_map(|w| compile(&w.match_cmd).map(|re| WatchRule { re, label: w.label }))
                .collect(),
            overrides: cfg
                .overrides
                .into_iter()
                .map(|o| OverrideRule {
                    port: o.match_port,
                    cmd: o.match_cmd.as_deref().and_then(compile),
                    project: o.project,
                    label: o.label,
                })
                .collect(),
            groups: cfg
                .group
                .into_iter()
                .map(|g| GroupRule {
                    name: g.name,
                    selectors: g.members.into_iter().filter_map(parse_member).collect(),
                })
                .collect(),
        }
    }

    /// The declared group a target belongs to, if any — overrides its project
    /// for grouping/kill purposes (bundles e.g. an app with its database).
    pub fn group_name(&self, ports: &[u16], project: &str, label: &str) -> Option<String> {
        self.groups
            .iter()
            .find(|g| g.selectors.iter().any(|s| s.matches(ports, project, label)))
            .map(|g| g.name.clone())
    }

    /// Label + typed URL for a target: config command rules first, then the
    /// built-in subtree-aware classifier.
    pub fn label_and_url(&self, argvs: &[Vec<String>], port: Option<u16>) -> (String, Option<Url>) {
        for argv in argvs {
            let joined = argv.join(" ");
            for rule in &self.cmd_rules {
                if rule.re.is_match(&joined) {
                    let url = match &rule.url {
                        Some(tpl) => {
                            let p = port.map(|p| p.to_string()).unwrap_or_default();
                            let value = tpl.replace("{port}", &p);
                            Some(Url {
                                scheme: scheme_from_url(&value),
                                value,
                            })
                        }
                        None => port.and_then(|p| default_url(&rule.label, p)),
                    };
                    return (rule.label.clone(), url);
                }
            }
        }
        let label = label_from_subtree(argvs);
        let url = port.and_then(|p| default_url(&label, p));
        (label, url)
    }

    /// Watcher label: config watch rules first, then the built-in detector.
    pub fn watcher_label(&self, argv: &[String]) -> Option<String> {
        let joined = argv.join(" ");
        for w in &self.watch_rules {
            if w.re.is_match(&joined) {
                return Some(w.label.clone());
            }
        }
        watcher_label(argv)
    }

    /// Apply the first matching `[[override]]` to (project, label) in place.
    pub fn apply_override(
        &self,
        port: Option<u16>,
        argv_joined: &str,
        project: &mut String,
        label: &mut String,
    ) {
        for o in &self.overrides {
            let port_ok = o.port.is_none_or(|p| port == Some(p));
            let cmd_ok = o.cmd.as_ref().is_none_or(|re| re.is_match(argv_joined));
            if port_ok && cmd_ok {
                if let Some(p) = &o.project {
                    *project = p.clone();
                }
                if let Some(l) = &o.label {
                    *label = l.clone();
                }
                return;
            }
        }
    }
}

fn scheme_from_url(v: &str) -> UrlScheme {
    if v.starts_with("https://") {
        UrlScheme::Https
    } else if v.starts_with("http://") {
        UrlScheme::Http
    } else if v.starts_with("postgres") {
        UrlScheme::Postgres
    } else if v.starts_with("redis") {
        UrlScheme::Redis
    } else if v.starts_with("mysql") {
        UrlScheme::Mysql
    } else {
        UrlScheme::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declared_group_matches_by_port_and_text() {
        let r = Resolver {
            cmd_rules: vec![],
            watch_rules: vec![],
            overrides: vec![],
            groups: vec![GroupRule {
                name: "client-portal".into(),
                selectors: vec![
                    GroupSelector::Port(5432),
                    GroupSelector::Text("worker".into()),
                ],
            }],
        };
        // postgres on :5432 -> grouped under client-portal by port
        assert_eq!(
            r.group_name(&[5432], "somedb", "postgres").as_deref(),
            Some("client-portal")
        );
        // a "worker" command -> grouped by label text
        assert_eq!(
            r.group_name(&[], "anything", "worker").as_deref(),
            Some("client-portal")
        );
        // unrelated target -> no group
        assert_eq!(r.group_name(&[3000], "other", "next dev"), None);
    }

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn watcher_label_needs_a_standalone_flag_not_a_substring() {
        assert_eq!(
            watcher_label(&argv(&["node", "--watch", "noop.js"])).as_deref(),
            Some("watch")
        );
        assert_eq!(
            watcher_label(&argv(&["node", "/x/.bin/tsc", "--watch"])).as_deref(),
            Some("tsc --watch")
        );
        // regression: a shell whose -c script merely *mentions* --watch must NOT match
        assert_eq!(
            watcher_label(&argv(&["/bin/zsh", "-c", "cargo run -- --watch x"])),
            None
        );
        assert_eq!(
            watcher_label(&argv(&["cargo", "watch", "-x", "run"])).as_deref(),
            Some("cargo watch")
        );
    }

    #[test]
    fn label_sees_through_wrappers_to_the_real_tool() {
        // pnpm parent + vite child in the subtree -> "vite"
        let tree = vec![
            argv(&["node", "/x/node_modules/.bin/pnpm", "dev"]),
            argv(&["node", "/x/node_modules/vite/bin/vite.js"]),
        ];
        assert_eq!(label_from_subtree(&tree), "vite");
        assert_eq!(
            label_from_subtree(&[argv(&["node", "/x/.bin/next", "dev"])]),
            "next dev"
        );
        // unknown -> node-script fallback
        assert_eq!(
            label_from_subtree(&[argv(&["node", "server.js"])]),
            "node server.js"
        );
    }

    #[test]
    fn is_shell_distinguishes_shells_from_tools() {
        assert!(is_shell("zsh"));
        assert!(is_shell("-bash"));
        assert!(!is_shell("node"));
    }

    #[test]
    fn url_schemes_are_typed() {
        assert!(default_url("postgres", 5432).unwrap().scheme == UrlScheme::Postgres);
        assert!(default_url("next dev", 3000).unwrap().scheme == UrlScheme::Http);
        assert!(scheme_from_url("redis://localhost:6379") == UrlScheme::Redis);
        assert!(scheme_from_url("http://localhost:3000") == UrlScheme::Http);
    }

    #[test]
    fn override_pins_project() {
        let r = Resolver {
            cmd_rules: vec![],
            watch_rules: vec![],
            overrides: vec![OverrideRule {
                port: Some(3000),
                cmd: None,
                project: Some("pinned".into()),
                label: None,
            }],
            groups: vec![],
        };
        let mut project = "orig".to_string();
        let mut label = "node".to_string();
        r.apply_override(Some(3000), "node server.js", &mut project, &mut label);
        assert_eq!(project, "pinned");
        assert_eq!(label, "node"); // untouched
                                   // non-matching port leaves it alone
        let mut p2 = "orig".to_string();
        r.apply_override(Some(9999), "node", &mut p2, &mut label);
        assert_eq!(p2, "orig");
    }

    #[test]
    fn project_root_and_name_from_manifest() {
        let dir = std::env::temp_dir().join(format!("marina-root-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("package.json"), r#"{"name":"acme"}"#).unwrap();
        // walks up from sub/ to the marker dir
        assert_eq!(project_root(&dir.join("sub")), Some(dir.clone()));
        assert_eq!(project_name(&dir), "acme");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn default_url_picks_the_right_scheme() {
        assert!(default_url("redis", 6379).unwrap().scheme == UrlScheme::Redis);
        assert!(default_url("mysql", 3306).unwrap().scheme == UrlScheme::Mysql);
        assert!(default_url("mongodb", 27017).unwrap().scheme == UrlScheme::Other);
        assert!(default_url("vite", 5173).unwrap().scheme == UrlScheme::Http);
    }

    #[test]
    fn known_tool_is_none_for_unrecognized() {
        assert!(known_tool(&argv(&["node", "randomthing.js"])).is_none());
        assert!(known_tool(&argv(&["./my-custom-binary"])).is_none());
    }

    #[test]
    fn config_command_rule_overrides_builtin_label() {
        let r = Resolver {
            cmd_rules: vec![CmdRule {
                re: Regex::new("my-special-server").unwrap(),
                label: "special".into(),
                url: Some("http://localhost:{port}/health".into()),
            }],
            watch_rules: vec![],
            overrides: vec![],
            groups: vec![],
        };
        let (label, url) = r.label_and_url(&[argv(&["node", "my-special-server.js"])], Some(8080));
        assert_eq!(label, "special");
        assert_eq!(url.unwrap().value, "http://localhost:8080/health");
    }
}
