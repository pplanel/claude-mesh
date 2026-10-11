//! The round journal, and the report made from it.
//!
//! Every round against every peer, the empty ones and the failed ones
//! included, appends one JSON line to `state.rounds.jsonl` next to the state
//! file: when it started, how long it took, what it fetched, any error, and
//! each file it did something with ([`FileEvent`]). At [`MAX_BYTES`] the
//! journal is moved to `state.rounds.1.jsonl` (replacing the previous one)
//! and a new one starts, so at most two files' worth is kept.
//!
//! ```text
//! {"at":1791752619000,"peer":"imperium","ms":412,"pulled":1,"fetched":82944,"tails":1,
//!  "files":[{"path":"projects/@SRCKEY@-x/s.jsonl","action":"pulled","bytes":82944,"tail":true}]}
//! ```
//!
//! [`render`] turns it into the `claude-mesh report` text: totals per peer,
//! traffic per hour, the files that moved the most, and the latest merges,
//! conflicts and failures.

use crate::sync::{Action, FileEvent, Report};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

/// The journal is rotated when it grows past this (8 MiB).
pub const MAX_BYTES: u64 = 8 * 1024 * 1024;

/// One round against one peer.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Round {
    /// When the round started, milliseconds since the Unix epoch.
    pub at: u64,
    /// The peer pulled from.
    pub peer: String,
    /// How long the round took, connecting included, in milliseconds.
    pub ms: u64,
    /// Why the round failed or broke off (unreachable, connection lost).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Files pulled (see [`Report::pulled`]).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub pulled: usize,
    /// Files merged.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub merged: usize,
    /// Files whose version here lost a conflict.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub conflicts: usize,
    /// Files left for a later round.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub busy: usize,
    /// Files that couldn't be written here.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub failed: usize,
    /// Bytes fetched.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub fetched: u64,
    /// Logs fetched as tails.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub tails: usize,
    /// What happened to each file.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<FileEvent>,
}

/// For `skip_serializing_if`.
fn is_zero(n: &usize) -> bool {
    *n == 0
}

/// For `skip_serializing_if`.
fn is_zero_u64(n: &u64) -> bool {
    *n == 0
}

impl Round {
    /// The journal entry for a round against `peer` that started at `at`,
    /// took `ms`, did `report` and ended with `error`.
    pub fn new(at: u64, peer: &str, ms: u64, report: Report, error: Option<String>) -> Round {
        Round {
            at,
            peer: peer.to_string(),
            ms,
            error,
            pulled: report.pulled,
            merged: report.merged,
            conflicts: report.conflicts,
            busy: report.skipped_active,
            failed: report.failed,
            fetched: report.fetched,
            tails: report.tails,
            files: report.files,
        }
    }
}

/// The journal kept next to the state file `state` (`state.rounds.jsonl`).
pub fn path_for(state: &Path) -> PathBuf {
    state.with_extension("rounds.jsonl")
}

/// The rotated journal (`state.rounds.1.jsonl`).
fn rotated(journal: &Path) -> PathBuf {
    journal.with_extension("1.jsonl")
}

/// Append `round` to `journal`, rotating it first when it's past
/// [`MAX_BYTES`].
pub fn append(journal: &Path, round: &Round) -> std::io::Result<()> {
    if let Some(dir) = journal.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if std::fs::metadata(journal).is_ok_and(|m| m.len() > MAX_BYTES) {
        std::fs::rename(journal, rotated(journal))?;
    }
    let mut line = serde_json::to_vec(round).map_err(std::io::Error::other)?;
    line.push(b'\n');
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(journal)?
        .write_all(&line)
}

/// Every round in `journal` and its rotated predecessor at or after `since`
/// (ms since the epoch), oldest first. Lines that don't parse are skipped.
pub fn read(journal: &Path, since: u64) -> Vec<Round> {
    let mut rounds = Vec::new();
    for path in [rotated(journal), journal.to_path_buf()] {
        let Ok(f) = std::fs::File::open(&path) else {
            continue;
        };
        for line in std::io::BufReader::new(f).lines().map_while(Result::ok) {
            if let Ok(r) = serde_json::from_str::<Round>(&line)
                && r.at >= since
            {
                rounds.push(r);
            }
        }
    }
    rounds.sort_by_key(|r| r.at);
    rounds
}

