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
#[cfg(test)]
mod heap;
mod journal;
mod layout;
mod merge;
mod origin;
mod scan;
mod session;
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
  claude-mesh session SESSION-ID|PATH.jsonl
                                        a session's outline: stretches by Mac and day,
                                        prompts, the first of each, compactions
  claude-mesh repair [--dry-run] [--force] SESSION-ID|PATH.jsonl
                                        put a transcript merged out of order back in time
                                        order, so `claude --resume` opens at its real end;
                                        refuses an open session, keeps a backup

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
        Some("repair") => repair(&args[1..]),
        Some("session") => session(&args[1..]),
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
    // Next to the state file, like state.peers/: never inside the synced folder.
    let state_dir = &node.state_path.with_extension("origins");
    // Lines Claude Code wrote here since the last round are this Mac's;
    // claim them before pulling anything (see `origin`). With no peer there's
    // nobody to tell (and the tests' servers, which have none, would race the
    // `once` runs over the same bookkeeping); lines written meanwhile are
    // still claimed once a peer appears.
    if !node.peers.is_empty() {
        match origin::claim(&node.host.config, state_dir, &node.host.name) {
            Ok(0) => {}
            Ok(n) => log!("origins: claimed {n} new lines"),
            Err(e) => log!("origins: can't claim ({e})"),
        }
    }
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
        // Transcripts written from this peer: their lines aren't ours.
        let written: Vec<PathBuf> = report
            .files
            .iter()
            .filter(|f| {
                matches!(f.action, sync::Action::Pulled | sync::Action::Merged | sync::Action::Conflict)
                    && f.path.starts_with("projects/")
                    && f.path.ends_with(".jsonl")
                    && f.path.matches('/').count() == 2
            })
            .filter_map(|f| {
                let rel = node.scanner.canon().rel_from_canonical(&f.path);
                node.scanner.local_path(&rel)
            })
            .collect();
        if let Err(e) = origin::mark_seen(state_dir, &written) {
            log!("origins: can't mark pulled lines ({e})");
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
        release_free_memory();
        std::thread::sleep(Duration::from_secs(interval));
    }
}

/// Hand memory freed during the round back to the system. macOS's allocator
/// keeps large freed blocks for reuse, and they count in the process's
/// footprint (Activity Monitor) until it's asked to let them go.
fn release_free_memory() {
    #[cfg(target_os = "macos")]
    {
        unsafe extern "C" {
            /// libmalloc: release free pages of `zone` (null: every zone)
            /// until `goal` bytes are released (0: as many as possible).
            fn malloc_zone_pressure_relief(zone: *mut std::ffi::c_void, goal: usize) -> usize;
        }
        // SAFETY: a plain libmalloc call; a null zone means all zones.
        unsafe { malloc_zone_pressure_relief(std::ptr::null_mut(), 0) };
    }
}

/// `repair [--dry-run] SESSION-ID`: sort a transcript into time order
/// ([`merge::sort_jsonl`]). Merges before the fix put old branches last, so
/// `claude --resume`, which takes the file's last `last-prompt` line as the
/// end of the conversation, opened mid-way. Refuses a session open on this
/// Mac; keeps the original next to the state file. The other Macs settle on
/// the repaired file through the next rounds.
fn repair(args: &[String]) -> i32 {
    let dry = args.iter().any(|a| a == "--dry-run");
    let force = args.iter().any(|a| a == "--force");
    let Some(id) = args.iter().find(|a| !a.starts_with("--")) else {
        eprint!("{USAGE}");
        return 2;
    };
    let node = match node() {
        Ok(n) => n,
        Err(e) => {
            eprintln!("claude-mesh: {e}");
            return 1;
        }
    };
    let config = &node.host.config;
    let (id, path) = match find_transcript(config, id) {
        Ok(found) => found,
        Err(e) => {
            eprintln!("claude-mesh: {e}");
            return 1;
        }
    };
    let (id, path) = (&id, &path);
    if sync::live_sessions(config).iter().any(|s| s == id) {
        eprintln!("claude-mesh: {id} is open on this Mac; close it first");
        return 1;
    }
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("claude-mesh: can't read {}: {e}", path.display());
            return 1;
        }
    };
    let sorted = merge::sort_jsonl(&text);
    let last_prompt = |t: &str| {
        t.lines()
            .rev()
            .find(|l| l.contains(r#""type":"last-prompt""#))
            .and_then(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .and_then(|v| v["lastPrompt"].as_str().map(|s| s.chars().take(60).collect::<String>()))
            .unwrap_or_default()
    };
    let (before, after): (Vec<&str>, Vec<&str>) = (text.lines().collect(), sorted.lines().collect());
    let moved = before.iter().zip(&after).filter(|(a, b)| a != b).count();
    println!("{}", path.display());
    println!("  lines:        {} → {} ({moved} in a different place)", before.len(), after.len());
    println!("  resume ends:  {:?} → {:?}", last_prompt(&text), last_prompt(&sorted));
    if sorted == text {
        println!("  already in time order");
        return 0;
    }
    // Claude itself writes a few lines milliseconds out of order; that's
    // harmless. Only a file whose resume point is wrong needs rewriting.
    if last_prompt(&sorted) == last_prompt(&text) && !force {
        println!("  --resume already opens at the end; nothing to do (--force to sort anyway)");
        return 0;
    }
    if dry {
        println!("  (dry run: nothing written)");
        return 0;
    }
    let backups = node.state_path.with_file_name("repair-backups");
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let backup = backups.join(format!("{id}-{secs}.jsonl"));
    if let Err(e) = std::fs::create_dir_all(&backups).and_then(|()| std::fs::copy(path, &backup)) {
        eprintln!("claude-mesh: can't keep a backup in {}: {e}", backups.display());
        return 1;
    }
    if let Err(e) = sync::write_atomic(path, sorted.as_bytes()) {
        eprintln!("claude-mesh: can't write {}: {e}", path.display());
        return 1;
    }
    println!("  repaired; the original is in {}", backup.display());
    0
}

/// A transcript by session id (under any project) or by path, as (session
/// id, path). A path picks one copy when the id is under several projects
/// (a session moved between projects keeps its old copy there).
fn find_transcript(config: &std::path::Path, arg: &str) -> Result<(String, PathBuf), String> {
    let given = PathBuf::from(arg);
    if given.is_file() && given.extension().is_some_and(|e| e == "jsonl") {
        let id = given.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        return Ok((id, given));
    }
    let found: Vec<PathBuf> = std::fs::read_dir(config.join("projects"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|d| d.path().join(format!("{arg}.jsonl")))
        .filter(|p| p.is_file())
        .collect();
    match found.as_slice() {
        [one] => Ok((arg.to_string(), one.clone())),
        _ => Err(format!(
            "{} transcripts named {arg}.jsonl under {}; give the path",
            found.len(),
            config.join("projects").display()
        )),
    }
}

/// `session SESSION-ID|PATH.jsonl`: print the session's outline
/// ([`session::print`]).
fn session(args: &[String]) -> i32 {
    let Some(arg) = args.first() else {
        eprint!("{USAGE}");
        return 2;
    };
    let node = match node() {
        Ok(n) => n,
        Err(e) => {
            eprintln!("claude-mesh: {e}");
            return 1;
        }
    };
    let result = find_transcript(&node.host.config, arg)
        .and_then(|(id, path)| session::print(&node.host.config, &id, &path).map_err(|e| e.to_string()));
    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("claude-mesh: {e}");
            1
        }
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
