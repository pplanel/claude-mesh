//! The airshell side: how peers are reached, and registering this Mac's
//! service with the running airshell-proxy.
//!
//! airshell owns identity and transport. Each Mac's proxy advertises
//! `<host>-claudesync` (QUIC, `_claudesync._udp`) and relays it to the local
//! `claude-mesh serve`; peers dial it with `airshell-connect`. The proxy
//! admits only paired Macs (their Secure Enclave keys in `authorized_peers`,
//! later mesh membership), so claude-mesh does no authentication itself.
//!
//! The service is added through the proxy's control socket (`config.get`,
//! then `config.apply` with the file's hash), so nobody edits
//! `~/.config/airshell/config.toml` by hand and the proxy applies it at once.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

/// The Bonjour service type of every claude-mesh service.
pub const SERVICE_TYPE: &str = "_claudesync._udp";

pub fn service_name(host: &str) -> String {
    format!("{host}-claudesync")
}

/// The command that opens a stream to `peer`'s claude-mesh.
pub fn route(peer: &str) -> Vec<String> {
    let connect = std::env::var("CLAUDE_MESH_AIRSHELL_CONNECT")
        .unwrap_or_else(|_| "airshell-connect".to_string());
    vec![
        connect,
        service_name(peer),
        "--transport".into(),
        "quic".into(),
        "--service-type".into(),
        SERVICE_TYPE.into(),
        "--timeout".into(),
        "15".into(),
    ]
}

/// The `[[service]]` entry for this Mac.
pub fn service_entry(host: &str, port: u16) -> String {
    format!(
        "\n# Added by claude-mesh: peers reach this Mac's `claude-mesh serve` here.\n\
         [[service]]\n\
         name = \"{}\"\n\
         transport = \"quic\"\n\
         service_type = \"{SERVICE_TYPE}\"\n\
         host = \"127.0.0.1\"\n\
         port = {port}\n",
        service_name(host)
    )
}

/// Whether `config` already has this Mac's service.
fn has_service(config: &str, host: &str) -> bool {
    let line = format!("name = \"{}\"", service_name(host));
    config.lines().any(|l| l.trim_start().starts_with(&line))
}

/// Make sure the proxy advertises this Mac's service. Returns what happened.
pub fn register(host: &str, port: u16) -> Result<String, String> {
    let mut sock = Control::open()?;
    let config = sock.call(json!({"method": "config.get"}))?;
    let text = config["text"].as_str().unwrap_or_default();
    if has_service(text, host) {
        return Ok(format!(
            "{} already in the proxy's config",
            service_name(host)
        ));
    }
    let mut new = text.to_string();
    if !new.is_empty() && !new.ends_with('\n') {
        new.push('\n');
    }
    new.push_str(&service_entry(host, port));
    let applied = sock.call(json!({
        "method": "config.apply",
        "params": {"text": new, "base_hash": config["hash"]},
    }))?;
    Ok(format!(
        "added {} to {} (started {})",
        service_name(host),
        config["path"].as_str().unwrap_or("the proxy's config"),
        applied["started"]
    ))
}

/// A connection to airshell-proxy's control socket.
struct Control {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
    next_id: u64,
}

impl Control {
    fn open() -> Result<Control, String> {
        let path = socket_path().ok_or("can't find the per-user temp dir")?;
        let stream = UnixStream::connect(&path)
            .map_err(|e| format!("{}: {e} (is airshell-proxy running?)", path.display()))?;
        let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
        let reader = BufReader::new(stream.try_clone().map_err(|e| e.to_string())?);
        Ok(Control {
            stream,
            reader,
            next_id: 1,
        })
    }

    /// Send `{"v":1,"id":…, method, params}`; the `ok` value, or the error.
    fn call(&mut self, mut request: Value) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        request["v"] = json!(1);
        request["id"] = json!(id);
        let mut line = request.to_string();
        line.push('\n');
        self.stream
            .write_all(line.as_bytes())
            .map_err(|e| e.to_string())?;
        loop {
            let mut line = String::new();
            if self
                .reader
                .read_line(&mut line)
                .map_err(|e| e.to_string())?
                == 0
            {
                return Err("the proxy closed the control socket".into());
            }
            let response: Value = serde_json::from_str(&line).map_err(|e| e.to_string())?;
            if response["id"] != json!(id) {
                continue;
            }
            if let Some(err) = response.get("err") {
                return Err(format!("{}: {}", err["code"], err["message"]));
            }
            return Ok(response["ok"].clone());
        }
    }
}

/// `<per-user temp dir>/airshell-proxy.sock`, as airshell-proto finds it:
/// from `confstr(_CS_DARWIN_USER_TEMP_DIR)`, not `$TMPDIR` (unset under ssh
/// and some launchd jobs). `CLAUDE_MESH_AIRSHELL_SOCKET` overrides it.
fn socket_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("CLAUDE_MESH_AIRSHELL_SOCKET") {
        return Some(PathBuf::from(p));
    }
    let out = std::process::Command::new("/usr/bin/getconf")
        .arg("DARWIN_USER_TEMP_DIR")
        .output()
        .ok()?;
    let dir = String::from_utf8(out.stdout).ok()?;
    let dir = dir.trim();
    (!dir.is_empty()).then(|| PathBuf::from(dir).join("airshell-proxy.sock"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_entry_is_recognised_once_added() {
        let config = "[[service]]\nname = \"arrakis\"\nport = 22\n";
        assert!(!has_service(config, "arrakis"));
        let added = format!("{config}{}", service_entry("arrakis", 7480));
        assert!(has_service(&added, "arrakis"));
        assert!(added.contains("service_type = \"_claudesync._udp\""));
    }

    #[test]
    fn routes_dial_the_peers_service_over_quic() {
        let r = route("caladan");
        assert_eq!(r[1], "caladan-claudesync");
        assert!(r.windows(2).any(|w| w == ["--transport", "quic"]));
    }
}
