//! `claude-mesh`: keep Claude Code's state in sync across the Macs, over
//! airshell, so every Mac sees every session, memory, plan and prompt.
//!
//! ```text
//!   arrakis                         caladan                         imperium
//!   ~/src/pplanel/.claude           ~/.claude                       ~/.claude
//!        │ scan                          │ scan                          │ scan
//!   claude-mesh serve ◀── pull ─── claude-mesh serve ◀── pull ─── claude-mesh serve
//!        │          ─── pull ──▶         │          ─── pull ──▶         │
//!   airshell-proxy  ═════ QUIC (LAN or AWDL), paired Macs only ═════ airshell-proxy
//! ```
//!
//! Every Mac runs `serve`: it answers its peers' requests on
//! `127.0.0.1:7480` (airshell-proxy relays them there), and every 30 s it
//! pulls from each peer and merges into its own config folder. No Mac ever
//! writes to another.
//!
//! # Reading order
//!
//! Follow a file from one Mac's disk to another's:
//!
//! 1. [`layout`]: where each Mac keeps its sources and its Claude config.
//! 2. [`canon`]: the machine-independent form names and contents travel in.
//! 3. [`scan`]: what is synced, and the [`scan::Manifest`] that describes it.
//! 4. [`airshell`] and [`wire`]: how a Mac reaches a peer, and what they say.
//! 5. [`sync`]: what a Mac does with each file a peer has.
//! 6. [`merge`]: how two versions become one, the same way on every Mac.
//!    [`journal`] records what each round did, for `report` and `status`.
//! 7. This file: the commands, the round loop, the environment.
//!
//! `docs/codebase.md` walks the same path with diagrams; `docs/design.md`
//! has the decisions, `docs/operations.md` the failure modes.
//!
//! # Commands
//!
//! | Command    | What it does                                                   |
//! |------------|----------------------------------------------------------------|
//! | `serve`    | [`serve`]: listen, register with airshell-proxy, pull forever  |
//! | `once`     | [`once`]: one pull round against every peer                    |
//! | `status`   | [`status`]: this Mac, and whether each peer answers            |
//! | `register` | [`register`]: add this Mac's service to airshell-proxy         |
//! | `report`   | [`report`]: what recent rounds did, from the [`journal`]        |

// Every item documented, private ones included: `cargo doc
// --document-private-items` is the code's reference.
#![warn(clippy::missing_docs_in_private_items)]

/// `eprintln!` with a `[unix seconds]` prefix: the daemon's log line.
macro_rules! log {
    ($($arg:tt)*) => {
        eprintln!("{} {}", $crate::timestamp(), format!($($arg)*))
    };
}

mod airshell;
mod canon;
mod journal;
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

/// `claude-mesh` with no or unknown arguments prints this.
const USAGE: &str = "\
claude-mesh — keep Claude Code's state in sync across Macs, over airshell

usage:
  claude-mesh serve [--interval SECS]   serve this Mac's state, register it with
                                        airshell-proxy, pull from peers forever (every 30 s)
  claude-mesh once                      pull from every peer once, then exit
  claude-mesh status                    show this Mac, its peers, and whether they answer
  claude-mesh register                  add this Mac's service to airshell-proxy, then exit
  claude-mesh report [--since 1h] [--peer NAME] [--files]
                                        what recent rounds fetched, merged and couldn't do
                                        (from the journal; --since defaults to 24h)

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
        Some("report") => report(&args[1..]),
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
    /// Its name in the layout table.
    name: String,
    /// The command (or `tcp:<addr>`) that opens a stream to it; see
    /// [`wire::Peer::connect`].
    route: Vec<String>,
}

/// Everything a command needs to know about this Mac.
struct Node {
    /// This Mac's layout entry (its config folder possibly overridden).
    host: Host,
    /// Scans this Mac's config folder; shared with the server threads.
    scanner: Arc<Scanner>,
    /// Where [`State`] is kept between rounds.
    state_path: PathBuf,
    /// Where `serve` listens, `127.0.0.1:7480` unless overridden.
    listen: String,
    /// Every other Mac, and how to reach it.
    peers: Vec<PeerSpec>,
}

/// This Mac, from the layout table and the `CLAUDE_MESH_*` overrides. Fails
/// when this Mac isn't in the table.
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

/// One pull round: for each peer, connect, [`sync::sync_peer`], log what
/// changed, save the state. An unreachable peer is logged and skipped.
fn round(node: &Node) {
    let mut state = State::load(&node.state_path);
    let journal = journal::path_for(&node.state_path);
    for peer in &node.peers {
        let started = now_ms();
        let mut report = sync::Report::default();
        let error = match Peer::connect(&peer.route) {
            Err(e) => {
                log!("{}: unreachable ({e})", peer.name);
                Some(format!("unreachable: {e}"))
            }
            Ok(mut conn) => sync::sync_peer(
                &node.host.name,
                &peer.name,
                &mut conn,
                &node.scanner,
                &mut state,
                // Next to the state file (state.peers/<peer>), never inside
                // the synced folder.
                &node.state_path.with_extension("peers").join(&peer.name),
                &mut report,
            )
            .err()
            .map(|e| {
                log!("{}: {e}", peer.name);
                e.to_string()
            }),
        };
        if report.wrote_anything() {
            log!(
                "{}: pulled {}, merged {}, conflicts {}, busy {}, failed {}, fetched {} ({} as tails)",
                peer.name,
                report.pulled,
                report.merged,
                report.conflicts,
                report.skipped_active,
                report.failed,
                journal::bytes(report.fetched),
                report.tails
            );
        }
        if let Err(e) = state.save(&node.state_path) {
            log!("can't save state to {}: {e}", node.state_path.display());
        }
        let round = journal::Round::new(
            started,
            &peer.name,
            now_ms().saturating_sub(started),
            report,
            error,
        );
        if let Err(e) = journal::append(&journal, &round) {
            log!("can't write the journal {}: {e}", journal.display());
        }
    }
}

