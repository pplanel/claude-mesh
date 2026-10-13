//! What gets synced, and the manifest that describes it.
//!
//! A [`Manifest`] maps every synced file's **canonical** path to an [`Entry`]
//! (hash of its canonical contents, size, mtime). It is what a Mac sends a
//! peer first each round; the peer compares it with its own and fetches
//! only what differs. Only the top-level entries in [`INCLUDE`] are walked,
//! symlinks are never followed, and files over [`MAX_FILE_BYTES`] are left
//! out. Hashing is the expensive part, so [`Scanner`] remembers each file's
//! entry and re-reads a file only when its size or mtime changed.

use crate::canon::Canon;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::UNIX_EPOCH;

/// Top-level entries of the Claude config folder that are synced. Anything
/// not listed stays on its Mac: credentials, settings (nix-managed), plugin
/// and cache folders, per-process state (`sessions/`, `session-env/`,
/// `shell-snapshots/`, `daemon*`), the quota hook's marks, backups.
pub const INCLUDE: &[&str] = &[
    "projects",      // transcripts, subagent transcripts, tool results, memory
    "plans",         // plan-mode files
    "todos",         // per-session todo lists
    "tasks",         // task lists
    "file-history",  // file snapshots used by rewind
    "agents",        // user-defined subagents
    "commands",      // user-defined slash commands
    "history.jsonl", // prompt history (up-arrow)
];

/// Files above this size are skipped (logged once per scan).
pub const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;

/// One file in a manifest, described in canonical form.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Entry {
    /// BLAKE3 of the canonical content, hex.
    pub hash: String,
    /// Size of the file on disk (this Mac's form, not the canonical one).
    pub size: u64,
    /// Modification time, milliseconds since the Unix epoch.
    pub mtime: u64,
}

/// Canonical relative path → entry.
pub type Manifest = BTreeMap<String, Entry>;

/// Builds manifests, re-reading only files whose size or mtime changed.
pub struct Scanner {
    /// The Claude config folder.
    root: PathBuf,
    /// This Mac's translation to canonical form.
    canon: Canon,
    /// Absolute path → (size, mtime, entry) when it was last hashed. Shared
    /// by the server threads and the pull loop, hence the mutex.
    cache: Mutex<BTreeMap<PathBuf, (u64, u64, Entry)>>,
}

impl Scanner {
    /// A scanner of the config folder `root`, translating with `canon`.
    pub fn new(root: PathBuf, canon: Canon) -> Self {
        Scanner {
            root,
            canon,
            cache: Mutex::new(BTreeMap::new()),
        }
    }

    /// The Claude config folder this scanner reads.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// This Mac's translation to and from canonical form.
    pub fn canon(&self) -> &Canon {
        &self.canon
    }

    /// Walk the synced entries and describe every file in canonical form.
    /// Files unchanged since the last call (same size and mtime) aren't read.
    pub fn manifest(&self) -> Manifest {
        let mut files = Vec::new();
        for top in INCLUDE {
            collect(&self.root.join(top), &mut files);
        }
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        let mut out = Manifest::new();
        for path in files {
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            let size = meta.len();
            if size > MAX_FILE_BYTES {
                continue;
            }
            let mtime = mtime_ms(&meta);
            let Some(rel) = rel_str(&self.root, &path) else {
                continue;
            };
            let canon_rel = self.canon.rel_to_canonical(&rel);
            if let Some((s, m, e)) = cache.get(&path)
                && *s == size
                && *m == mtime
            {
                out.insert(canon_rel, e.clone());
                continue;
            }
            let Ok(file) = std::fs::File::open(&path) else {
                continue;
            };
            let Ok(hash) = self.canon.hash_canonical(&rel, file) else {
                continue;
            };
            let entry = Entry {
                hash: hash.to_hex().to_string(),
                size,
                mtime,
            };
            cache.insert(path, (size, mtime, entry.clone()));
            out.insert(canon_rel, entry);
        }
        out
    }

