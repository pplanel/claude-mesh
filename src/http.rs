//! A deliberately tiny HTTP/1.1 server and client over std TCP.
//!
//! The server only ever binds `127.0.0.1`: other Macs reach it through
//! airshell-proxy, which authenticates them (its `authorized_peers`) and
//! carries the bytes over QUIC (LAN or AWDL). There's no TLS here because
//! the bytes never leave this Mac unencrypted.
//!
//! Routes (all `GET`, `Connection: close`):
//!
//! | Path           | Body                                              |
//! |----------------|---------------------------------------------------|
//! | `/health`      | `{"host": "...", "version": "..."}`               |
//! | `/manifest`    | the [`Manifest`](crate::scan::Manifest) as JSON   |
//! | `/file/<hex>`  | canonical bytes of the canonical path `<hex>`     |

use crate::scan::Scanner;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Duration;

const IO_TIMEOUT: Duration = Duration::from_secs(60);

pub fn serve(addr: &str, host: String, scanner: Arc<Scanner>) -> std::io::Result<()> {
    let listener = TcpListener::bind(addr)?;
    log!("serving on {addr}");
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let scanner = Arc::clone(&scanner);
        let host = host.clone();
        std::thread::spawn(move || {
            if let Err(e) = handle(stream, &host, &scanner) {
                log!("request failed: {e}");
            }
        });
    }
    Ok(())
}

fn handle(stream: TcpStream, host: &str, scanner: &Scanner) -> std::io::Result<()> {
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    // Drain the headers; nothing in them matters to us.
    let mut header = String::new();
    while reader.read_line(&mut header)? > 2 {
        header.clear();
    }
    let mut parts = request_line.split_whitespace();
    let (method, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    let mut out = stream;
    if method != "GET" {
        return respond(&mut out, 405, b"method not allowed");
    }
    match path {
        "/health" => {
            let body = serde_json::json!({ "host": host, "version": env!("CARGO_PKG_VERSION") });
            respond(&mut out, 200, body.to_string().as_bytes())
        }
        "/manifest" => {
            let body = serde_json::to_vec(&scanner.manifest()).unwrap_or_default();
            respond(&mut out, 200, &body)
        }
        p if p.starts_with("/file/") => {
            let Some(rel) = hex_decode(&p["/file/".len()..]) else {
                return respond(&mut out, 400, b"bad path");
            };
            match scanner.read_canonical(&rel) {
                Some(bytes) => respond(&mut out, 200, &bytes),
                None => respond(&mut out, 404, b"not found"),
            }
        }
        _ => respond(&mut out, 404, b"not found"),
    }
}

fn respond(out: &mut TcpStream, status: u16, body: &[u8]) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Error",
    };
    write!(
        out,
        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    out.write_all(body)?;
    out.flush()
}

/// `GET http://<addr><path>`; returns the body of a 200, an error otherwise.
pub fn get(addr: &str, path: &str) -> std::io::Result<Vec<u8>> {
    let mut stream = TcpStream::connect(addr)?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n"
    )?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw)?;
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| other("malformed response"))?;
    let head = String::from_utf8_lossy(&raw[..split]);
    let status = head.split_whitespace().nth(1).unwrap_or("");
    if status != "200" {
        return Err(other(&format!("{path}: HTTP {status}")));
    }
    Ok(raw[split + 4..].to_vec())
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
}
