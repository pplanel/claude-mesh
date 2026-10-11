//! Pulling from a peer and merging into this Mac's Claude config.
//!
//! Sync is **pull-only**: each Mac fetches its peers' manifests and changed
//! files and merges them into its *own* folder. No Mac ever writes to
//! another, so a bug or a bad peer can only make this Mac fetch too much,
//! never corrupt someone else's files.
//!
//! For each file the peer has, the decision is:
//!
//! 1. Peer's hash unchanged since our last pull → nothing to do.
//! 2. Same hash as ours → just remember it.
//! 3. Our copy was written in the last [`ACTIVE_WINDOW_MS`] → skip this round:
//!    a live Claude session may be appending to it.
//! 4. Otherwise merge (see [`crate::merge`]) and write the result if it
//!    differs from ours. Appends are written as appends, so a session that
//!    starts writing again mid-sync never sees its file replaced.

use crate::canon::Canon;
use crate::merge::{Strategy, last_writer_wins, merge_jsonl, merge_line_union, strategy_for};
use crate::scan::{Manifest, Scanner, mtime_ms};
use crate::wire::Peer;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Files modified more recently than this are left alone until a later round.
/// `CLAUDE_MESH_ACTIVE_MS` overrides it (tests set it to 0).
pub const ACTIVE_WINDOW_MS: u64 = 60_000;

fn active_window_ms() -> u64 {
    std::env::var("CLAUDE_MESH_ACTIVE_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(ACTIVE_WINDOW_MS)
}

/// What this Mac remembers between rounds. Kept outside the Claude config so
/// it is never synced itself.
#[derive(Default, Serialize, Deserialize)]
pub struct State {
    /// Canonical path → hash of our copy right after we last synced it.
    /// If our copy no longer has this hash, *we* changed it since.
    pub local: BTreeMap<String, String>,
    /// Peer → canonical path → the peer's hash when we last pulled it.
    pub seen: BTreeMap<String, BTreeMap<String, String>>,
}

impl State {
    pub fn load(path: &Path) -> State {
        std::fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self).unwrap_or_default())?;
        std::fs::rename(tmp, path)
    }
}

#[derive(Default, Debug)]
pub struct Report {
    pub pulled: usize,
    pub merged: usize,
    pub conflicts: usize,
    pub skipped_active: usize,
}

/// One round against one peer, over an open session `conn`.
pub fn sync_peer(
    this_host: &str,
    peer: &str,
    conn: &mut Peer,
    scanner: &Scanner,
    state: &mut State,
) -> std::io::Result<Report> {
    let remote: Manifest = serde_json::from_slice(&conn.request("manifest")?)
        .map_err(|e| std::io::Error::other(format!("bad manifest from {peer}: {e}")))?;
    let local = scanner.manifest();
    let canon = scanner.canon();
    let now = now_ms();
    let mut report = Report::default();

    for (rel, theirs) in &remote {
        let seen = state.seen.entry(peer.to_string()).or_default();
        if seen.get(rel) == Some(&theirs.hash) {
            continue;
        }
        let ours = local.get(rel);
        if ours.is_some_and(|o| o.hash == theirs.hash) {
            seen.insert(rel.clone(), theirs.hash.clone());
            state.local.insert(rel.clone(), theirs.hash.clone());
            continue;
        }
        let local_rel = canon.rel_from_canonical(rel);
        let Some(path) = scanner.local_path(&local_rel) else {
            continue;
        };
        if is_active(&path, now) {
            report.skipped_active += 1;
            continue;
        }

        let remote_bytes = conn.file(rel)?;
        if blake3::hash(&remote_bytes).to_hex().as_str() != theirs.hash {
            // The peer's file changed between its manifest and our fetch;
            // the next round will see the new hash.
            continue;
        }
        let local_bytes = ours.and_then(|_| scanner.read_canonical(rel));

        let (result, conflict) = match &local_bytes {
            None => (remote_bytes.clone(), false),
            Some(mine) => {
                let unchanged_here = ours.is_some_and(|o| state.local.get(rel) == Some(&o.hash));
                resolve(
                    rel,
                    mine,
                    ours.map_or(0, |o| o.mtime),
                    &remote_bytes,
                    theirs.mtime,
                    unchanged_here,
                )
            }
        };

        if conflict && let Some(mine) = &local_bytes {
            let conflict_rel = conflict_name(&local_rel, this_host);
            if let Some(cpath) = scanner.local_path(&conflict_rel) {
                write_atomic(&cpath, &canon.content_from_canonical(&local_rel, mine))?;
                report.conflicts += 1;
                log!("conflict on {rel}: kept {peer}'s version, ours saved as {conflict_rel}");
            }
        }

        if local_bytes.as_deref() != Some(result.as_slice()) {
            if is_active(&path, now_ms()) {
                report.skipped_active += 1;
                continue;
            }
            write_local(canon, &local_rel, &path, local_bytes.as_deref(), &result)?;
            if local_bytes.is_none() || result == remote_bytes {
                report.pulled += 1;
            } else {
                report.merged += 1;
            }
        }
        let seen = state.seen.entry(peer.to_string()).or_default();
        seen.insert(rel.clone(), theirs.hash.clone());
        state
            .local
            .insert(rel.clone(), blake3::hash(&result).to_hex().to_string());
    }
    Ok(report)
}

