# Design

## Goal

Claude Code on three Macs, three accounts, should feel like one computer and one account: any session can be resumed anywhere, memory written on one Mac is known on the others, prompt history is shared. Without a server, and working over AWDL when the Macs have no network in common.

## Shape

```
      ┌──────────── this Mac ────────────┐
      │ claude-mesh serve                 │
      │  ├─ HTTP on 127.0.0.1:7480 ◀──────┼── airshell [[service]] <me>-claudesync ◀── peers
      │  └─ every 30 s, for each peer:    │
      │       GET 127.0.0.1:748x ─────────┼──▶ airshell [[forward]] <peer>-claudesync ──▶ peer
      └──────────────────────────────────┘
```

- **Transport is airshell's problem.** `claude-mesh` speaks plain HTTP on localhost. airshell-proxy authenticates peers (each Mac's `authorized_peers`, certificate pinning in `known_peers`), encrypts with QUIC, and picks the LAN or AWDL. Nothing in `claude-mesh` handles keys or the network.
- **Pull-only.** A Mac only writes its own files. Peers expose read-only routes (`/health`, `/manifest`, `/file/<hex path>`).
- **No coordinator.** Every Mac pulls from every other; there's no leader and nothing to elect.

## What is synced

Top-level entries of the Claude config folder, listed in `INCLUDE` (`src/scan.rs`):

| Synced | Why |
|---|---|
| `projects/` | transcripts (`<session>.jsonl`), subagent transcripts, tool results, per-project `memory/` |
| `plans/`, `todos/`, `tasks/` | plan-mode files and task lists |
| `file-history/` | snapshots used to rewind edits |
| `agents/`, `commands/` | user-defined subagents and slash commands |
| `history.jsonl` | prompt history (up-arrow) |

Everything else stays: `.credentials.json`, `settings.json` (nix-managed), `plugins/`, `skills/` (nix-managed symlinks), `cache/`, `sessions/` and `session-env/` (per-process), `shell-snapshots/`, `daemon*`, `quota/` (the handoff hook's per-Mac marks), `backups/`. Symlinks are never followed or synced. Files over 64 MiB are skipped.

## Canonical form

The same conversation lives at different paths on different Macs and mentions different absolute paths inside. Before hashing or sending, each Mac rewrites into **canonical form** (`src/canon.rs`):

| Local | Canonical |
|---|---|
| the Claude config folder (e.g. `/Users/pplanel/src/pplanel/.claude`) | `@CFG@` |
| the source folder (e.g. `/Users/pplanel/src/pplanel`) | `@SRC@` |
| a project folder name under the source folder (`-Users-pplanel-src-pplanel-x`) | `@SRCKEY@-x` |

The config folder is replaced first because on arrakis it sits inside the source folder. Project keys only match at a `-` boundary. Only text files (`.jsonl`, `.json`, `.md`, `.txt`) are rewritten; anything else is copied byte for byte. The receiving Mac turns the tokens back into its own paths.

Hashes (BLAKE3) are computed on canonical bytes, so the same transcript on arrakis and imperium has the same hash even though the files differ on disk.

## Deciding what to do with a file

For every file in a peer's manifest (`src/sync.rs`):

1. **The peer's hash is the one we saw last time** → skip. (Per-peer memory in the state file.)
2. **Same hash as ours** → record it, nothing to write.
3. **Our copy was modified in the last 60 s** → skip this round; a live session may be writing it.
4. **Fetch and merge.** Re-check the hash of what arrived (the file may have changed between manifest and fetch), merge, and write only if the result differs from our copy.

The state file (`~/.local/state/claude-mesh/state.json`, outside the synced folder) remembers, per path, our hash right after the last sync. If our copy still has that hash, *we* haven't changed it, so a peer's newer version simply replaces it. If both changed, it's a conflict.

## Merge rules

All merges are **symmetric**, `merge(a, b) == merge(b, a)`, which is what makes the Macs converge to the same bytes (`src/merge.rs`).

| Files | Rule |
|---|---|
| `*.jsonl` (transcripts, history) | If one is a prefix of the other, the longer one. Otherwise the common prefix, then each side's new lines as a block, blocks ordered by their first `timestamp`, lines already present skipped. Claude transcripts are trees (`parentUuid`), so two branches side by side are still valid. |
| `memory/MEMORY.md` | Line union: every entry from both, once. |
| Everything else | If only the peer changed it, theirs. If both did, newest mtime wins (ties broken by size, then bytes) and the losing version is written next to it as `name.conflict-<host>.ext`, which then syncs too. |

Writes are atomic (temp file + rename) except when a log only grew: then the new lines are **appended**, so a session that resumes writing mid-sync never sees its file swapped underneath it.

## Why it converges

Each merge is symmetric and idempotent (merging a result with either input gives the result back), and a merged file contains everything both inputs had. Every Mac pulls from both others, so after two rounds everyone has seen every change and holds the same content. The end-to-end test `tests/mesh.rs` checks this with three processes and three layouts.

## Deliberately not done (yet)

- **Deletions don't propagate.** Deleting a session or memory file on one Mac doesn't delete it elsewhere; it comes back from a peer. Safer for a first version; tombstones would fix it.
- **Whole-file transfer.** A changed transcript is fetched whole. Fine at today's sizes (tens of MB in total); content-defined chunking is what shift-compute is for.
- **No push.** Changes arrive within one interval (30 s), not instantly.
