//! How Macs talk: a line protocol over one stream per round.
//!
//! `claude-mesh serve` answers it on `127.0.0.1:7480`. Other Macs reach that
//! port through airshell-proxy, which carries it over QUIC (LAN or AWDL) and
//! admits only paired Macs: their Secure Enclave keys in `authorized_peers`,
//! later mesh membership. So nothing here does authentication or TLS.
//!
//! | Request       | Response body                                   |
//! |---------------|-------------------------------------------------|
//! | `health`      | `{"host": "...", "version": "..."}`             |
//! | `manifest`    | the [`Manifest`](crate::scan::Manifest) as JSON |
//! | `file <hex>`  | canonical bytes of the canonical path `<hex>`   |
//!
//! Every response is a line `ok <len>` or `err <len>`, then `<len>` bytes.

use crate::scan::Scanner;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

/// How long a route gets to answer `health` before we give up on it.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

/// Serve the protocol on `addr`, one thread per connection.
pub fn serve_tcp(addr: &str, host: String, scanner: Arc<Scanner>) -> std::io::Result<()> {
    let listener = TcpListener::bind(addr)?;
    log!("serving on {addr}");
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let (host, scanner) = (host.clone(), Arc::clone(&scanner));
        std::thread::spawn(move || {
            let Ok(reader) = stream.try_clone() else {
                return;
            };
            if let Err(e) = serve_stream(BufReader::new(reader), stream, &host, &scanner) {
                log!("connection failed: {e}");
            }
        });
    }
    Ok(())
}

/// Answer requests from `input` until it closes.
pub fn serve_stream(
    input: impl BufRead,
    mut out: impl Write,
    host: &str,
    scanner: &Scanner,
) -> std::io::Result<()> {
    for line in input.lines() {
        let (ok, body) = answer(line?.trim(), host, scanner);
        writeln!(out, "{} {}", if ok { "ok" } else { "err" }, body.len())?;
        out.write_all(&body)?;
        out.flush()?;
    }
    Ok(())
}

fn answer(request: &str, host: &str, scanner: &Scanner) -> (bool, Vec<u8>) {
    match request.split_once(' ').unwrap_or((request, "")) {
        ("health", _) => {
            let body = serde_json::json!({ "host": host, "version": env!("CARGO_PKG_VERSION") });
            (true, body.to_string().into_bytes())
        }
        ("manifest", _) => match serde_json::to_vec(&scanner.manifest()) {
            Ok(body) => (true, body),
            Err(e) => (false, e.to_string().into_bytes()),
        },
        ("file", hex) => match hex_decode(hex).and_then(|rel| scanner.read_canonical(&rel)) {
            Some(bytes) => (true, bytes),
            None => (false, b"not found".to_vec()),
        },
        _ => (false, b"unknown request".to_vec()),
    }
}

/// An open session with a peer's `claude-mesh serve`.
pub struct Peer {
    /// The `airshell-connect` process carrying the stream, if any.
    child: Option<Arc<Mutex<Child>>>,
    input: Box<dyn Write + Send>,
    output: BufReader<Box<dyn Read + Send>>,
    /// The `health` answer it gave when we connected.
    pub health: String,
}

impl Peer {
    /// Open `route`: `tcp:<addr>` connects straight to a `serve` (tests,
    /// debugging); anything else is a command whose stdin/stdout is the
    /// stream, such as `airshell-connect caladan-claudesync --transport quic`.
    /// The route must answer `health` within [`CONNECT_TIMEOUT`].
    pub fn connect(route: &[String]) -> std::io::Result<Peer> {
        match route {
            [one] if one.starts_with("tcp:") => Self::tcp(&one["tcp:".len()..]),
            _ => Self::command(route),
        }
    }

