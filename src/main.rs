//! `claude-mesh`: keep Claude Code's state in sync across the Macs, over
//! airshell. See README.md and docs/.

macro_rules! log {
    ($($arg:tt)*) => {
        eprintln!("{} {}", $crate::timestamp(), format!($($arg)*))
    };
}

mod airshell;
mod canon;
mod layout;
mod merge;
mod scan;
mod sync;
mod wire;

use canon::Canon;
use layout::Host;
use scan::Scanner;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use sync::State;
use wire::Peer;

const USAGE: &str = "\
claude-mesh — keep Claude Code's state in sync across Macs, over airshell

usage:
  claude-mesh serve [--interval SECS]   serve this Mac's state, register it with
                                        airshell-proxy, pull from peers forever (every 30 s)
  claude-mesh once                      pull from every peer once, then exit
  claude-mesh status                    show this Mac, its peers, and whether they answer
  claude-mesh register                  add this Mac's service to airshell-proxy, then exit

environment:
  CLAUDE_MESH_LAYOUT      the layout table (default: /etc/claude-mesh/hosts.json)
  CLAUDE_MESH_HOST        this Mac's name (default: hostname -s)
  CLAUDE_MESH_CONFIG_DIR  the Claude config folder (default: from the layout table)
  CLAUDE_MESH_STATE       state file (default: ~/.local/state/claude-mesh/state.json)
  CLAUDE_MESH_LISTEN      where `serve` listens (default: 127.0.0.1:7480)
  CLAUDE_MESH_PEERS       peers as name=route;... instead of airshell, where a route
                          is tcp:<addr> or a command (tests, debugging)
  CLAUDE_MESH_ACTIVE_MS   leave files written more recently than this alone (default: 60000)
";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(String::as_str) {
        Some("serve") => serve(&args[1..]),
        Some("once") => once(),
        Some("status") => status(),
        Some("register") => register(),
        _ => {
            eprint!("{USAGE}");
            2
        }
    };
    std::process::exit(code);
}

/// The port `serve` listens on, on every Mac; airshell relays to it.
const SERVE_PORT: u16 = 7480;

/// A peer and the route that reaches it.
struct PeerSpec {
    name: String,
    route: Vec<String>,
}

/// Everything a command needs to know about this Mac.
struct Node {
    host: Host,
    scanner: Arc<Scanner>,
    state_path: PathBuf,
    listen: String,
    peers: Vec<PeerSpec>,
}

fn node() -> Result<Node, String> {
    let name = layout::this_host_name();
    let hosts = layout::hosts()?;
    let mut host = hosts
        .iter()
        .find(|h| h.name == name)
        .cloned()
        .ok_or_else(|| format!("{name:?} isn't in the layout table; set CLAUDE_MESH_HOST"))?;
    if let Ok(dir) = std::env::var("CLAUDE_MESH_CONFIG_DIR") {
        host.config = PathBuf::from(dir);
    }
    let home = PathBuf::from(std::env::var("HOME").unwrap_or_default());
    let state_path = std::env::var("CLAUDE_MESH_STATE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home.join(".local/state/claude-mesh/state.json"));
    let listen =
        std::env::var("CLAUDE_MESH_LISTEN").unwrap_or_else(|_| format!("127.0.0.1:{SERVE_PORT}"));
    let peers = match std::env::var("CLAUDE_MESH_PEERS") {
        Ok(spec) => spec
            .split(';')
            .filter_map(|p| p.split_once('='))
            .map(|(n, route)| PeerSpec {
                name: n.to_string(),
                route: route.split_whitespace().map(String::from).collect(),
            })
            .collect(),
        Err(_) => hosts
            .iter()
            .filter(|h| h.name != host.name)
            .map(|h| PeerSpec {
                name: h.name.clone(),
                route: airshell::route(&h.name),
            })
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
    for peer in &node.peers {
        let mut conn = match Peer::connect(&peer.route) {
            Ok(conn) => conn,
            Err(e) => {
                log!("{}: unreachable ({e})", peer.name);
                continue;
            }
        };
        match sync::sync_peer(
            &node.host.name,
            &peer.name,
            &mut conn,
            &node.scanner,
            &mut state,
        ) {
            Ok(r) if r.pulled + r.merged + r.conflicts + r.failed > 0 => log!(
                "{}: pulled {}, merged {}, conflicts {}, busy {}, failed {}",
                peer.name,
                r.pulled,
                r.merged,
                r.conflicts,
                r.skipped_active,
                r.failed
            ),
            Ok(_) => {}
            Err(e) => log!("{}: {e}", peer.name),
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
    let names: Vec<&str> = node.peers.iter().map(|p| p.name.as_str()).collect();
    log!(
        "{} config {} peers {:?}",
        node.host.name,
        node.host.config.display(),
        names
    );
    let (listen, name, scanner) = (
        node.listen.clone(),
        node.host.name.clone(),
        Arc::clone(&node.scanner),
    );
    std::thread::spawn(move || {
        if let Err(e) = wire::serve_tcp(&listen, name, scanner) {
            log!("server stopped: {e}");
            std::process::exit(1);
        }
    });
    // Peers can reach us only once the proxy advertises our service. Without
    // a proxy (tests set CLAUDE_MESH_PEERS) there's nothing to register.
    if std::env::var_os("CLAUDE_MESH_PEERS").is_none() {
        match airshell::register(&node.host.name, listen_port(&node.listen)) {
            Ok(what) => log!("airshell: {what}"),
            Err(e) => log!("airshell: can't register ({e}); peers can't reach this Mac yet"),
        }
    }
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
    for peer in &node.peers {
        let answer = match Peer::connect(&peer.route) {
            Ok(conn) => conn.health.clone(),
            Err(e) => format!("not reachable: {e}"),
        };
        println!("peer {:<9} {answer}", peer.name);
    }
    0
}

fn register() -> i32 {
    let node = match node() {
        Ok(n) => n,
        Err(e) => return fail(&e),
    };
    match airshell::register(&node.host.name, listen_port(&node.listen)) {
        Ok(what) => {
            println!("{what}");
            0
        }
        Err(e) => fail(&e),
    }
}

fn listen_port(listen: &str) -> u16 {
    listen
        .rsplit_once(':')
        .and_then(|(_, p)| p.parse().ok())
        .unwrap_or(SERVE_PORT)
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
