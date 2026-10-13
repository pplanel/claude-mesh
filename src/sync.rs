//! Pulling from a peer and merging into this Mac's Claude config.
//!
//! Sync is **pull-only**: each Mac fetches its peers' manifests and changed
//! files and merges them into its *own* folder. No Mac ever writes to
//! another, so a bug or a bad peer can only make this Mac fetch too much,
//! never corrupt someone else's files.
//!
//! For each file in the peer's manifest, [`sync_peer`] decides:
//!
//! 1. **Seen**: the peer's hash is the one we pulled last time → nothing.
//! 2. **Same**: the peer's hash is our copy's → remember it, nothing to write.
//! 3. **Busy**: our copy was written in the last [`ACTIVE_WINDOW_MS`], or it
//!    belongs to a session open on this Mac ([`live_sessions`]) → leave it
//!    for a later round.
//! 4. **Fetch**: a log just its new bytes when possible ([`fetch_tail`]),
//!    else the whole file; checked against the peer's hash.
//! 5. **Same on disk**: the peer's version, written in our layout, is byte
//!    for byte our file → the same file, whatever the canonical forms say
//!    (see "When it isn't reversible" in [`crate::canon`]); remember it.
//! 6. **Merge** ([`resolve`], with [`crate::merge`]) and write the result if
//!    it differs ([`write_local`]): appends as appends, anything else
//!    atomically; a lost conflict also writes our version as a copy.
//! 7. **Remember** the peer's hash, and our copy's hash *as read back*; keep
//!    the peer's copy when ours reads back differently, as the base for its
//!    next tail.
//!
//! [`State`] is what carries over between rounds; a file that can't be
//! written here (`failed`) is logged and skipped, never the whole round.

use crate::canon::Canon;
use crate::merge::{Strategy, last_writer_wins, merge_jsonl, merge_line_union, merge_lines_3way, strategy_for};
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

/// [`ACTIVE_WINDOW_MS`], or `CLAUDE_MESH_ACTIVE_MS` when set.
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
    /// The state saved at `path`, or an empty one (first run, or unreadable:
    /// the worst case is a round that re-checks everything).
    pub fn load(path: &Path) -> State {
        std::fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    /// Write the state to `path` atomically (a temp file, then a rename).
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self).unwrap_or_default())?;
        std::fs::rename(tmp, path)
    }
}

/// What one round did to one file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    /// We didn't have it, or took the peer's version as is.
    Pulled,
    /// The result combines both sides (a log with two branches, the memory
    /// index).
    Merged,
    /// Both sides changed it; the peer's version won and ours was saved as a
    /// conflict copy (`note` names it).
    Conflict,
    /// Left for a later round (`note`: `recent` or `open session`).
    Busy,
    /// Couldn't be written here (`note` says why).
    Failed,
}

/// One file's part in a round, as the journal records it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEvent {
    /// The canonical path.
    pub path: String,
    /// What happened to it.
    pub action: Action,
    /// Bytes fetched for it (0 when nothing was fetched).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub bytes: u64,
    /// Whether it came as a tail (just a log's new bytes).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub tail: bool,
    /// A conflict copy's name, why it was busy, or the write error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// For `skip_serializing_if`.
fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// What one round against one peer did. Filled in as the round goes, so a
/// round that breaks off still reports what it got done.
#[derive(Default, Debug)]
pub struct Report {
    /// Files we didn't have, or took as the peer's version.
    pub pulled: usize,
    /// Files whose result combines both sides.
    pub merged: usize,
    /// Files where our version lost and was saved as a conflict copy.
    pub conflicts: usize,
    /// Files left alone this round (recently written, or an open session's).
    pub skipped_active: usize,
    /// Files we couldn't write here (logged); retried when the peer's copy changes.
    pub failed: usize,
    /// Bytes fetched from the peer (file bodies, full or tails).
    pub fetched: u64,
    /// Logs fetched as just their new lines.
    pub tails: usize,
    /// Every file the round did something with, or left busy.
    pub files: Vec<FileEvent>,
}

impl Report {
    /// Count `event` and keep it.
    fn record(&mut self, event: FileEvent) {
        match event.action {
            Action::Pulled => self.pulled += 1,
            Action::Merged => self.merged += 1,
            Action::Conflict => self.conflicts += 1,
            Action::Busy => self.skipped_active += 1,
            Action::Failed => self.failed += 1,
        }
        self.files.push(event);
    }

