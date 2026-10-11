# Operations

## Logs

`serve` logs to stderr; under launchd that's `~/Library/Logs/claude-mesh.log`. It's quiet unless something happened:

```
[1791745142] airshell: added arrakis-claudesync to /Users/pplanel/.config/airshell/config.toml (started ["arrakis-claudesync"])
[1791745147] caladan: pulled 12, merged 1, conflicts 0, busy 2
[1791745245] imperium: unreachable (connection closed)
[1791480060] conflict on plans/x.md: kept imperium's version, ours saved as plans/x.conflict-arrakis.md
```

- **pulled**: files we didn't have, or took as-is.
- **merged**: files combined from both sides.
- **conflicts**: our version lost and was saved as a `.conflict-<host>` copy.
- **busy**: files skipped because they were written in the last minute; they're retried next round.

The airshell side is in airshell-proxy's log: `grep claudesync ~/Library/Logs/airshell-proxy.err.log | tail`. Each connection from a peer shows as `client allowed … fingerprint=<peer>`, then `accepted` and `closed` with the bytes moved.

## Troubleshooting

| Symptom | Likely cause | Fix |
|---|---|---|
| Log: `airshell: can't register (… is airshell-proxy running?)` | The proxy isn't running, or is too old for the control socket | `launchctl list \| grep airshell`; peers can't reach this Mac until it registers (restart `serve` once the proxy is up) |
| `status`: a peer is `not reachable: … not found` | The peer's `serve` isn't running, so its proxy doesn't advertise `<peer>-claudesync` | `dns-sd -B _claudesync._udp`; `launchctl list \| grep claude-mesh` on the peer |
| `status`: `not reachable: … add the output of airshell-connect --print-fingerprint there` | This Mac isn't in the peer's `authorized_peers` | [setup step 2](setup.md#2-pair-the-macs-in-airshell) |
| `unreachable (connection closed)` for every peer, while peers still pull from this Mac | macOS's prompt to allow claude-mesh's connections is waiting on this Mac's screen (it can serve, not dial) | Accept it on that Mac; the next round connects |
| airshell log: `peer certificate changed; refusing` | A Mac's airshell identity was recreated | Remove its line from `~/.config/airshell/known_peers` if expected |
| `status`: host isn't in the layout table | A new Mac or a different `hostname -s` | Add it to `handoff.hosts` (nix-darwin's `claude/hosts.nix`), or set `CLAUDE_MESH_HOST` |
| A session appears twice in `claude --resume` | The same session diverged on two Macs (both kept, as branches) | Expected; resume either |
| `*.conflict-<host>.*` files appear | Both Macs edited the same non-log file between syncs | Pick the right one, delete the other on **every** Mac (deletions don't sync) |

## Known limits and risks

- **Deletions don't sync.** A file deleted on one Mac comes back from the others. To really delete, stop `claude-mesh` everywhere, delete on every Mac, start again.
- **Path rewriting is textual.** Inside transcripts, every occurrence of this Mac's source and config paths is rewritten, including paths a conversation merely *quotes* (e.g. a transcript on caladan that mentions `/Users/pplanel/src/pplanel/...` as text will read `/Users/pplanel/src/pplanel/pplanel/...` on arrakis). `claude-handoff` rewrites paths the same textual way.
- **Same session open on two Macs at once.** Both keep appending; the logs diverge and are merged as two branches. Claude resumes one branch; nothing is lost, but it's confusing. Use one Mac per session at a time, as with handoff.
- **Local access.** The server binds `127.0.0.1` without auth: any process on this Mac can read the synced files through it. They are your own files under your account, already readable by those processes.
- **Anyone airshell admits can read.** Every Mac in `authorized_peers` (or, later, every device of your mesh member) can fetch all synced files, including transcripts. Pair only your own Macs.
- **Full rescans.** Each round lists the synced folders; unchanged files are hashed once and cached by (size, mtime).

## Removing it

Set `programs.claude-code-nix.mesh.enable = false` (it stops the agent everywhere once applied). Then, on each Mac:

```sh
# delete the <host>-claudesync [[service]] from ~/.config/airshell/config.toml
rm -r ~/.local/state/claude-mesh
```

Synced files stay where they are; nothing in the Claude folders depends on `claude-mesh`.