/// What `claude-mesh report` shows.
pub struct Options {
    /// Only rounds at or after this (ms since the epoch).
    pub since: u64,
    /// Only this peer.
    pub peer: Option<String>,
    /// List every file event, not just the summary.
    pub files: bool,
    /// The host the report is about (shown in the title).
    pub host: String,
}

/// The report for `rounds` (already filtered by time).
pub fn render(rounds: &[Round], opts: &Options) -> String {
    let rounds: Vec<&Round> = rounds
        .iter()
        .filter(|r| opts.peer.as_ref().is_none_or(|p| &r.peer == p))
        .collect();
    let mut out = String::new();
    let _ = writeln!(
        out,
        "claude-mesh report: {} pulling, since {}",
        opts.host,
        datetime(opts.since)
    );
    if rounds.is_empty() {
        let _ = writeln!(out, "\nno rounds in this period");
        return out;
    }

    // Per peer.
    let _ = writeln!(
        out,
        "\n{:<10} {:>6} {:>6} {:>8} {:>8} {:>10} {:>6} {:>6} {:>6} {:>5} {:>5}  LAST OK",
        "PEER",
        "ROUNDS",
        "ERRORS",
        "AVG",
        "MAX",
        "FETCHED",
        "TAILS",
        "PULLED",
        "MERGED",
        "CONFL",
        "FAIL",
    );
    let mut by_peer: BTreeMap<&str, Vec<&Round>> = BTreeMap::new();
    for r in &rounds {
        by_peer.entry(r.peer.as_str()).or_default().push(r);
    }
    for (peer, rs) in &by_peer {
        let n = rs.len() as u64;
        let errors = rs.iter().filter(|r| r.error.is_some()).count();
        let sum = |f: fn(&Round) -> u64| rs.iter().map(|r| f(r)).sum::<u64>();
        let last_ok = rs
            .iter()
            .rev()
            .find(|r| r.error.is_none())
            .map_or("never".to_string(), |r| datetime(r.at));
        let _ = writeln!(
            out,
            "{:<10} {:>6} {:>6} {:>8} {:>8} {:>10} {:>6} {:>6} {:>6} {:>5} {:>5}  {}",
            peer,
            n,
            errors,
            duration(sum(|r| r.ms) / n.max(1)),
            duration(rs.iter().map(|r| r.ms).max().unwrap_or(0)),
            bytes(sum(|r| r.fetched)),
            sum(|r| r.tails as u64),
            sum(|r| r.pulled as u64),
            sum(|r| r.merged as u64),
            sum(|r| r.conflicts as u64),
            sum(|r| r.failed as u64),
            last_ok
        );
    }

    // Per hour.
    let peers: Vec<&str> = by_peer.keys().copied().collect();
    let mut hours: BTreeMap<u64, BTreeMap<&str, u64>> = BTreeMap::new();
    for r in &rounds {
        *hours
            .entry(r.at / 3_600_000)
            .or_default()
            .entry(r.peer.as_str())
            .or_default() += r.fetched;
    }
    let _ = write!(out, "\nfetched per hour (UTC)\n{:<17}", "HOUR");
    for p in &peers {
        let _ = write!(out, " {p:>10}");
    }
    out.push('\n');
    for (hour, per) in &hours {
        let _ = write!(out, "{:<17}", datetime(hour * 3_600_000));
        for p in &peers {
            let _ = write!(out, " {:>10}", bytes(per.get(p).copied().unwrap_or(0)));
        }
        out.push('\n');
    }

    // Files that moved the most.
    let mut by_file: BTreeMap<&str, (u64, usize, usize)> = BTreeMap::new();
    for e in rounds.iter().flat_map(|r| &r.files) {
        if e.bytes > 0 {
            let f = by_file.entry(e.path.as_str()).or_default();
            f.0 += e.bytes;
            f.1 += 1;
            f.2 += usize::from(e.tail);
        }
    }
    let mut top: Vec<_> = by_file.into_iter().collect();
    top.sort_by_key(|(_, (b, _, _))| std::cmp::Reverse(*b));
    if !top.is_empty() {
        let _ = writeln!(
            out,
            "\nfiles that moved the most\n{:>10} {:>6} {:>6}  PATH",
            "FETCHED", "TIMES", "TAILS"
        );
        for (path, (b, times, tails)) in top.iter().take(10) {
            let _ = writeln!(out, "{:>10} {:>6} {:>6}  {}", bytes(*b), times, tails, path);
        }
    }

    // Latest merges, conflicts, failures and errors.
    let mut notable: Vec<(u64, &str, String)> = Vec::new();
    for r in &rounds {
        if let Some(e) = &r.error {
            notable.push((r.at, &r.peer, format!("round failed: {e}")));
        }
        for e in &r.files {
            if matches!(e.action, Action::Merged | Action::Conflict | Action::Failed) {
                notable.push((r.at, &r.peer, describe(e)));
            }
        }
    }
    if !notable.is_empty() {
        let _ = writeln!(out, "\nlatest merges, conflicts and failures");
        for (at, peer, what) in notable.iter().rev().take(15).rev() {
            let _ = writeln!(out, "{}  {:<9} {}", datetime(*at), peer, what);
        }
    }

    if opts.files {
        let _ = writeln!(out, "\nevery file");
        for r in &rounds {
            for e in &r.files {
                let _ = writeln!(out, "{}  {:<9} {}", datetime(r.at), r.peer, describe(e));
            }
        }
    }
    out
}

