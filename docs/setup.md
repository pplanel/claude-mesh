# Setting up claude-mesh on the Macs

nix-darwin does most of it (`modules/darwin/claude-mesh.nix`): it runs `claude-mesh serve` at login as the user agent `dev.pplanel.claude-mesh` and writes the layout table to `/etc/claude-mesh/hosts.json`. What's left per Mac is building the binary and pairing the Macs in airshell.

## Before you start

- **airshell-proxy running as a user agent** (`airshell.nix`), with its control socket (airshell from AIR-53 on). Check: `ls "$(getconf DARWIN_USER_TEMP_DIR)"airshell-proxy.sock`.
- **The repo on each Mac.** src-sync keeps it at `~/src/pplanel/claude-mesh` on arrakis and `~/src/claude-mesh` elsewhere.
- **You don't need all three Macs.** Two Macs are a valid mesh; a Mac that's away is skipped until it's back.

## 1. Build

```sh
cd ~/src/claude-mesh          # ~/src/pplanel/claude-mesh on arrakis
cargo build --release
./target/release/claude-mesh status
```

`status` prints this Mac's name and its Claude config folder (from the layout table), then asks each peer for `health`.

## 2. Pair the Macs in airshell

airshell admits only Macs listed in `~/.config/airshell/authorized_peers`. Each Mac must list the other two.

On each Mac, print its fingerprint (its Secure Enclave key):

```sh
airshell-connect --print-fingerprint
```

Then on each Mac, add the **other two** (one line each: `<64 hex> <label>`):

```sh
(umask 077 && echo "<fingerprint> caladan" >> ~/.config/airshell/authorized_peers)
```

The file is read on every connection, so it applies immediately. A peer's certificate is pinned in `known_peers` the first time this Mac dials it.

When the Macs join an airshell mesh, this step goes away: a QUIC service with no `allow` rule admits the devices of its own member, which is all three Macs.

## 3. Start it

With nix-darwin, it starts at login; after building, restart it:

```sh
launchctl kickstart -k gui/$(id -u)/dev.pplanel.claude-mesh
```

On start, `serve` adds its service to airshell-proxy through the control socket. The log says so:

```
[1791745142] serving on 127.0.0.1:7480
[1791745142] airshell: added arrakis-claudesync to /Users/pplanel/.config/airshell/config.toml (started ["arrakis-claudesync"])
```

`claude-mesh register` does only that step, for checking. The first time it dials a peer, macOS may ask whether claude-mesh can find devices on the local network: allow it.

## 4. Check

```sh
dns-sd -B _claudesync._udp            # the other Macs' services (Ctrl-C to stop)
./target/release/claude-mesh status
# peer caladan   {"host":"caladan","version":"0.1.0"}
# peer imperium  {"host":"imperium","version":"0.1.0"}
tail -f ~/Library/Logs/claude-mesh.log
```

On first contact, each Mac pulls everything the others have; expect a burst of `pulled N` lines (on 2026-10-11: 1478 files from caladan in about two minutes). Then `claude --resume` (`claude-personal` on arrakis) lists sessions from every Mac.

## The first sync, safely

The first round merges histories that were never compared. Back up each config folder first:

```sh
mkdir -p ~/claude-backups
tar czf ~/claude-backups/claude-$(hostname -s)-$(date +%F).tgz -C ~ .claude          # caladan, imperium
tar czf ~/claude-backups/claude-arrakis-$(date +%F).tgz -C ~/src/pplanel .claude     # arrakis
```

Then enable the Macs one at a time and watch the log.

## Without nix-darwin

Run `./target/release/claude-mesh serve` from a terminal (not from an ssh session: processes started over ssh don't get the Local Network grant, so they can serve but not dial). Without `/etc/claude-mesh/hosts.json`, the built-in layout table is used; `CLAUDE_MESH_LAYOUT` points at another file.
