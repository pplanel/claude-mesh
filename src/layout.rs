//! Where each Mac keeps its sources and its Claude config.
//!
//! The table comes from `/etc/claude-mesh/hosts.json`, which nix-darwin writes
//! from `modules/darwin/claude/hosts.nix`, the same table `claude-handoff`
//! reads, so the two can't disagree. `CLAUDE_MESH_LAYOUT` points elsewhere;
//! without either, a built-in copy is used (development only).
//!
//! ```json
//! { "caladan": { "src": "/Users/pplanel/src", "config": "/Users/pplanel/.claude" } }
//! ```

use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::PathBuf;

pub const LAYOUT_FILE: &str = "/etc/claude-mesh/hosts.json";

/// One Mac in the mesh.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Host {
    #[serde(skip)]
    pub name: String,
    /// The user's source folder on this Mac.
    pub src: PathBuf,
    /// The Claude config folder on this Mac.
    pub config: PathBuf,
}

/// Every Mac, from the layout file (or the built-in table).
pub fn hosts() -> Result<Vec<Host>, String> {
    let path = std::env::var("CLAUDE_MESH_LAYOUT").unwrap_or_else(|_| LAYOUT_FILE.to_string());
    match std::fs::read(&path) {
        Ok(bytes) => parse(&bytes).map_err(|e| format!("{path}: {e}")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(builtin()),
        Err(e) => Err(format!("{path}: {e}")),
    }
}

fn parse(bytes: &[u8]) -> Result<Vec<Host>, serde_json::Error> {
    let table: BTreeMap<String, Host> = serde_json::from_slice(bytes)?;
    Ok(table
        .into_iter()
        .map(|(name, host)| Host { name, ..host })
        .collect())
}

/// The table as of 2026-10-11, for running outside nix-darwin.
pub fn builtin() -> Vec<Host> {
    let home = PathBuf::from("/Users/pplanel");
    let host = |name: &str, src: &str, config: &str| Host {
        name: name.into(),
        src: home.join(src),
        config: home.join(config),
    };
    vec![
        host("arrakis", "src/pplanel", "src/pplanel/.claude"),
        host("caladan", "src", ".claude"),
        host("imperium", "src", ".claude"),
    ]
}

/// The short hostname of this Mac (`hostname -s`), or `CLAUDE_MESH_HOST` when set.
pub fn this_host_name() -> String {
    if let Ok(name) = std::env::var("CLAUDE_MESH_HOST") {
        return name;
    }
    let out = std::process::Command::new("hostname").arg("-s").output();
    match out {
        Ok(o) => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        Err(_) => String::new(),
    }
}

/// Claude names a project's folder after its path, with `/` and `.` turned into `-`.
pub fn project_key(path: &str) -> String {
    path.chars()
        .map(|c| if c == '/' || c == '.' { '-' } else { c })
        .collect()
}

/// A built-in host by name, for unit tests.
#[cfg(test)]
pub fn find(name: &str) -> Option<Host> {
    builtin().into_iter().find(|h| h.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_key_matches_claude() {
        assert_eq!(
            project_key("/Users/pplanel/src/pplanel/shift-compute"),
            "-Users-pplanel-src-pplanel-shift-compute"
        );
        assert_eq!(
            project_key("/Users/pplanel/.claude"),
            "-Users-pplanel--claude"
        );
    }

    #[test]
    fn layout_file_parses_like_the_builtin_table() {
        let json = r#"{
          "arrakis":  {"src": "/Users/pplanel/src/pplanel", "config": "/Users/pplanel/src/pplanel/.claude"},
          "caladan":  {"src": "/Users/pplanel/src", "config": "/Users/pplanel/.claude"},
          "imperium": {"src": "/Users/pplanel/src", "config": "/Users/pplanel/.claude"}
        }"#;
        assert_eq!(parse(json.as_bytes()).unwrap(), builtin());
    }
}