    /// Whether the round wrote anything or failed to (what's worth a log line).
    pub fn wrote_anything(&self) -> bool {
        self.pulled + self.merged + self.conflicts + self.failed > 0
    }
}

/// An event for `path` with no bytes, tail or note.
fn event(path: &str, action: Action) -> FileEvent {
    FileEvent {
        path: path.to_string(),
        action,
        bytes: 0,
        tail: false,
        note: None,
    }
}

/// One round against one peer, over an open session `conn`, recording into
/// `report` as it goes. An error ends the round (the stream is broken), but
/// what was done until then stays in `report` and in `state`.
pub fn sync_peer(
    this_host: &str,
    peer: &str,
    conn: &mut Peer,
    scanner: &Scanner,
    state: &mut State,
    peer_copies: &Path,
    report: &mut Report,
) -> std::io::Result<()> {
    let remote: Manifest = serde_json::from_slice(&conn.request("manifest")?)
        .map_err(|e| std::io::Error::other(format!("bad manifest from {peer}: {e}")))?;
    let local = scanner.manifest();
    let canon = scanner.canon();
    let now = now_ms();
    let live = live_sessions(scanner.root());

    for (rel, theirs) in &remote {
        let seen = state.seen.entry(peer.to_string()).or_default();
        if seen.get(rel) == Some(&theirs.hash) {
            continue;
        }
        let ours = local.get(rel);
        if ours.is_some_and(|o| o.hash == theirs.hash) {
            settle_peer_copy(peer_copies, rel, scanner.read_canonical(rel).as_deref());
            seen.insert(rel.clone(), theirs.hash.clone());
            state.local.insert(rel.clone(), theirs.hash.clone());
            continue;
        }
        let local_rel = canon.rel_from_canonical(rel);
        let Some(path) = scanner.local_path(&local_rel) else {
            continue;
        };
        if belongs_to(&local_rel, &live) || is_active(&path, now) {
            let why = if belongs_to(&local_rel, &live) {
                "open session"
            } else {
                "recent"
            };
            report.record(FileEvent {
                note: Some(why.into()),
                ..event(rel, Action::Busy)
            });
            continue;
        }

        let (remote_bytes, fetched, tail) =
            match fetch_tail(conn, rel, ours.is_some(), scanner, theirs, peer_copies)? {
                Some((bytes, tail_len)) => (bytes, tail_len as u64, true),
                None => {
                    let bytes = conn.file(rel)?;
                    let n = bytes.len() as u64;
                    (bytes, n, false)
                }
            };
        report.fetched += fetched;
        report.tails += usize::from(tail);
        let fetched_event = |action| FileEvent {
            bytes: fetched,
            tail,
            ..event(rel, action)
        };
        if blake3::hash(&remote_bytes).to_hex().as_str() != theirs.hash {
            // The peer's file changed between its manifest and our fetch;
            // the next round will see the new hash.
            continue;
        }
        // The same bytes on disk once written in our layout: the same file,
        // whatever the canonical forms say (rewriting paths isn't always
        // reversible). Record it; nothing to merge, nothing in conflict.
        if ours.is_some()
            && std::fs::read(&path).ok()
                == Some(canon.content_from_canonical(&local_rel, &remote_bytes))
        {
            state
                .seen
                .entry(peer.to_string())
                .or_default()
                .insert(rel.clone(), theirs.hash.clone());
            if let Some(o) = ours {
                state.local.insert(rel.clone(), o.hash.clone());
            }
            continue;
        }
        let local_bytes = ours.and_then(|_| scanner.read_canonical(rel));

        let (result, conflict) = match &local_bytes {
            None => (remote_bytes.clone(), false),
            Some(mine) => {
                let unchanged_here = ours.is_some_and(|o| state.local.get(rel) == Some(&o.hash));
                let base = (strategy_for(rel) == Strategy::LineUnion)
                    .then(|| std::fs::read(peer_copy_path(peer_copies, rel)).ok())
                    .flatten();
                resolve(
                    rel,
                    base.as_deref(),
                    mine,
                    ours.map_or(0, |o| o.mtime),
                    &remote_bytes,
                    theirs.mtime,
                    unchanged_here,
                )
            }
        };

        // A file that can't be written here (a name too long for this
        // layout, a permission) is logged and skipped, not the whole round.
        // It counts as seen, so it's retried when the peer's copy changes.
        let mut conflict_copy = None;
        let written = (|| -> std::io::Result<Option<bool>> {
            // A conflict copy that itself conflicts isn't copied again: that
            // would nest (`x.conflict-a.conflict-a…`) a little more each round.
            if conflict
                && !rel.contains(".conflict-")
                && let Some(mine) = &local_bytes
            {
                let conflict_rel = conflict_name(&local_rel, this_host);
                if let Some(cpath) = scanner.local_path(&conflict_rel) {
                    write_atomic(&cpath, &canon.content_from_canonical(&local_rel, mine))?;
                    log!("conflict on {rel}: kept {peer}'s version, ours saved as {conflict_rel}");
                    conflict_copy = Some(conflict_rel);
                }
            }
            if local_bytes.as_deref() == Some(result.as_slice()) {
                return Ok(Some(false));
            }
            if is_active(&path, now_ms()) {
                return Ok(None);
            }
            write_local(canon, &local_rel, &path, local_bytes.as_deref(), &result)?;
            // Their content keeps their time. Written with ours, a copy
            // relayed through a third Mac would look newer than an edit
            // made after it, and win the next last-writer-wins.
            if result == remote_bytes {
                set_mtime_ms(&path, theirs.mtime)?;
            }
            Ok(Some(true))
        })();
        match written {
            Ok(None) => {
                report.record(FileEvent {
                    note: Some("recent".into()),
                    ..fetched_event(Action::Busy)
                });
                continue;
            }
            Ok(_) if conflict_copy.is_some() => report.record(FileEvent {
                note: conflict_copy,
                ..fetched_event(Action::Conflict)
            }),
            Ok(Some(true)) if local_bytes.is_none() || result == remote_bytes => {
                report.record(fetched_event(Action::Pulled));
            }
            Ok(Some(true)) => report.record(fetched_event(Action::Merged)),
            Ok(Some(false)) => {}
            Err(e) => {
                report.record(FileEvent {
                    note: Some(e.to_string()),
                    ..fetched_event(Action::Failed)
                });
                log!("{peer}: can't write {local_rel}: {e}");
                state
                    .seen
                    .entry(peer.to_string())
                    .or_default()
                    .insert(rel.clone(), theirs.hash.clone());
                continue;
            }
        }
        let seen = state.seen.entry(peer.to_string()).or_default();
        seen.insert(rel.clone(), theirs.hash.clone());
        // Remember our copy's hash as the scanner will see it: rewriting paths
        // isn't always reversible (text that names another Mac's config dir
        // becomes a token there), so re-reading what we wrote can give other
        // canonical bytes than `result`. Recording `result`'s hash would make
        // the next round think we changed the file, and conflict forever.
        let ours_now = scanner.read_canonical(rel).unwrap_or(result);
        // When our copy, read back, isn't the peer's (two branches of a
        // session merged, or paths that read back differently in our layout),
        // ours can't be the base for the peer's next tail. Keep the peer's
        // copy for that; drop it once they're the same.
        if strategy_for(rel) == Strategy::LineUnion {
            keep_peer_copy(peer_copies, rel, &remote_bytes);
        } else if rel.ends_with(".jsonl") {
            if ours_now == remote_bytes {
                forget_peer_copy(peer_copies, rel);
            } else {
                keep_peer_copy(peer_copies, rel, &remote_bytes);
            }
        }
        state
            .local
            .insert(rel.clone(), blake3::hash(&ours_now).to_hex().to_string());
    }
    Ok(())
}