    /// The canonical content of a canonical path, if it is a synced file.
    pub fn read_canonical(&self, canon_rel: &str) -> Option<Vec<u8>> {
        let rel = self.canon.rel_from_canonical(canon_rel);
        let path = self.local_path(&rel)?;
        let bytes = std::fs::read(path).ok()?;
        Some(self.canon.content_to_canonical(&rel, &bytes))
    }

    /// A canonical path's file opened for reading, with its local relative
    /// path (what [`Canon::for_each_canonical`] needs) and its size now, if
    /// it is a synced file.
    pub fn open_canonical(&self, canon_rel: &str) -> Option<(std::fs::File, String, u64)> {
        let rel = self.canon.rel_from_canonical(canon_rel);
        let file = std::fs::File::open(self.local_path(&rel)?).ok()?;
        let size = file.metadata().ok()?.len();
        Some((file, rel, size))
    }

    /// The absolute path of a local relative path, refusing anything that
    /// escapes the config folder or isn't under an included entry.
    pub fn local_path(&self, rel: &str) -> Option<PathBuf> {
        if !is_safe_rel(rel) {
            return None;
        }
        let first = rel.split('/').next()?;
        INCLUDE.contains(&first).then(|| self.root.join(rel))
    }
}

/// Relative, no `..`, no empty or `.` components, no absolute paths.
pub fn is_safe_rel(rel: &str) -> bool {
    !rel.is_empty()
        && !rel.starts_with('/')
        && rel
            .split('/')
            .all(|c| !c.is_empty() && c != "." && c != "..")
}

/// A file's modification time in milliseconds since the Unix epoch (0 when
/// the system won't say).
pub fn mtime_ms(meta: &std::fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Regular files under `path` (or `path` itself), never following symlinks.
fn collect(path: &Path, out: &mut Vec<PathBuf>) {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return;
    };
    if meta.file_type().is_symlink() {
        return;
    }
    if meta.is_file() {
        out.push(path.to_path_buf());
    } else if meta.is_dir() {
        let Ok(rd) = std::fs::read_dir(path) else {
            return;
        };
        for entry in rd.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            // Our own temp files, and macOS litter.
            if name.starts_with(".claude-mesh-") || name == ".DS_Store" {
                continue;
            }
            collect(&entry.path(), out);
        }
    }
}

/// `path` relative to `root`, as UTF-8 (`None` for anything else).
fn rel_str(root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    let s = rel.to_str()?;
    Some(s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::find;

    #[test]
    fn unsafe_paths_are_refused() {
        for bad in [
            "",
            "/etc/passwd",
            "../x",
            "projects/../../x",
            "projects//x",
            "./projects",
        ] {
            assert!(!is_safe_rel(bad), "{bad}");
        }
        assert!(is_safe_rel("projects/k/a.jsonl"));
    }

    #[test]
    fn manifest_includes_only_synced_entries() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("projects/-Users-pplanel/memory")).unwrap();
        std::fs::write(
            root.join("projects/-Users-pplanel/memory/MEMORY.md"),
            "- x\n",
        )
        .unwrap();
        std::fs::write(root.join("history.jsonl"), "{}\n").unwrap();
        std::fs::write(root.join(".credentials.json"), "secret").unwrap();
        std::fs::create_dir_all(root.join("sessions")).unwrap();
        std::fs::write(root.join("sessions/1.json"), "{}").unwrap();

        let s = Scanner::new(root.to_path_buf(), Canon::new(&find("imperium").unwrap()));
        let m = s.manifest();
        let keys: Vec<&str> = m.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            ["history.jsonl", "projects/-Users-pplanel/memory/MEMORY.md"]
        );
        assert!(s.local_path(".credentials.json").is_none());
        assert!(s.local_path("sessions/1.json").is_none());
    }
}
