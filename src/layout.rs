//! Where each Mac keeps its sources and its Claude config, and how the mesh
//! reaches each peer.
//!
//! The table mirrors `claude-handoff`'s `layout()`: arrakis keeps the personal
//! Claude account under its source folder (`claude-personal` sets
//! `CLAUDE_CONFIG_DIR`), the other Macs use `~/.claude`.

use std::path::PathBuf;

/// The TCP port `claude-mesh serve` listens on, on every Mac. airshell-proxy
/// advertises it to the mesh as the QUIC service `<host>-claudesync`.
pub const SERVE_PORT: u16 = 7480;

/// The Bonjour service type the airshell service and forwards use.
pub const SERVICE_TYPE: &str = "_claudesync._udp";

/// One Mac in the mesh.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Host {
    pub name: String,
    /// The user's source folder on this Mac.
    pub src: PathBuf,
    /// The Claude config folder on this Mac.
    pub config: PathBuf,
    /// The local port where *other* Macs reach this one through an airshell
    /// `[[forward]]`. Each peer gets its own port so every Mac can forward to
    /// every other at once.
    pub forward_port: u16,
}

/// The three Macs. Kept in one place so every node agrees.
pub fn hosts() -> Vec<Host> {
    let home = PathBuf::from("/Users/pplanel");
    vec![
        Host {
            name: "arrakis".into(),
            src: home.join("src/pplanel"),
            config: home.join("src/pplanel/.claude"),
            forward_port: 7481,
        },
        Host {
            name: "caladan".into(),
            src: home.join("src"),
            config: home.join(".claude"),
            forward_port: 7482,
        },
        Host {
            name: "imperium".into(),
            src: home.join("src"),
            config: home.join(".claude"),
            forward_port: 7483,
        },
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

pub fn find(name: &str) -> Option<Host> {
    hosts().into_iter().find(|h| h.name == name)
}

/// Every host except `name`.
pub fn peers_of(name: &str) -> Vec<Host> {
    hosts().into_iter().filter(|h| h.name != name).collect()
}

/// Claude names a project's folder after its path, with `/` and `.` turned into `-`.
pub fn project_key(path: &str) -> String {
    path.chars()
        .map(|c| if c == '/' || c == '.' { '-' } else { c })
        .collect()
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
    fn every_host_has_a_distinct_forward_port() {
        let mut ports: Vec<u16> = hosts().iter().map(|h| h.forward_port).collect();
        ports.sort();
        ports.dedup();
        assert_eq!(ports.len(), hosts().len());
        assert!(!ports.contains(&SERVE_PORT));
    }
}