/// What a round against a peer would still have to do, from its manifest,
/// without fetching anything: see [`pending`].
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Pending {
    /// Files the next round would fetch.
    pub files: usize,
    /// Their size on the peer (its layout; a tail fetches less).
    pub bytes: u64,
    /// Files the next round would leave alone (recent or an open session's).
    pub busy: usize,
    /// Files we have that the peer doesn't, or has differently: roughly what
    /// the peer will pull from us.
    pub theirs_to_pull: usize,
}

/// How far this Mac is from `peer`, given the peer's manifest `remote`: the
/// first three steps of [`sync_peer`] (seen, same, busy), nothing fetched.
pub fn pending(peer: &str, remote: &Manifest, scanner: &Scanner, state: &State) -> Pending {
    let local = scanner.manifest();
    let canon = scanner.canon();
    let live = live_sessions(scanner.root());
    let now = now_ms();
    let seen = state.seen.get(peer);
    let mut p = Pending::default();
    for (rel, theirs) in remote {
        if seen.and_then(|s| s.get(rel)) == Some(&theirs.hash)
            || local.get(rel).is_some_and(|o| o.hash == theirs.hash)
        {
            continue;
        }
        let local_rel = canon.rel_from_canonical(rel);
        let Some(path) = scanner.local_path(&local_rel) else {
            continue;
        };
        if belongs_to(&local_rel, &live) || is_active(&path, now) {
            p.busy += 1;
        } else {
            p.files += 1;
            p.bytes += theirs.size;
        }
    }
    p.theirs_to_pull = local
        .iter()
        .filter(|(rel, ours)| remote.get(*rel).is_none_or(|t| t.hash != ours.hash))
        .count();
    p
}

