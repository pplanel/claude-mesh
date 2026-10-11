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

#[test]
fn a_file_that_cant_be_written_doesnt_stop_the_round() {
    // A name that fits on caladan but not on arrakis, whose project folder is
    // longer (`-Users-pplanel-src-pplanel-…`): only that file is skipped.
    let mesh = start();
    let long = "x".repeat(255 - "-Users-pplanel-src-".len());
    write(
        &mesh.dirs[1],
        &format!("projects/-Users-pplanel-src-{long}/s.jsonl"),
        "{}\n",
    );
    write(&mesh.dirs[1], "plans/ok.md", "still arrives");
    let st = command(&mesh, 0, &["once"]).status().unwrap();
    assert!(st.success());
    assert_eq!(read(&mesh.dirs[0], "plans/ok.md"), "still arrives");
}

#[test]
fn two_canonical_forms_of_one_file_dont_conflict_forever() {
    // Text naming a path only one layout rewrites ("/Users/pplanel/src/.claude"
    // is plain text on arrakis, `@SRC@/.claude` on caladan) gets different
    // canonical bytes on each side, and arrakis can't write the peers' form
    // back without it reading as something else. With arrakis's copy the
    // oldest, as when a file is born there, that must not conflict (and copy
    // the copies) every round.
    let mesh = start();
    let rel = "projects/-Users-pplanel/tool-results/t1.txt";
    write(&mesh.dirs[0], rel, "see /Users/pplanel/src/.claude/x\n");
    let old = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    std::fs::File::options()
        .write(true)
        .open(mesh.dirs[0].join(rel))
        .unwrap()
        .set_modified(old)
        .unwrap();
    let conflicts = |mesh: &Mesh| -> Vec<String> {
        mesh.dirs
            .iter()
            .flat_map(|d| walk(d))
            .filter(|p| p.contains(".conflict-"))
            .collect()
    };
    converge(&mesh);
    let first = conflicts(&mesh);
    // At most the one honest conflict of first contact (the rewritten path
    // really reads differently on arrakis), and never a copy of a copy.
    assert!(
        first.iter().all(|p| p.matches(".conflict-").count() == 1),
        "{first:?}"
    );
    // And nothing new afterwards: the Macs agree and stay put.
    for _ in 0..3 {
        converge(&mesh);
    }
    let mut later = conflicts(&mesh);
    let mut first = first;
    later.sort();
    first.sort();
    assert_eq!(later, first);
}

fn walk(dir: &Path) -> Vec<String> {
    let mut out = vec![];
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(walk(&p));
        } else {
            out.push(p.display().to_string());
        }
    }
    out
}

#[test]
fn a_grown_log_is_fetched_as_its_tail() {
    let mesh = start();
    let key = "projects/-Users-pplanel/big.jsonl";
    let line = |i: usize| {
        format!(
            r#"{{"uuid":"{i}","timestamp":"2026-10-11T10:00:00Z","pad":"{}"}}"#,
            "x".repeat(200)
        )
    };
    let first: String = (0..2000).map(|i| line(i) + "\n").collect();
    write(&mesh.dirs[0], key, &first);
    converge(&mesh);
    // The session goes on on caladan: one more line.
    let grown = format!("{first}{}\n", line(2000));
    write(&mesh.dirs[1], key, &grown);
    let out = command(&mesh, 0, &["once"]).output().unwrap();
    let log = String::from_utf8_lossy(&out.stderr);
    assert!(
        log.contains("caladan: pulled 1") && log.contains("(1 as tails)"),
        "{log}"
    );
    // About one line, not the ~450 KiB file.
    assert!(log.contains("fetched 1 KiB"), "{log}");
    assert_eq!(read(&mesh.dirs[0], key), grown);
}

#[test]
fn a_diverged_log_falls_back_to_the_whole_file() {
    let mesh = start();
    let key = "projects/-Users-pplanel/d.jsonl";
    let a = r#"{"uuid":"a","timestamp":"2026-10-11T10:00:00Z"}"#;
    let b = r#"{"uuid":"b","timestamp":"2026-10-11T10:01:00Z"}"#;
    let c = r#"{"uuid":"c","timestamp":"2026-10-11T10:02:00Z"}"#;
    write(&mesh.dirs[0], key, &format!("{a}\n"));
    converge(&mesh);
    // Each side adds its own line: not a prefix of each other any more.
    write(&mesh.dirs[0], key, &format!("{a}\n{b}\n"));
    write(&mesh.dirs[1], key, &format!("{a}\n{c}\n"));
    converge(&mesh);
    for d in &mesh.dirs {
        assert_eq!(read(d, key), format!("{a}\n{b}\n{c}\n"), "{}", d.display());
    }
}

#[test]
fn a_diverged_log_that_keeps_growing_still_comes_as_tails() {
    // This session's case: a branch of it lives on caladan, while arrakis
    // keeps writing its own (active, so it never merges caladan's in).
    let mesh = start();
    let key = "projects/-Users-pplanel/live.jsonl";
    let line = |id: &str, ts: &str| {
        format!(
            r#"{{"uuid":"{id}","timestamp":"2026-10-11T10:{ts}:00Z","pad":"{}"}}"#,
            "y".repeat(500)
        )
    };
    write(&mesh.dirs[0], key, &format!("{}\n", line("a", "00")));
    converge(&mesh);
    let ab = format!("{}\n{}\n", line("a", "00"), line("b", "01"));
    write(&mesh.dirs[0], key, &ab);
    write(
        &mesh.dirs[1],
        key,
        &format!("{}\n{}\n", line("a", "00"), line("c", "02")),
    );
    let once = |i: usize| {
        let out = command(&mesh, i, &["once"]).output().unwrap();
        String::from_utf8_lossy(&out.stderr).to_string()
    };
    once(1); // caladan merges arrakis's branch in, and keeps arrakis's copy
    // arrakis goes on writing, never pulling.
    write(&mesh.dirs[0], key, &format!("{ab}{}\n", line("d", "03")));
    let log = once(1);
    let arrakis = log.lines().find(|l| l.contains("arrakis:")).unwrap_or("");
    assert!(
        arrakis.contains("(1 as tails)") && arrakis.contains("fetched 1 KiB"),
        "{log}"
    );
    let got = read(&mesh.dirs[1], key);
    for id in ["a", "b", "c", "d"] {
        assert!(
            got.contains(&format!(r#""uuid":"{id}""#)),
            "{id} missing: {got}"
        );
    }
}

#[test]
fn a_log_that_reads_back_differently_still_grows_by_tails() {
    // A session quoting a path that only arrakis rewrites: imperium's copy,
    // read back, is never a prefix of arrakis's, even when nothing diverged.
    let mesh = start();
    let key = "projects/-Users-pplanel/paths.jsonl";
    let line = |i: usize| {
        format!(
            r#"{{"uuid":"{i}","timestamp":"2026-10-11T10:00:00Z","t":"see /Users/pplanel/src/.claude {}"}}"#,
            "z".repeat(300)
        )
    };
    let first: String = (0..500).map(|i| line(i) + "\n").collect();
    write(&mesh.dirs[0], key, &first);
    let once = |i: usize| {
        let out = command(&mesh, i, &["once"]).output().unwrap();
        String::from_utf8_lossy(&out.stderr).to_string()
    };
    once(2);
    write(&mesh.dirs[0], key, &format!("{first}{}\n", line(500)));
    let log = once(2);
    let arrakis = log.lines().find(|l| l.contains("arrakis:")).unwrap_or("");
    assert!(
        arrakis.contains("(1 as tails)") && arrakis.contains("fetched 1 KiB"),
        "{log}"
    );
}