/// `serve [--interval SECS]`: start the server thread, register with
/// airshell-proxy (unless peers come from `CLAUDE_MESH_PEERS`), then run a
/// [`round`] every interval, forever. Exits only if the server stops.
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

/// `once`: one [`round`], then exit.
fn once() -> i32 {
    match node() {
        Ok(n) => {
            round(&n);
            0
        }
        Err(e) => fail(&e),
    }
}

/// `status`: this Mac's name, config folder, state file and file count, and
/// each peer's `health` answer (or why it can't be reached).
fn status() -> i32 {
    let node = match node() {
        Ok(n) => n,
        Err(e) => return fail(&e),
    };
    println!("this Mac:  {}", node.host.name);
    println!("config:    {}", node.host.config.display());
    println!("state:     {}", node.state_path.display());
    println!("files:     {}", node.scanner.manifest().len());
    let state = State::load(&node.state_path);
    let rounds = journal::read(&journal::path_for(&node.state_path), 0);
    println!(
        "\n{:<10} {:<9} {:>14} {:>6} {:>13}  LAST ROUND",
        "PEER", "ANSWERS", "TO PULL", "BUSY", "IT LACKS"
    );
    for peer in &node.peers {
        let last =
            rounds
                .iter()
                .rev()
                .find(|r| r.peer == peer.name)
                .map_or("none yet".to_string(), |r| {
                    format!(
                        "{}, {}{}",
                        journal::datetime(r.at),
                        journal::bytes(r.fetched),
                        r.error.as_ref().map_or(String::new(), |e| format!(", {e}"))
                    )
                });
        let (answers, pending) = match Peer::connect(&peer.route) {
            Err(e) => (format!("no ({e})"), None),
            Ok(mut conn) => {
                let remote: Option<scan::Manifest> = conn
                    .request("manifest")
                    .ok()
                    .and_then(|b| serde_json::from_slice(&b).ok());
                (
                    "yes".to_string(),
                    remote.map(|m| sync::pending(&peer.name, &m, &node.scanner, &state)),
                )
            }
        };
        match pending {
            Some(p) => println!(
                "{:<10} {:<9} {:>14} {:>6} {:>13}  {last}",
                peer.name,
                answers,
                format!("{} ({})", p.files, journal::bytes(p.bytes)),
                p.busy,
                format!("~{} files", p.theirs_to_pull),
            ),
            None => println!("{:<10} {answers}  last round: {last}", peer.name),
        }
    }
    println!(
        "\nTO PULL: files the next round fetches from the peer (their size; tails fetch less)."
    );
    println!("BUSY: left for later (written in the last minute, or an open session's).");
    println!("IT LACKS: files here the peer doesn't have the same; it pulls them on its rounds.");
    0
}

/// `report [--since DUR] [--peer NAME] [--files]`: what the journal says
/// about recent rounds; see [`journal::render`].
fn report(args: &[String]) -> i32 {
    let node = match node() {
        Ok(n) => n,
        Err(e) => return fail(&e),
    };
    let value = |flag: &str| args.windows(2).find(|w| w[0] == flag).map(|w| w[1].clone());
    let window = match value("--since") {
        None => 86_400_000,
        Some(d) => match journal::parse_duration(&d) {
            Some(ms) => ms,
            None => return fail(&format!("--since {d}: use e.g. 30m, 2h, 1d")),
        },
    };
    let since = now_ms().saturating_sub(window);
    let rounds = journal::read(&journal::path_for(&node.state_path), since);
    print!(
        "{}",
        journal::render(
            &rounds,
            &journal::Options {
                since,
                peer: value("--peer"),
                files: args.iter().any(|a| a == "--files"),
                host: node.host.name.clone(),
            }
        )
    );
    0
}

/// `register`: [`airshell::register`] this Mac's service, then exit.
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

/// The port of a `host:port` listen address ([`SERVE_PORT`] if unparsable).
fn listen_port(listen: &str) -> u16 {
    listen
        .rsplit_once(':')
        .and_then(|(_, p)| p.parse().ok())
        .unwrap_or(SERVE_PORT)
}

/// Print `msg` as an error and return the exit code 1.
fn fail(msg: &str) -> i32 {
    eprintln!("claude-mesh: {msg}");
    1
}

/// Milliseconds since the Unix epoch.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// `[<unix seconds>]`, the prefix of every log line.
pub fn timestamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("[{secs}]")
}
