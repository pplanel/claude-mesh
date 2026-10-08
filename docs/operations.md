# Operations

## Logs

`serve` logs to stderr; under launchd that's `~/Library/Logs/claude-mesh.log`. It's quiet unless something happened:

```
[1791480000] caladan: pulled 12, merged 1, conflicts 0, busy 2
[1791480030] imperium (127.0.0.1:7483): Connection refused (os error 61)
[1791480060] conflict on plans/x.md: kept imperium's version, ours saved as plans/x.conflict-arrakis.md
```

- **pulled**: files we didn't have, or took as-is.
- **merged**: files combined from both sides.
- **conflicts**: our version lost and was saved as a `.conflict-<host>` copy.
- **busy**: files skipped because they were written in the last minute; they're retried next round.

The airshell side: `~/Library/Logs/airshell-proxy.json`, e.g. `jq -r 'select(.fields.service | tostring | test("claudesync"))' ~/Library/Logs/airshell-proxy.json`.

## Troubleshooting

| Symptom | Likely cause | Fix |
|---|---|---|
| `status` shows a peer as `Connection refused` | The forward isn't in this Mac's airshell config, or the proxy hasn't reloaded | `claude-mesh airshell-config`, compare with `~/.config/airshell/config.toml` |
| A peer request hangs ~30 s, then fails | Local Network not approved for airshell-proxy on one of the Macs, or the peer's `claude-mesh` isn't running | Approve the macOS prompt; `launchctl list \| grep claude-mesh` on the peer |
| airshell log: `unknown client rejected` | This Mac isn't in the peer's `authorized_peers` | [setup step 3](setup.md#3-authorize-the-macs-to-each-other) |
| airshell log: `peer certificate changed; refusing` | A Mac's airshell identity was recreated | Remove its line from `~/.config/airshell/known_peers` if expected |
| `status`: host isn't in the layout table | A new Mac or a different `hostname -s` | Add it to `hosts()` in `src/layout.rs` (or set `CLAUDE_MESH_HOST`) |
| A session appears twice in `claude --resume` | The same session diverged on two Macs (both kept, as branches) | Expected; resume either |
| `*.conflict-<host>.*` files appear | Both Macs edited the same non-log file between syncs | Pick the right one, delete the other on **every** Mac (deletions don't sync) |

## Known limits and risks

- **Deletions don't sync.** A file deleted on one Mac comes back from the others. To really delete, stop `claude-mesh` everywhere, delete on every Mac, start again.
- **Path rewriting is textual.** Inside transcripts, every occurrence of this Mac's source and config paths is rewritten, including paths a conversation merely *quotes* (e.g. a transcript on caladan that mentions `/Users/pplanel/src/pplanel/...` as text will read `/Users/pplanel/src/pplanel/pplanel/...` on arrakis). `claude-handoff` rewrites paths the same textual way.
- **Same session open on two Macs at once.** Both keep appending; the logs diverge and are merged as two branches. Claude resumes one branch; nothing is lost, but it's confusing. Use one Mac per session at a time, as with handoff.
- **Local access.** The server binds `127.0.0.1` without auth: any process on this Mac can read the synced files through it. They are your own files under your account, already readable by those processes.
- **Full rescans.** Each round lists the synced folders; unchanged files are hashed once and cached by (size, mtime).
## Removing it

On each Mac:

```sh
launchctl unload ~/Library/LaunchAgents/dev.pplanel.claude-mesh.plist
rm ~/Library/LaunchAgents/dev.pplanel.claude-mesh.plist
# delete the -claudesync [[service]] and [[forward]] entries from ~/.config/airshell/config.toml
rm -r ~/.local/state/claude-mesh
```

Synced files stay where they are; nothing in the Claude folders depends on `claude-mesh`.