/// Merge our canonical bytes with the peer's. Returns the result and whether
/// our version lost a conflict (and should be kept as a copy).
fn resolve(
    rel: &str,
    mine: &[u8],
    my_mtime: u64,
    theirs: &[u8],
    their_mtime: u64,
    unchanged_here: bool,
) -> (Vec<u8>, bool) {
    let as_text = |b: &[u8]| std::str::from_utf8(b).ok().map(str::to_string);
    match (strategy_for(rel), as_text(mine), as_text(theirs)) {
        (Strategy::Jsonl, Some(a), Some(b)) => (merge_jsonl(&a, &b).into_bytes(), false),
        (Strategy::LineUnion, Some(a), Some(b)) => (merge_line_union(&a, &b).into_bytes(), false),
        _ if unchanged_here => (theirs.to_vec(), false),
        _ => {
            if last_writer_wins(my_mtime, mine, their_mtime, theirs) {
                (mine.to_vec(), false)
            } else {
                (theirs.to_vec(), true)
            }
        }
    }
}

/// Write the merged canonical `result` to `path` in local form. When the
/// result only adds to what's there (a log that grew), append the tail
/// instead of replacing the file.
fn write_local(
    canon: &Canon,
    local_rel: &str,
    path: &Path,
    old_canonical: Option<&[u8]>,
    result: &[u8],
) -> std::io::Result<()> {
    let new_local = canon.content_from_canonical(local_rel, result);
    if let Some(old) = old_canonical {
        let old_local = canon.content_from_canonical(local_rel, old);
        if local_rel.ends_with(".jsonl") && new_local.starts_with(&old_local) {
            let current = std::fs::read(path)?;
            if current == old_local {
                let mut f = std::fs::OpenOptions::new().append(true).open(path)?;
                return f.write_all(&new_local[old_local.len()..]);
            }
        }
    }
    write_atomic(path, &new_local)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let tmp: PathBuf = dir.join(format!(".claude-mesh-{}-{name}", std::process::id()));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

fn is_active(path: &Path, now: u64) -> bool {
    std::fs::symlink_metadata(path)
        .map(|m| now.saturating_sub(mtime_ms(&m)) < active_window_ms())
        .unwrap_or(false)
}

/// `memory/note.md` → `memory/note.conflict-arrakis.md`.
pub fn conflict_name(rel: &str, host: &str) -> String {
    let (dir, file) = rel.rsplit_once('/').map_or(("", rel), |(d, f)| (d, f));
    let file = match file.rsplit_once('.') {
        Some((stem, ext)) => format!("{stem}.conflict-{host}.{ext}"),
        None => format!("{file}.conflict-{host}"),
    };
    if dir.is_empty() {
        file
    } else {
        format!("{dir}/{file}")
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conflict_names_keep_the_extension() {
        assert_eq!(
            conflict_name("projects/k/memory/a.md", "arrakis"),
            "projects/k/memory/a.conflict-arrakis.md"
        );
        assert_eq!(
            conflict_name("plans/x", "caladan"),
            "plans/x.conflict-caladan"
        );
    }

    #[test]
    fn unchanged_local_takes_theirs_without_conflict() {
        let (r, c) = resolve("plans/p.md", b"old", 5, b"new", 1, true);
        assert_eq!((r.as_slice(), c), (&b"new"[..], false));
    }

    #[test]
    fn both_changed_newer_wins_and_loser_is_kept() {
        let (r, c) = resolve("plans/p.md", b"mine", 1, b"theirs", 2, false);
        assert_eq!((r.as_slice(), c), (&b"theirs"[..], true));
        let (r, c) = resolve("plans/p.md", b"mine", 3, b"theirs", 2, false);
        assert_eq!((r.as_slice(), c), (&b"mine"[..], false));
    }
}