/// A log that only grew on the peer: fetch just the new bytes. The base is
/// the peer's copy we kept when we diverged from it, else our own copy.
/// Returns the peer's whole file (base + tail) and the tail's length, or
/// `None` when no base plus its tail gives the peer's hash (it diverged or
/// shrank, or the peer is too old for `tail`), so the caller fetches it whole.
fn fetch_tail(
    conn: &mut Peer,
    rel: &str,
    have_ours: bool,
    scanner: &Scanner,
    theirs: &crate::scan::Entry,
    peer_copies: &Path,
) -> std::io::Result<Option<(Vec<u8>, usize)>> {
    if !have_ours || !rel.ends_with(".jsonl") {
        return Ok(None);
    }
    let bases = [
        std::fs::read(peer_copy_path(peer_copies, rel)).ok(),
        scanner.read_canonical(rel),
    ];
    for mut base in bases.into_iter().flatten() {
        let Ok(tail) = conn.tail(rel, base.len()) else {
            // `err`: an old peer, or the file shrank there. The stream is
            // still in step (a whole response was read), so carry on.
            continue;
        };
        let tail_len = tail.len();
        base.extend_from_slice(&tail);
        if blake3::hash(&base).to_hex().as_str() == theirs.hash {
            return Ok(Some((base, tail_len)));
        }
    }
    Ok(None)
}

/// Where the peer's copy of `rel` is kept: `dir/<blake3(rel), 32 hex>.jsonl`.
fn peer_copy_path(dir: &Path, rel: &str) -> PathBuf {
    dir.join(format!(
        "{}.jsonl",
        &blake3::hash(rel.as_bytes()).to_hex()[..32]
    ))
}

/// Keep `bytes` as the peer's copy of `rel`; a failure only costs a whole
/// fetch next time, so it's logged, not returned.
fn keep_peer_copy(dir: &Path, rel: &str, bytes: &[u8]) {
    if let Err(e) =
        std::fs::create_dir_all(dir).and_then(|()| write_atomic(&peer_copy_path(dir, rel), bytes))
    {
        log!("can't keep the peer's copy of {rel}: {e}");
    }
}

/// When our copy and the peer's are the same: a jsonl needs no peer copy
/// any more, a line-union file keeps it as the base of its next merge.
fn settle_peer_copy(dir: &Path, rel: &str, same: Option<&[u8]>) {
    match same {
        Some(bytes) if strategy_for(rel) == Strategy::LineUnion => keep_peer_copy(dir, rel, bytes),
        _ => forget_peer_copy(dir, rel),
    }
}

/// Drop the peer's copy of `rel`, if any.
fn forget_peer_copy(dir: &Path, rel: &str) {
    let _ = std::fs::remove_file(peer_copy_path(dir, rel));
}