/// One file event as a line: `merged  projects/…/s.jsonl  12.0 KiB (tail)`.
fn describe(e: &FileEvent) -> String {
    let action = match e.action {
        Action::Pulled => "pulled",
        Action::Merged => "merged",
        Action::Conflict => "conflict",
        Action::Busy => "busy",
        Action::Failed => "failed",
    };
    let mut s = format!("{action:<8} {}", e.path);
    if e.bytes > 0 {
        let _ = write!(
            s,
            "  {}{}",
            bytes(e.bytes),
            if e.tail { " (tail)" } else { "" }
        );
    }
    if let Some(n) = &e.note {
        let _ = write!(s, "  [{n}]");
    }
    s
}

/// `1536` → `1.5 KiB`.
pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut v = n as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}

/// `1234` ms → `1.2s`; `95_000` → `1m35s`.
fn duration(ms: u64) -> String {
    match ms {
        0..1_000 => format!("{ms}ms"),
        1_000..60_000 => format!("{:.1}s", ms as f64 / 1000.0),
        _ => format!("{}m{:02}s", ms / 60_000, ms % 60_000 / 1000),
    }
}

/// Milliseconds since the epoch as `2026-10-11 21:18Z` (UTC, no time zone
/// database needed).
pub fn datetime(ms: u64) -> String {
    let secs = ms / 1000;
    let (days, rem) = (secs / 86_400, secs % 86_400);
    // Howard Hinnant's days-to-civil.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60
    )
}

/// `30m`, `2h`, `1d`, `45s` → milliseconds.
pub fn parse_duration(s: &str) -> Option<u64> {
    let (n, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit())?);
    let n: u64 = n.parse().ok()?;
    let per = match unit {
        "s" => 1_000,
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        _ => return None,
    };
    Some(n * per)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn datetimes_are_utc_civil_dates() {
        assert_eq!(datetime(0), "1970-01-01 00:00Z");
        // 2026-10-11 21:18:50 UTC
        assert_eq!(datetime(1_791_753_530_000), "2026-10-11 21:18Z");
        assert_eq!(datetime(951_782_400_000), "2000-02-29 00:00Z");
    }

    #[test]
    fn sizes_and_durations_read_well() {
        assert_eq!(bytes(512), "512 B");
        assert_eq!(bytes(1536), "1.5 KiB");
        assert_eq!(bytes(13_550 * 1024), "13.2 MiB");
        assert_eq!(duration(412), "412ms");
        assert_eq!(duration(95_000), "1m35s");
        assert_eq!(parse_duration("2h"), Some(7_200_000));
        assert_eq!(parse_duration("h"), None);
        assert_eq!(parse_duration("3w"), None);
    }

    #[test]
    fn the_journal_rotates_and_reads_back_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let j = path_for(&dir.path().join("state.json"));
        let r = |at| Round {
            at,
            peer: "caladan".into(),
            ..Round::default()
        };
        append(&j, &r(2)).unwrap();
        append(&j, &r(1)).unwrap();
        // Past the limit: the next append starts a new file.
        let big = std::fs::OpenOptions::new().append(true).open(&j).unwrap();
        big.set_len(MAX_BYTES + 1).unwrap();
        append(&j, &r(3)).unwrap();
        assert!(rotated(&j).exists());
        let ats: Vec<u64> = read(&j, 0).iter().map(|r| r.at).collect();
        assert_eq!(ats, [1, 2, 3]);
        assert_eq!(read(&j, 2).len(), 2);
    }
}
