//! End to end: three `claude-mesh` processes, one per Mac layout, each with
//! its own Claude config folder, talking over localhost (`tcp:` routes)
//! instead of airshell.

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

const HOSTS: [&str; 3] = ["arrakis", "caladan", "imperium"];

struct Mesh {
    _tmp: tempfile::TempDir,
    dirs: Vec<PathBuf>,
    ports: Vec<u16>,
    servers: Vec<Child>,
}

impl Drop for Mesh {
    fn drop(&mut self) {
        for s in &mut self.servers {
            let _ = s.kill();
            let _ = s.wait();
        }
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn command(mesh: &Mesh, i: usize, args: &[&str]) -> Command {
    let peers: Vec<String> = (0..3)
        .filter(|&j| j != i)
        .map(|j| format!("{}=tcp:127.0.0.1:{}", HOSTS[j], mesh.ports[j]))
        .collect();
    let mut c = Command::new(env!("CARGO_BIN_EXE_claude-mesh"));
    c.args(args)
        .env("CLAUDE_MESH_HOST", HOSTS[i])
        .env("CLAUDE_MESH_CONFIG_DIR", &mesh.dirs[i])
        .env(
            "CLAUDE_MESH_STATE",
            mesh.dirs[i].with_extension("state.json"),
        )
        .env("CLAUDE_MESH_LISTEN", format!("127.0.0.1:{}", mesh.ports[i]))
        .env("CLAUDE_MESH_PEERS", peers.join(";"))
        .env("CLAUDE_MESH_ACTIVE_MS", "0");
    c
}

fn start() -> Mesh {
    let tmp = tempfile::tempdir().unwrap();
    let dirs: Vec<PathBuf> = HOSTS.iter().map(|h| tmp.path().join(h)).collect();
    for d in &dirs {
        std::fs::create_dir_all(d).unwrap();
    }
    let ports: Vec<u16> = HOSTS.iter().map(|_| free_port()).collect();
    let mut mesh = Mesh {
        _tmp: tmp,
        dirs,
        ports,
        servers: vec![],
    };
    for i in 0..3 {
        let child = command(&mesh, i, &["serve", "--interval", "3600"])
            .spawn()
            .unwrap();
        mesh.servers.push(child);
    }
    for &p in &mesh.ports {
        let deadline = Instant::now() + Duration::from_secs(10);
        while std::net::TcpStream::connect(("127.0.0.1", p)).is_err() {
            assert!(Instant::now() < deadline, "server on {p} didn't start");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    mesh
}

/// Two full rounds on every Mac: enough for anything to reach everyone.
fn converge(mesh: &Mesh) {
    for _ in 0..2 {
        for i in 0..3 {
            let st = command(mesh, i, &["once"]).status().unwrap();
            assert!(st.success());
        }
    }
}

fn write(dir: &Path, rel: &str, content: &str) {
    let p = dir.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, content).unwrap();
}

fn read(dir: &Path, rel: &str) -> String {
    std::fs::read_to_string(dir.join(rel))
        .unwrap_or_else(|e| panic!("{}: {e}", dir.join(rel).display()))
}

#[test]
fn a_session_moves_between_layouts_with_paths_rewritten() {
    let mesh = start();
    let (arrakis, caladan) = (&mesh.dirs[0], &mesh.dirs[1]);
    let line = r#"{"uuid":"1","timestamp":"2026-10-08T10:00:00Z","cwd":"/Users/pplanel/src/pplanel/shift-compute"}"#;
    write(
        arrakis,
        "projects/-Users-pplanel-src-pplanel-shift-compute/s1.jsonl",
        &format!("{line}\n"),
    );
    converge(&mesh);

    let got = read(
        caladan,
        "projects/-Users-pplanel-src-shift-compute/s1.jsonl",
    );
    assert!(
        got.contains(r#""cwd":"/Users/pplanel/src/shift-compute""#),
        "{got}"
    );
    // And it comes back to arrakis unchanged, not doubled.
    assert_eq!(
        read(
            arrakis,
            "projects/-Users-pplanel-src-pplanel-shift-compute/s1.jsonl"
        ),
        format!("{line}\n")
    );
}

#[test]
fn a_continued_session_grows_everywhere() {
    let mesh = start();
    let key = "projects/-Users-pplanel/s2.jsonl";
    let one = r#"{"uuid":"1","timestamp":"2026-10-08T10:00:00Z"}"#;
    let two = r#"{"uuid":"2","timestamp":"2026-10-08T10:01:00Z"}"#;
    write(&mesh.dirs[0], key, &format!("{one}\n"));
    converge(&mesh);
    // The session moved to imperium and kept going there.
    let mut grown = read(&mesh.dirs[2], key);
    grown.push_str(two);
    grown.push('\n');
    write(&mesh.dirs[2], key, &grown);
    converge(&mesh);
    for d in &mesh.dirs {
        assert_eq!(read(d, key), format!("{one}\n{two}\n"), "{}", d.display());
    }
}

#[test]
fn memory_index_entries_from_every_mac_survive() {
    let mesh = start();
    let rel = "projects/-Users-pplanel/memory/MEMORY.md";
    write(&mesh.dirs[0], rel, "- [A](a.md) — from arrakis\n");
    write(&mesh.dirs[1], rel, "- [B](b.md) — from caladan\n");
    write(&mesh.dirs[2], rel, "- [C](c.md) — from imperium\n");
    converge(&mesh);
    let first = read(&mesh.dirs[0], rel);
    for d in &mesh.dirs {
        let got = read(d, rel);
        assert_eq!(got, first, "{} differs", d.display());
        for n in ["a.md", "b.md", "c.md"] {
            assert_eq!(got.matches(n).count(), 1, "{n} in {got}");
        }
    }
}

#[test]
fn credentials_and_settings_never_leave_a_mac() {
    let mesh = start();
    write(&mesh.dirs[0], ".credentials.json", "secret");
    write(&mesh.dirs[0], "settings.json", "{}");
    write(&mesh.dirs[0], "plans/p.md", "plan");
    converge(&mesh);
    assert!(!mesh.dirs[1].join(".credentials.json").exists());
    assert!(!mesh.dirs[1].join("settings.json").exists());
    assert_eq!(read(&mesh.dirs[1], "plans/p.md"), "plan");
}

#[test]
fn a_command_route_carries_the_stream() {
    // What airshell-connect does, with nc: a process whose stdio is the stream.
    let mesh = start();
    write(&mesh.dirs[1], "plans/q.md", "via a command");
    let st = command(&mesh, 0, &["once"])
        .env(
            "CLAUDE_MESH_PEERS",
            format!("caladan=nc 127.0.0.1 {}", mesh.ports[1]),
        )
        .status()
        .unwrap();
    assert!(st.success());
    assert_eq!(read(&mesh.dirs[0], "plans/q.md"), "via a command");
}