/// Merge our canonical bytes with the peer's. Returns the result and whether
/// our version lost a conflict (and should be kept as a copy).
fn resolve(
    rel: &str,
    base: Option<&[u8]>,
    mine: &[u8],
    my_mtime: u64,
    theirs: &[u8],
    their_mtime: u64,
    unchanged_here: bool,
) -> (Vec<u8>, bool) {
    let as_text = |b: &[u8]| std::str::from_utf8(b).ok().map(str::to_string);
    match (strategy_for(rel), as_text(mine), as_text(theirs)) {
        (Strategy::Jsonl, Some(a), Some(b)) => (merge_jsonl(&a, &b).into_bytes(), false),
        (Strategy::LineUnion, Some(a), Some(b)) => {
            let merged = match base.and_then(|o| std::str::from_utf8(o).ok()) {
                Some(o) => merge_lines_3way(o, &a, &b),
                None => merge_line_union(&a, &b),
            };
            (merged.into_bytes(), false)
        }
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

/// Replace `path` with `bytes` in one step: write a temp file next to it,
/// then rename it over. Readers see the old file or the new one, never half.
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    // Short whatever the target's name: a long name plus a prefix could pass
    // the 255-byte limit. The scanner skips `.claude-mesh-*`.
    let name = path
        .file_name()
        .map(|n| n.as_encoded_bytes())
        .unwrap_or_default();
    let tag = &blake3::hash(name).to_hex()[..16];
    let tmp: PathBuf = dir.join(format!(".claude-mesh-{}-{tag}.tmp", std::process::id()));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

/// Set `path`'s modification time to `ms` milliseconds since the Unix epoch.
fn set_mtime_ms(path: &Path, ms: u64) -> std::io::Result<()> {
    let t = std::time::UNIX_EPOCH + std::time::Duration::from_millis(ms);
    std::fs::File::options().write(true).open(path)?.set_modified(t)
}

/// Whether `path` was modified less than the active window before `now`.
fn is_active(path: &Path, now: u64) -> bool {
    std::fs::symlink_metadata(path)
        .map(|m| now.saturating_sub(mtime_ms(&m)) < active_window_ms())
        .unwrap_or(false)
}

/// Sessions open on this Mac right now: Claude Code keeps
/// `sessions/<pid>.json` with the `sessionId` of each running process.
/// Their files are left alone however long they've been quiet (a long tool
/// call writes nothing for minutes); they merge once the session ends.
pub fn live_sessions(config: &Path) -> Vec<String> {
    let Ok(dir) = std::fs::read_dir(config.join("sessions")) else {
        return vec![];
    };
    dir.flatten()
        .filter_map(|e| {
            let v: serde_json::Value =
                serde_json::from_slice(&std::fs::read(e.path()).ok()?).ok()?;
            let pid = v["pid"].as_u64()?;
            let alive = std::process::Command::new("/bin/kill")
                .args(["-0", &pid.to_string()])
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|s| s.success());
            alive.then(|| v["sessionId"].as_str().map(str::to_string))?
        })
        .collect()
}

/// Whether `rel` (`projects/<key>/<session>.jsonl` or anything under
/// `projects/<key>/<session>/`) is one of `sessions`' files.
fn belongs_to(rel: &str, sessions: &[String]) -> bool {
    let mut parts = rel.split('/');
    if parts.next() != Some("projects") || parts.next().is_none() {
        return false;
    }
    let Some(name) = parts.next() else {
        return false;
    };
    let id = name.strip_suffix(".jsonl").unwrap_or(name);
    sessions.iter().any(|s| s == id)
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

/// Milliseconds since the Unix epoch.
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
    fn a_live_sessions_files_are_its_transcript_and_folder() {
        let live = vec!["8de4".to_string()];
        assert!(belongs_to("projects/k/8de4.jsonl", &live));
        assert!(belongs_to("projects/k/8de4/subagents/a.jsonl", &live));
        assert!(!belongs_to("projects/k/other.jsonl", &live));
        assert!(!belongs_to("projects/k/memory/MEMORY.md", &live));
        assert!(!belongs_to("plans/8de4.jsonl", &live));
    }

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
        let (r, c) = resolve("plans/p.md", None, b"old", 5, b"new", 1, true);
        assert_eq!((r.as_slice(), c), (&b"new"[..], false));
    }

    #[test]
    fn both_changed_newer_wins_and_loser_is_kept() {
        let (r, c) = resolve("plans/p.md", None, b"mine", 1, b"theirs", 2, false);
        assert_eq!((r.as_slice(), c), (&b"theirs"[..], true));
        let (r, c) = resolve("plans/p.md", None, b"mine", 3, b"theirs", 2, false);
        assert_eq!((r.as_slice(), c), (&b"mine"[..], false));
    }

    #[test]
    fn an_index_unchanged_here_takes_their_edit() {
        let stale = b"- [A](a.md) \xe2\x80\x94 one\n";
        let edited = b"- [A](a.md) \xe2\x80\x94 one, edited\n";
        let (r, c) = resolve("projects/k/memory/MEMORY.md", Some(stale), stale, 1, edited, 2, true);
        assert_eq!((r.as_slice(), c), (&edited[..], false));
    }
}
