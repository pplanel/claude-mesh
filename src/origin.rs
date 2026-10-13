//! Which Mac each transcript line was written on.
//!
//! A transcript, once synced, is the same file everywhere: nothing in it
//! says on which Mac a turn happened. Each Mac keeps a registry of the lines
//! it wrote, `mesh/origins/<session>.origin` in the Claude config folder,
//! one `<uuid> <host>` per line. The registries sync like everything else
//! (merged as a set of lines, never shrinking), so every Mac ends up knowing
//! every line's origin. It lives outside `projects/` so Claude Code never
//! mistakes it for a session.
//!
//! How a Mac knows a line is its own:
//!
//! * At the start of a round ([`claim`]), new lines in this Mac's
//!   transcripts whose uuid it hasn't seen are its own: only Claude Code,
//!   running here, adds lines between rounds.
//! * Lines claude-mesh itself writes from a peer are marked seen right after
//!   ([`mark_seen`]), so they're never claimed. That's safe because
//!   claude-mesh never writes a file Claude Code is writing: open sessions
//!   and recently written files are left alone.
//! * The first time, everything already there is marked seen without an
//!   owner; otherwise the first Mac to run would claim all history.
//!
//! "Seen" lives next to the state file (`state.origins/`), never synced: it
//! is what this Mac has seen, not a fact about the line. The functions take
//! that folder as `work`.

use std::collections::{BTreeMap, HashSet};
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// Where the registries live, relative to the Claude config folder.
pub const DIR: &str = "mesh/origins";

/// The registry of `session`'s line origins.
pub fn registry_path(config: &Path, session: &str) -> PathBuf {
    config.join(DIR).join(format!("{session}.origin"))
}

/// Each line's origin in `session`, from the registry: uuid → host.
pub fn read_registry(config: &Path, session: &str) -> BTreeMap<String, String> {
    std::fs::read_to_string(registry_path(config, session))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split_once(' '))
        .map(|(u, h)| (u.to_string(), h.to_string()))
        .collect()
}

/// A transcript line's `"uuid"`, read without building the rest of the line
/// (a tool result can be megabytes).
pub fn line_uuid(line: &str) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Line {
        uuid: Option<String>,
    }
    serde_json::from_str::<Line>(line).ok()?.uuid
}

/// Session transcripts under `config`: `projects/<key>/<session>.jsonl`, as
/// (session id, path). Subagent transcripts live deeper and aren't included.
fn transcripts(config: &Path) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    for project in std::fs::read_dir(config.join("projects")).into_iter().flatten().flatten() {
        for f in std::fs::read_dir(project.path()).into_iter().flatten().flatten() {
            let path = f.path();
            if path.extension().is_some_and(|e| e == "jsonl")
                && let Some(id) = path.file_stem().map(|s| s.to_string_lossy().into_owned())
            {
                out.push((id, path));
            }
        }
    }
    out
}

/// Per transcript, how far it has been read: its size and mtime then, and
/// the byte offset read up to (transcripts only grow, so the next read can
/// start there).
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct Progress {
    /// Transcript path → (size, mtime ms, offset read up to).
    files: BTreeMap<String, (u64, u64, u64)>,
    /// Whether the first pass (everything seen, nothing claimed) has run.
    bootstrapped: bool,
}

/// This Mac's bookkeeping folder (`state.origins/`, given by the caller).
fn work_dir(work: &Path) -> PathBuf {
    work.to_path_buf()
}

