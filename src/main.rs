//! `claude-mesh`: keep Claude Code's state in sync across the Macs, over
//! airshell. See README.md and docs/.

macro_rules! log {
    ($($arg:tt)*) => {
        eprintln!("{} {}", $crate::timestamp(), format!($($arg)*))
    };
}

mod canon;
mod http;
mod layout;
mod merge;
mod scan;
mod sync;

use canon::Canon;
use layout::{Host, SERVE_PORT, SERVICE_TYPE};
use scan::Scanner;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use sync::State;

const USAGE: &str = "\
claude-mesh — keep Claude Code's state in sync across Macs, over airshell

usage:
  claude-mesh serve [--interval SECS]   serve this Mac's state and pull from peers forever
  claude-mesh once                      pull from every peer once, then exit
  claude-mesh status                    show this Mac, its peers, and whether they answer
  claude-mesh airshell-config           print the airshell-proxy entries for this Mac
  claude-mesh launchd-plist             print a launchd agent that runs `serve` at login

environment (mostly for tests):
  CLAUDE_MESH_HOST        this Mac's name (default: hostname -s)
  CLAUDE_MESH_CONFIG_DIR  the Claude config folder (default: from the layout table)
  CLAUDE_MESH_STATE       state file (default: ~/.local/state/claude-mesh/state.json)
  CLAUDE_MESH_LISTEN      listen address (default: 127.0.0.1:7480)
  CLAUDE_MESH_PEERS       peers as name=host:port,... (default: airshell forwards)
  CLAUDE_MESH_ACTIVE_MS   leave files written more recently than this alone (default: 60000)
";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(String::as_str) {
        Some("serve") => serve(&args[1..]),
        Some("once") => once(),
        Some("status") => status(),
        Some("airshell-config") => airshell_config(),
        Some("launchd-plist") => launchd_plist(),
        _ => {
            eprint!("{USAGE}");
            2
        }
    };
    std::process::exit(code);
}

/// Everything a command needs to know about this Mac.
struct Node {
    host: Host,
    scanner: Arc<Scanner>,
    state_path: PathBuf,
    listen: String,
    peers: Vec<(String, String)>,
}

fn node() -> Result<Node, String> {
    let name = layout::this_host_name();
    let mut host = layout::find(&name).ok_or_else(|| {
        format!("{name:?} isn't in the layout table (src/layout.rs); set CLAUDE_MESH_HOST")
    })?;
    if let Ok(dir) = std::env::var("CLAUDE_MESH_CONFIG_DIR") {
        host.config = PathBuf::from(dir);
    }
    let state_path = std::env::var("CLAUDE_MESH_STATE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(std::env::var("HOME").unwrap_or_default())
                .join(".local/state/claude-mesh/state.json")
        });
    let listen =
        std::env::var("CLAUDE_MESH_LISTEN").unwrap_or_else(|_| format!("127.0.0.1:{SERVE_PORT}"));
    let peers = match std::env::var("CLAUDE_MESH_PEERS") {
        Ok(spec) => spec
            .split(',')
            .filter_map(|p| {
                p.split_once('=')
                    .map(|(n, a)| (n.to_string(), a.to_string()))
            })
            .collect(),
        Err(_) => layout::peers_of(&host.name)
            .into_iter()
            .map(|p| (p.name, format!("127.0.0.1:{}", p.forward_port)))
            .collect(),
    };
    let scanner = Arc::new(Scanner::new(host.config.clone(), Canon::new(&host)));
    Ok(Node {
        host,
        scanner,
        state_path,
        listen,
        peers,
    })
}

fn round(node: &Node) {
    let mut state = State::load(&node.state_path);
    for (peer, addr) in &node.peers {
        match sync::sync_peer(&node.host.name, peer, addr, &node.scanner, &mut state) {
            Ok(r) if r.pulled + r.merged + r.conflicts > 0 => log!(
                "{peer}: pulled {}, merged {}, conflicts {}, busy {}",
                r.pulled,
                r.merged,
                r.conflicts,
                r.skipped_active
            ),
            Ok(_) => {}
            Err(e) => log!("{peer} ({addr}): {e}"),
        }
        if let Err(e) = state.save(&node.state_path) {
            log!("can't save state to {}: {e}", node.state_path.display());
        }
    }
}

fn serve(args: &[String]) -> i32 {
    let node = match node() {
        Ok(n) => n,
        Err(e) => return fail(&e),
    };
    let interval = args
        .windows(2)
        .find(|w| w[0] == "--interval")
        .and_then(|w| w[1].parse().ok())
        .unwrap_or(30u64);
    log!(
        "{} config {} peers {:?}",
        node.host.name,
        node.host.config.display(),
        node.peers
    );
    let (listen, name, scanner) = (
        node.listen.clone(),
        node.host.name.clone(),
        Arc::clone(&node.scanner),
    );
    std::thread::spawn(move || {
        if let Err(e) = http::serve(&listen, name, scanner) {
            log!("server stopped: {e}");
            std::process::exit(1);
        }
    });
    loop {
        round(&node);
        std::thread::sleep(Duration::from_secs(interval));
    }
}

fn once() -> i32 {
    match node() {
        Ok(n) => {
            round(&n);
            0
        }
        Err(e) => fail(&e),
    }
}

fn status() -> i32 {
    let node = match node() {
        Ok(n) => n,
        Err(e) => return fail(&e),
    };
    println!("this Mac:  {}", node.host.name);
    println!("config:    {}", node.host.config.display());
    println!("state:     {}", node.state_path.display());
    println!("files:     {}", node.scanner.manifest().len());
    for (peer, addr) in &node.peers {
        let answer = match http::get(addr, "/health") {
            Ok(body) => String::from_utf8_lossy(&body).to_string(),
            Err(e) => format!("not reachable: {e}"),
        };
        println!("peer {peer:<9} {addr:<16} {answer}");
    }
    0
}

fn airshell_config() -> i32 {
    let node = match node() {
        Ok(n) => n,
        Err(e) => return fail(&e),
    };
    let me = &node.host.name;
    println!("# claude-mesh entries for {me}'s ~/.config/airshell/config.toml");
    println!("# The proxy reloads the file within seconds; no restart needed.\n");
    println!("[[service]]");
    println!("name = \"{me}-claudesync\"");
    println!("transport = \"quic\"");
    println!("service_type = \"{SERVICE_TYPE}\"");
    println!("host = \"127.0.0.1\"");
    println!("port = {SERVE_PORT}\n");
    for peer in layout::peers_of(me) {
        println!("[[forward]]");
        println!("name = \"{}-claudesync\"", peer.name);
        println!("transport = \"quic\"");
        println!("service_type = \"{SERVICE_TYPE}\"");
        println!("listen = \"localhost:{}\"\n", peer.forward_port);
    }
    0
}

fn launchd_plist() -> i32 {
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let home = std::env::var("HOME").unwrap_or_default();
    print!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>dev.pplanel.claude-mesh</string>
  <key>ProgramArguments</key>
  <array><string>{exe}</string><string>serve</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardErrorPath</key><string>{home}/Library/Logs/claude-mesh.log</string>
</dict>
</plist>
"#
    );
    0
}

fn fail(msg: &str) -> i32 {
    eprintln!("claude-mesh: {msg}");
    1
}

pub fn timestamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("[{secs}]")
}
