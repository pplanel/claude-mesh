# claude-mesh

Makes Claude Code on **arrakis, caladan and imperium** behave like one computer with one account: the same conversations, memory, plans, todos and prompt history on every Mac, kept in sync continuously over [airshell](https://github.com/pplanel/airshell) (QUIC, over the LAN or AWDL when there's no network).

Today `claude-handoff` copies one session and its memory when you move it. `claude-mesh` keeps *everything* converged all the time, so `claude --resume` on any Mac lists every session from every Mac.

```
 caladan                         imperium                        arrakis
 ~/.claude                       ~/.claude                       ~/src/pplanel/.claude
 claude-mesh serve ◀──QUIC──▶ claude-mesh serve ◀──QUIC──▶ claude-mesh serve
   127.0.0.1:7480  airshell-proxy  127.0.0.1:7480  airshell-proxy  127.0.0.1:7480
```

## What it does

- **Every 30 seconds, each Mac pulls from the other two.** It asks each peer for a manifest (path, hash, size, mtime of every synced file), fetches what changed, merges it into its own Claude folder, and never writes to another Mac.
- **Paths are rewritten between layouts.** arrakis keeps sources in `~/src/pplanel` and Claude's config in `~/src/pplanel/.claude`; caladan and imperium use `~/src` and `~/.claude`. Project folders (`projects/-Users-pplanel-src-pplanel-x` ↔ `projects/-Users-pplanel-src-x`) and paths inside transcripts are translated, the same way `claude-handoff` does it.
- **Conversations merge, they don't overwrite.** Transcripts are append-only logs: the longer one wins, and two branches of the same session are both kept. Memory indexes (`MEMORY.md`) keep every entry from every Mac. Other files: newest wins, and the losing version is saved next to it as `name.conflict-<host>.ext`.
- **Live sessions are left alone.** A file written in the last 60 seconds isn't touched; growth is written as an append, never a replacement.
- **Secrets stay put.** Only `projects/`, `plans/`, `todos/`, `tasks/`, `file-history/`, `agents/`, `commands/` and `history.jsonl` are synced. Credentials, `settings.json`, plugins, caches, quota marks and per-process state never leave a Mac.

## Quick start

On **each** Mac (details and checks in [docs/setup.md](docs/setup.md)):

```sh
cd ~/src/pplanel/claude-mesh                 # arrakis; ~/src/claude-mesh on caladan and imperium
cargo build --release
./target/release/claude-mesh airshell-config   # append its output to ~/.config/airshell/config.toml
# authorize the other two Macs' airshell fingerprints (docs/setup.md, step 3)
./target/release/claude-mesh launchd-plist > ~/Library/LaunchAgents/dev.pplanel.claude-mesh.plist
launchctl load ~/Library/LaunchAgents/dev.pplanel.claude-mesh.plist
./target/release/claude-mesh status            # both peers should answer
```

## Commands

| Command | What it does |
|---|---|
| `claude-mesh serve [--interval SECS]` | Serve this Mac's state and pull from peers forever (default every 30 s). This is what launchd runs. |
| `claude-mesh once` | One pull round against every peer, then exit. |
| `claude-mesh status` | This Mac, its config folder, how many files it syncs, and whether each peer answers. |
| `claude-mesh airshell-config` | The `[[service]]` and `[[forward]]` entries this Mac needs in airshell-proxy. |
| `claude-mesh launchd-plist` | A launchd agent that runs `serve` at login and restarts it. |

## Docs

- [docs/setup.md](docs/setup.md): installing on the three Macs, airshell entries, authorization, checks.
- [docs/design.md](docs/design.md): how it works, the canonical form, merge rules, why it converges.
- [docs/operations.md](docs/operations.md): logs, troubleshooting, known limits, removing it.

## Development

```sh
cargo test                      # unit tests + an end-to-end test with three real processes
cargo clippy --all-targets -- -D warnings
cargo fmt --all
```

The end-to-end tests (`tests/mesh.rs`) start three `claude-mesh` processes, one per Mac layout, with temporary config folders and talk over localhost instead of airshell.

Rust 2024, three dependencies (`serde`, `serde_json`, `blake3`), a hand-rolled HTTP/1.1 server and client on `std`, no async runtime.

## Origin

Written by Claude (Opus 5.5) on 2026-10-08, in one go, while Pedro was on a break: "create a program to sync the folders that handoff copies, on 3 machines, as if it was 1 computer and 1 account, using airshell over AWDL/QUIC." Pedro asked for good docs, then for simplicity. Every decision above is open to his review.