    fn tcp(addr: &str) -> std::io::Result<Peer> {
        let stream = TcpStream::connect(addr)?;
        stream.set_read_timeout(Some(CONNECT_TIMEOUT))?;
        let reader = stream.try_clone()?;
        let mut peer = Peer {
            child: None,
            input: Box::new(stream.try_clone()?),
            output: BufReader::new(Box::new(reader)),
            health: String::new(),
        };
        peer.health = String::from_utf8_lossy(&peer.request("health")?).to_string();
        stream.set_read_timeout(None)?;
        Ok(peer)
    }

    fn command(argv: &[String]) -> std::io::Result<Peer> {
        let (program, args) = argv.split_first().ok_or_else(|| other("empty route"))?;
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let (Some(stdin), Some(stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            return Err(other("no pipes"));
        };
        let child = Arc::new(Mutex::new(child));
        // A route that hangs (peer not found, AWDL down) is killed.
        let (done, watch) = mpsc::channel::<()>();
        let watchdog = {
            let child = Arc::clone(&child);
            std::thread::spawn(move || {
                if watch.recv_timeout(CONNECT_TIMEOUT).is_err()
                    && let Ok(mut c) = child.lock()
                {
                    let _ = c.kill();
                }
            })
        };
        let mut peer = Peer {
            child: Some(child),
            input: Box::new(stdin),
            output: BufReader::new(Box::new(stdout)),
            health: String::new(),
        };
        let health = peer.request("health");
        let _ = done.send(());
        let _ = watchdog.join();
        match health {
            Ok(body) => {
                peer.health = String::from_utf8_lossy(&body).to_string();
                Ok(peer)
            }
            Err(e) => {
                // Kill it first, or reading stderr could wait forever.
                drop(peer);
                // The route says why on stderr (not found, certificate refused, ...).
                let mut why = String::new();
                let _ = BufReader::new(stderr).read_to_string(&mut why);
                let why = why.lines().rfind(|l| !l.trim().is_empty()).unwrap_or("");
                let msg = if why.is_empty() {
                    e.to_string()
                } else {
                    why.to_string()
                };
                Err(other(&msg))
            }
        }
    }

    /// Send one request; the body of an `ok`, an error otherwise.
    pub fn request(&mut self, request: &str) -> std::io::Result<Vec<u8>> {
        writeln!(self.input, "{request}")?;
        self.input.flush()?;
        let mut header = String::new();
        if self.output.read_line(&mut header)? == 0 {
            return Err(other("connection closed"));
        }
        let (status, len) = header
            .trim()
            .split_once(' ')
            .ok_or_else(|| other(&format!("bad header {header:?}")))?;
        let len: usize = len
            .parse()
            .map_err(|_| other(&format!("bad header {header:?}")))?;
        let mut body = vec![0; len];
        self.output.read_exact(&mut body)?;
        match status {
            "ok" => Ok(body),
            _ => Err(other(&format!(
                "{request}: {}",
                String::from_utf8_lossy(&body)
            ))),
        }
    }

    pub fn file(&mut self, rel: &str) -> std::io::Result<Vec<u8>> {
        self.request(&format!("file {}", hex_encode(rel)))
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        if let Some(child) = &self.child
            && let Ok(mut c) = child.lock()
        {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

fn other(msg: &str) -> std::io::Error {
    std::io::Error::other(msg.to_string())
}

pub fn hex_encode(s: &str) -> String {
    s.bytes().map(|b| format!("{b:02x}")).collect()
}

pub fn hex_decode(s: &str) -> Option<String> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    let bytes: Option<Vec<u8>> = (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect();
    String::from_utf8(bytes?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trip() {
        let p = "projects/@SRCKEY@-x/ä b.jsonl";
        assert_eq!(hex_decode(&hex_encode(p)).as_deref(), Some(p));
        assert_eq!(hex_decode("zz"), None);
        assert_eq!(hex_decode("abc"), None);
    }

    #[test]
    fn a_route_that_never_answers_fails() {
        let started = std::time::Instant::now();
        let err = Peer::connect(&["false".to_string()])
            .err()
            .map(|e| e.to_string());
        assert!(err.is_some());
        assert!(started.elapsed() < CONNECT_TIMEOUT);
    }
}
