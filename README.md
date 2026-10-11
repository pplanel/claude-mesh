# claude-mesh

Makes Claude Code on **arrakis, caladan and imperium** behave like one computer with one account: the same conversations, memory, plans, todos and prompt history on every Mac, kept in sync continuously over [airshell](https://github.com/pplanel/airshell) (QUIC with Secure Enclave keys, over the LAN or AWDL when there's no network).

Today `claude-handoff` copies one session and its memory when you move it. `claude-mesh` keeps *everything* converged all the time, so `claude --resume` on any Mac lists every session from every Mac.

```
 caladan                         imperium                        arrakis
 ~/.claude                       ~/.claude                       ~/src/pplanel/.claude
 claude-mesh serve               claude-mesh serve               claude-mesh serve
   airshell-proxy  ◀───QUIC───▶   airshell-proxy  ◀───QUIC───▶   airshell-proxy
```

## What it does

- **airshell does the networking.** Each Mac's `serve` registers a `<host>-claudesync` service with its airshell-proxy and dials the others with `airshell-connect`. airshell admits only paired Macs; claude-mesh holds no keys and opens no network sockets.
- **Every 30 seconds, each Mac pulls from the other two.** It asks each peer for a manifest (path, hash, size, mtime of every synced file), fetches what changed, merges it into its own Claude folder, and never writes to another Mac.
- **Paths are rewritten between layouts.** arrakis keeps sources in `~/src/pplanel` and Claude's config in `~/src/pplanel/.claude`; caladan and imperium use `~/src` and `~/.claude`. Project folders (`projects/-Users-pplanel-src-pplanel-x` ↔ `projects/-Users-pplanel-src-x`) and paths inside transcripts are translated, the same way `claude-handoff` does it.
- **Conversations merge, they don't overwrite.** Transcripts are append-only logs: the longer one wins, and two branches of the same session are both kept. Memory indexes (`MEMORY.md`) keep every entry from every Mac. Other files: newest wins, and the losing version is saved next to it as `name.conflict-<host>.ext`.
- **Live sessions are left alone.** A file written in the last 60 seconds isn't touched; growth is written as an append, never a replacement.
- **Secrets stay put.** Only `projects/`, `plans/`, `todos/`, `tasks/`, `file-history/`, `agents/`, `commands/` and `history.jsonl` are synced. Credentials, `settings.json`, plugins, caches, quota marks and per-process state never leave a Mac.

## Quick start

[claude-code-nix](https://github.com/pplanel/claude-code-nix)'s `programs.claude-code-nix.mesh` runs it on every Mac as a launchd agent, with the hosts from its handoff table. Then on **each** Mac (details and checks in [docs/setup.md](docs/setup.md)):

```sh
# pair the Macs: each one's `airshell-connect --print-fingerprint` in the
# others' ~/.config/airshell/authorized_peers (docs/setup.md, step 1)
claude-mesh status            # both peers should answer
tail -f ~/Library/Logs/claude-mesh.log
```

## Commands

| Command | What it does |
|---|---|
| `claude-mesh serve [--interval SECS]` | Serve this Mac's state on `127.0.0.1:7480`, register it with airshell-proxy, and pull from peers forever (default every 30 s). This is what launchd runs. |
| `claude-mesh once` | One pull round against every peer, then exit. |
| `claude-mesh status` | This Mac, its config folder, how many files it syncs, and whether each peer answers. |
| `claude-mesh register` | Add this Mac's `[[service]]` to airshell-proxy through its control socket, then exit. |

## Docs

- [docs/codebase.md](docs/codebase.md): a guided tour of the code, module by module, with diagrams. The reference is the rustdoc: `cargo doc --document-private-items --open`.
- [docs/setup.md](docs/setup.md): installing on the three Macs, airshell entries, authorization, checks.
- [docs/design.md](docs/design.md): how it works, the canonical form, merge rules, why it converges.
- [docs/operations.md](docs/operations.md): logs, troubleshooting, known limits, removing it.

## Development

```sh
cargo test                      # unit tests + an end-to-end test with three real processes
cargo clippy --all-targets -- -D warnings
cargo fmt --all
```

The end-to-end tests (`tests/mesh.rs`) start three `claude-mesh` processes, one per Mac layout, with temporary config folders and talk over localhost (`tcp:` routes, and one through `nc` as a stand-in for `airshell-connect`) instead of airshell.

Rust 2024, three dependencies (`serde`, `serde_json`, `blake3`), a line protocol on `std` threads, no async runtime.

## Origin

Written by Claude (Opus 5.5) on 2026-10-08, in one go, while Pedro was on a break: "create a program to sync the folders that handoff copies, on 3 machines, as if it was 1 computer and 1 account, using airshell over AWDL/QUIC." Pedro asked for good docs, then for simplicity. Every decision above is open to his review.