/// The progress saved in `work`, or a fresh one.
fn load_progress(state_dir: &Path) -> Progress {
    std::fs::read(work_dir(state_dir).join("progress.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// Save `p` in `work`, atomically.
fn save_progress(state_dir: &Path, p: &Progress) -> std::io::Result<()> {
    std::fs::create_dir_all(work_dir(state_dir))?;
    crate::sync::write_atomic(
        &work_dir(state_dir).join("progress.json"),
        &serde_json::to_vec(p).unwrap_or_default(),
    )
}

/// The uuids this Mac has seen in `session`.
fn load_seen(state_dir: &Path, session: &str) -> HashSet<String> {
    std::fs::read_to_string(work_dir(state_dir).join(format!("{session}.seen")))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

/// Append `uuids` to a file, one per line (`suffix` after each).
fn append_lines(path: &Path, uuids: &[String], suffix: &str) -> std::io::Result<()> {
    if uuids.is_empty() {
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut f = std::io::BufWriter::new(std::fs::OpenOptions::new().create(true).append(true).open(path)?);
    for u in uuids {
        writeln!(f, "{u}{suffix}")?;
    }
    f.flush()
}

/// Read `path` from `offset`, return the uuids of its complete lines not in
/// `seen` (adding them) and the offset of the end of the last complete line.
fn new_uuids(path: &Path, offset: u64, seen: &mut HashSet<String>) -> std::io::Result<(Vec<String>, u64)> {
    let mut file = std::fs::File::open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut lines = BufReader::new(file);
    let (mut fresh, mut end, mut line) = (Vec::new(), offset, Vec::new());
    loop {
        line.clear();
        let n = lines.read_until(b'\n', &mut line)?;
        // A line still being written (no newline yet) is read next time.
        if n == 0 || !line.ends_with(b"\n") {
            return Ok((fresh, end));
        }
        end += n as u64;
        if let Some(u) = std::str::from_utf8(&line).ok().and_then(line_uuid)
            && seen.insert(u.clone())
        {
            fresh.push(u);
        }
    }
}

/// Start of a round: claim the lines Claude Code wrote here since the last
/// round. Returns how many lines were claimed.
pub fn claim(config: &Path, state_dir: &Path, host: &str) -> std::io::Result<usize> {
    let mut progress = load_progress(state_dir);
    let mut claimed = 0;
    for (session, path) in transcripts(config) {
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        let (size, mtime) = (meta.len(), crate::scan::mtime_ms(&meta));
        let key = path.to_string_lossy().into_owned();
        let prev = progress.files.get(&key).copied();
        if prev.is_some_and(|(s, m, _)| s == size && m == mtime) {
            continue;
        }
        // Grown: read the new part. Shrunk or rewritten: read it all again
        // (seen uuids are skipped, so nothing is claimed twice).
        let offset = prev.map_or(0, |(_, _, o)| o).min(size);
        let mut seen = load_seen(state_dir, &session);
        let (fresh, end) = new_uuids(&path, offset, &mut seen)?;
        append_lines(&work_dir(state_dir).join(format!("{session}.seen")), &fresh, "")?;
        if progress.bootstrapped && !fresh.is_empty() {
            append_lines(&registry_path(config, &session), &fresh, &format!(" {host}"))?;
            claimed += fresh.len();
        }
        progress.files.insert(key, (size, mtime, end));
    }
    progress.bootstrapped = true;
    save_progress(state_dir, &progress)?;
    Ok(claimed)
}

/// After claude-mesh wrote `paths` (transcripts taken from a peer): their
/// lines aren't this Mac's, so mark them seen and the files read.
pub fn mark_seen(state_dir: &Path, paths: &[PathBuf]) -> std::io::Result<()> {
    let mut progress = load_progress(state_dir);
    for path in paths {
        let Some(session) = path.file_stem().map(|s| s.to_string_lossy().into_owned()) else {
            continue;
        };
        let mut seen = load_seen(state_dir, &session);
        let (fresh, end) = new_uuids(path, 0, &mut seen)?;
        append_lines(&work_dir(state_dir).join(format!("{session}.seen")), &fresh, "")?;
        if let Ok(meta) = std::fs::metadata(path) {
            progress.files.insert(
                path.to_string_lossy().into_owned(),
                (meta.len(), crate::scan::mtime_ms(&meta), end),
            );
        }
    }
    save_progress(state_dir, &progress)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(uuid: &str) -> String {
        format!(r#"{{"uuid":"{uuid}","timestamp":"2026-10-13T00:00:00.000Z","message":"x"}}"#)
    }

    #[test]
    fn claims_only_lines_written_here_after_the_first_pass() {
        let config = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let dir = config.path().join("projects/-Users-pplanel-src-x");
        std::fs::create_dir_all(&dir).unwrap();
        let t = dir.join("s1.jsonl");
        let append = |l: &str| {
            let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&t).unwrap();
            writeln!(f, "{l}").unwrap();
        };

        // History before claude-mesh knew about origins: seen, nobody's.
        append(&line("old"));
        assert_eq!(claim(config.path(), state.path(), "caladan").unwrap(), 0);

        // Claude Code writes here: ours.
        append(&line("mine"));
        assert_eq!(claim(config.path(), state.path(), "caladan").unwrap(), 1);

        // claude-mesh writes a peer's line: marked seen, never claimed.
        append(&line("theirs"));
        mark_seen(state.path(), std::slice::from_ref(&t)).unwrap();
        assert_eq!(claim(config.path(), state.path(), "caladan").unwrap(), 0);

        // A line still being written is left for the next round.
        let mut f = std::fs::OpenOptions::new().append(true).open(&t).unwrap();
        write!(f, "{}", line("half")).unwrap();
        assert_eq!(claim(config.path(), state.path(), "caladan").unwrap(), 0);
        writeln!(f).unwrap();
        assert_eq!(claim(config.path(), state.path(), "caladan").unwrap(), 1);

        let reg = read_registry(config.path(), "s1");
        assert_eq!(reg.get("mine").map(String::as_str), Some("caladan"));
        assert_eq!(reg.get("half").map(String::as_str), Some("caladan"));
        assert!(!reg.contains_key("old") && !reg.contains_key("theirs"));
    }
}
