# Setting up claude-mesh on the three Macs

Do steps 1–2 and 4–5 on **each** Mac (arrakis, caladan, imperium). Step 3 pairs them up.

## Before you start

- **airshell-proxy with QUIC services and `[[forward]]` support** on every Mac (the build the term-llm hub uses; see airshell's `docs/howto/term-llm-hub-and-nodes.md`, step 1). Check: `launchctl print gui/$(id -u)/org.nixos.airshell-proxy | grep -m1 program`.
- **Rust** (`cargo`), already on every Mac through nix-darwin.
- **The repo on each Mac.** It isn't on GitHub yet; copy it or push it somewhere first.
- **arrakis is the company machine.** Running a personal sync daemon there is your call; skip it there if policy says no. Two Macs are a valid mesh.

## 1. Build

```sh
cd ~/src/pplanel/crazy_project   # arrakis; ~/src/crazy_project on caladan and imperium
cargo build --release
./target/release/claude-mesh status
```

`status` should print this Mac's name and its Claude config folder:

| Mac | Config folder |
|---|---|
| arrakis | `/Users/pplanel/src/pplanel/.claude` |
| caladan, imperium | `/Users/pplanel/.claude` |

If it says the host isn't in the layout table, add it to `hosts()` in `src/layout.rs` (same table as `claude-handoff`'s `layout()`).

## 2. airshell entries

```sh
./target/release/claude-mesh airshell-config >> ~/.config/airshell/config.toml
```

This adds:

- **One `[[service]]`** named `<this-mac>-claudesync` (QUIC, `_claudesync._udp`) that relays to `127.0.0.1:7480`, where `claude-mesh serve` listens.
- **One `[[forward]]` per peer** named `<peer>-claudesync`, listening on a fixed local port per peer:

  | Peer | Local port on the other Macs |
  |---|---|
  | arrakis | 7481 |
  | caladan | 7482 |
  | imperium | 7483 |

The proxy reloads the file within about 5 seconds. Check:

```sh
dns-sd -B _claudesync._udp      # lists the other Macs' -claudesync services (Ctrl-C to stop)
```

## 3. Authorize the Macs to each other

airshell services only accept Macs listed in `~/.config/airshell/authorized_peers`. Each Mac must list the other two.

On each Mac, print its fingerprint:

```sh
airshell-connect --print-fingerprint
```

Then on each Mac, add the **other two** fingerprints (one line each: `<64 hex> <label>`):

```sh
(umask 077 && echo "<fingerprint> caladan" >> ~/.config/airshell/authorized_peers)
```

The file is read on every connection, so it applies immediately. If you've already set up the term-llm hub, some of these lines may already be there.

## 4. Run it at login

```sh
./target/release/claude-mesh launchd-plist > ~/Library/LaunchAgents/dev.pplanel.claude-mesh.plist
launchctl load ~/Library/LaunchAgents/dev.pplanel.claude-mesh.plist
```

The plist points at the binary you built; rebuild in place to upgrade, then `launchctl kickstart -k gui/$(id -u)/dev.pplanel.claude-mesh`. (A nix-darwin module would be the tidy long-term home for this.)

## 5. Check

```sh
./target/release/claude-mesh status
# peer caladan   127.0.0.1:7482   {"host":"caladan","version":"0.1.0"}
# peer imperium  127.0.0.1:7483   {"host":"imperium","version":"0.1.0"}
tail -f ~/Library/Logs/claude-mesh.log
```

On first contact, each Mac pulls everything the others have; expect a burst of `pulled N` lines. Then:

```sh
claude --resume        # (claude-personal on arrakis) — sessions from the other Macs appear
```

## The first sync, safely

The first round merges three histories that were never compared. Before enabling it everywhere, back up each config folder:

```sh
tar czf ~/claude-backup-$(hostname -s)-$(date +%F).tgz -C ~ .claude          # caladan, imperium
tar czf ~/claude-backup-arrakis-$(date +%F).tgz -C ~/src/pplanel .claude     # arrakis
```

Then enable the Macs one at a time and watch the log.
