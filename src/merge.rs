//! How two versions of a file become one.
//!
//! Every function here is **symmetric**, `merge(a, b) == merge(b, a)`, and
//! **idempotent**, merging the result with either input gives the result
//! back. Each Mac merges what it pulls into its own copy, in whatever order
//! the rounds happen; symmetry is what makes the three Macs end up with the
//! same bytes, and idempotence is what makes them stop changing once they do.
//!
//! | Files ([`strategy_for`])        | Rule                                 |
//! |---------------------------------|--------------------------------------|
//! | `*.jsonl` (transcripts, history)| [`merge_jsonl`]: longer prefix, or both branches |
//! | `memory/MEMORY.md`              | [`merge_line_union`]: every line once |
//! | everything else                 | [`last_writer_wins`], loser kept as a conflict copy |

use std::collections::HashSet;

/// Append-only logs (`*.jsonl`): transcripts, `history.jsonl`.
///
/// * One is a prefix of the other (the usual case: a session moved Macs and
///   kept going) → the longer one.
/// * They diverged, and one side's new lines are all in the other's → that
///   other side (what an earlier merge produced, merged again).
/// * Otherwise → the shared prefix, then both sides' new lines as one set,
///   sorted by `(timestamp, line)`, each line once. A line without a
///   timestamp counts as the time of the line before it on its side (the
///   earlier one if the sides differ), so it stays where it was written. Claude's
///   transcripts are trees linked by `parentUuid`, so interleaved branches
///   are still a valid file, and the newest line ends up last, where
///   `claude --resume` looks for the end of the conversation.
pub fn merge_jsonl(a: &str, b: &str) -> String {
    if a == b {
        return a.to_string();
    }
    // The usual case, without splitting either into lines: one side is the
    // other plus whole lines.
    if b.starts_with(a) && a.ends_with('\n') {
        return b.to_string();
    }
    if a.starts_with(b) && b.ends_with('\n') {
        return a.to_string();
    }
    let la: Vec<&str> = lines(a);
    let lb: Vec<&str> = lines(b);
    let common = la.iter().zip(&lb).take_while(|(x, y)| x == y).count();
    if common == lb.len() {
        return a.to_string();
    }
    if common == la.len() {
        return b.to_string();
    }

    let (prefix, ra, rb) = (&la[..common], &la[common..], &lb[common..]);
    let in_a: HashSet<&str> = ra.iter().copied().collect();
    let in_b: HashSet<&str> = rb.iter().copied().collect();
    // Strict inclusion only: two tails with the same lines in different
    // orders would make `a` win one way round and `b` the other.
    if in_b.len() < in_a.len() && in_b.is_subset(&in_a) {
        return a.to_string();
    }
    if in_a.len() < in_b.len() && in_a.is_subset(&in_b) {
        return b.to_string();
    }

    // The new lines of both sides as one set, in time order: the result
    // depends on which lines there are, not on the order either side wrote
    // them in, so merging it again with either side gives it back, and the
    // three Macs settle on the same file. (Taking the earlier of the two
    // sides' next lines instead didn't: a side already out of order, like
    // a file merged before this fix, gave a different order every round.)
    let since = prefix.iter().rev().find_map(|l| line_timestamp(l)).unwrap_or(i64::MIN);
    let mut keys: std::collections::HashMap<&str, i64> = std::collections::HashMap::new();
    for (key, line) in keyed(ra, since).into_iter().chain(keyed(rb, since)) {
        let k = keys.entry(line).or_insert(key);
        *k = (*k).min(key);
    }
    let in_prefix: HashSet<&str> = prefix.iter().copied().collect();
    let mut tail: Vec<(i64, &str)> = keys
        .into_iter()
        .filter(|(line, _)| !in_prefix.contains(line))
        .map(|(line, key)| (key, line))
        .collect();
    tail.sort_unstable();
    let mut out: Vec<&str> = prefix.to_vec();
    out.extend(tail.into_iter().map(|(_, line)| line));
    let mut s = out.join("\n");
    s.push('\n');
    s
}

/// Each line with the key it's interleaved by: its timestamp, or the last
/// one before it (starting from `since`, the shared prefix's last), then
/// the line itself.
fn keyed<'a>(tail: &[&'a str], since: i64) -> Vec<(i64, &'a str)> {
    let mut last = since;
    tail.iter()
        .map(|line| {
            if let Some(t) = line_timestamp(line) {
                last = t;
            }
            (last, *line)
        })
        .collect()
}

/// Line-set files (`MEMORY.md`, the memory index): every line from both,
/// shared lines once, in a stable order: lines of the version that sorts
/// first, then the other's extra lines.
pub fn merge_line_union(a: &str, b: &str) -> String {
    if a == b {
        return a.to_string();
    }
    let (first, second) = if a <= b { (a, b) } else { (b, a) };
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for line in lines(first).into_iter().chain(lines(second)) {
        if line.trim().is_empty() || seen.insert(line) {
            out.push(line);
        }
    }
    // Collapse runs of blank lines a union can create.
    out.dedup_by(|x, y| x.trim().is_empty() && y.trim().is_empty());
    let mut s = out.join("\n");
    s.push('\n');
    s
}

/// [`merge_line_union`] with a base, the peer's version we last merged:
/// lines either side removed since the base are gone, so an edited entry
/// (its old line removed, its new one added) leaves no stale copy and a
/// removed one doesn't come back. Symmetric in `mine` and `theirs` for the
/// same base.
pub fn merge_lines_3way(base: &str, mine: &str, theirs: &str) -> String {
    let (m, t): (HashSet<&str>, HashSet<&str>) = (lines(mine).into_iter().collect(), lines(theirs).into_iter().collect());
    let removed: HashSet<&str> = lines(base)
        .into_iter()
        .filter(|l| !l.trim().is_empty() && (!m.contains(l) || !t.contains(l)))
        .collect();
    let union = merge_line_union(mine, theirs);
    let mut out: Vec<&str> = lines(&union).into_iter().filter(|l| !removed.contains(l)).collect();
    out.dedup_by(|x, y| x.trim().is_empty() && y.trim().is_empty());
    let mut s = out.join("\n");
    s.push('\n');
    s
}

/// Everything else: the newer modification time wins; on a tie, the larger
/// content (then the bytes) so both sides pick the same one. Returns `true`
/// when `a` wins.
pub fn last_writer_wins(a_mtime: u64, a: &[u8], b_mtime: u64, b: &[u8]) -> bool {
    (a_mtime, a.len(), a) >= (b_mtime, b.len(), b)
}

/// Which merge a canonical path gets.
#[derive(Debug, PartialEq, Eq)]
pub enum Strategy {
    /// An append-only log: [`merge_jsonl`].
    Jsonl,
    /// A set of lines (the memory index): [`merge_line_union`].
    LineUnion,
    /// Anything else: [`last_writer_wins`], with a conflict copy when both
    /// sides changed it.
    LastWriterWins,
}

/// The merge for the canonical path `rel`, by its name alone.
pub fn strategy_for(rel: &str) -> Strategy {
    if rel.ends_with(".jsonl") {
        Strategy::Jsonl
    } else if rel.ends_with("/memory/MEMORY.md") || rel == "MEMORY.md" {
        Strategy::LineUnion
    } else {
        Strategy::LastWriterWins
    }
}

/// `s` split into lines, without their newlines.
fn lines(s: &str) -> Vec<&str> {
    s.lines().collect()
}

/// A line's `"timestamp"` in milliseconds since the Unix epoch: an RFC 3339
/// string (any fraction or offset, so `…:00Z`, `…:00.123Z` and `-03:00`
/// compare by the instant, not by their text) or a number (`history.jsonl`,
/// already in ms). `None` if the line isn't JSON or has no usable one.
fn line_timestamp(line: &str) -> Option<i64> {
    let v = serde_json::from_str::<serde_json::Value>(line).ok()?;
    match v.get("timestamp")? {
        serde_json::Value::String(s) => chrono::DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|t| t.timestamp_millis()),
        serde_json::Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_takes_the_longer() {
        let a = "1\n2\n";
        let b = "1\n2\n3\n";
        assert_eq!(merge_jsonl(a, b), b);
        assert_eq!(merge_jsonl(b, a), b);
    }

    #[test]
    fn diverged_logs_keep_both_branches_symmetrically() {
        let base = r#"{"uuid":"0","timestamp":"2026-10-08T10:00:00Z"}"#;
        let x = r#"{"uuid":"x","timestamp":"2026-10-08T11:00:00Z"}"#;
        let y = r#"{"uuid":"y","timestamp":"2026-10-08T10:30:00Z"}"#;
        let a = format!("{base}\n{x}\n");
        let b = format!("{base}\n{y}\n");
        let m1 = merge_jsonl(&a, &b);
        let m2 = merge_jsonl(&b, &a);
        assert_eq!(m1, m2);
        assert_eq!(m1, format!("{base}\n{y}\n{x}\n"));
        // Merging again changes nothing.
        assert_eq!(merge_jsonl(&m1, &a), m1);
        assert_eq!(merge_jsonl(&m1, &b), m1);
    }

    fn ts(t: &str) -> String {
        format!(r#"{{"timestamp":"2026-10-08T{t}:00.000Z"}}"#)
    }

    #[test]
    fn diverged_lines_interleave_by_timestamp() {
        let line = ts;
        let base = line("10:00");
        let a = format!("{base}\n{}\n{}\n", line("10:10"), line("10:40"));
        let b = format!("{base}\n{}\n{}\n", line("10:20"), line("10:30"));
        let want = ["10:00", "10:10", "10:20", "10:30", "10:40"]
            .map(line)
            .join("\n")
            + "\n";
        assert_eq!(merge_jsonl(&a, &b), want);
        assert_eq!(merge_jsonl(&b, &a), want);
    }

    #[test]
    fn timestamps_compare_by_instant_not_text() {
        // As text, "10:00:00Z" sorts after "10:00:00.500Z" ('.' < 'Z'), and
        // "07:30-03:00" (10:30 UTC) before "10:10Z".
        let base = r#"{"timestamp":"2026-10-08T09:00:00Z"}"#;
        let whole = r#"{"timestamp":"2026-10-08T10:00:00Z"}"#;
        let half = r#"{"timestamp":"2026-10-08T10:00:00.500Z"}"#;
        let ten = r#"{"timestamp":"2026-10-08T10:10:00Z"}"#;
        let brt = r#"{"timestamp":"2026-10-08T07:30:00-03:00"}"#;
        let a = [base, whole, brt].join("\n") + "\n";
        let b = [base, half, ten].join("\n") + "\n";
        let want = [base, whole, half, ten, brt].join("\n") + "\n";
        assert_eq!(merge_jsonl(&a, &b), want);
        assert_eq!(merge_jsonl(&b, &a), want);
    }

    #[test]
    fn a_side_already_out_of_order_still_settles() {
        // Like a transcript merged before the interleaving fix: an old block
        // (09:30) after newer lines. Merging must reach a fixed point, or
        // two Macs rewrite (and resend) the whole file every round.
        let a = [ts("09:00"), ts("11:00"), ts("09:30"), ts("12:00")].join("\n") + "\n";
        let b = [ts("09:00"), ts("10:00"), ts("11:30")].join("\n") + "\n";
        let m = merge_jsonl(&a, &b);
        assert_eq!(m, merge_jsonl(&b, &a));
        for side in [&a, &b] {
            assert_eq!(merge_jsonl(&m, side), m);
            assert_eq!(merge_jsonl(side, &m), m);
        }
        // A third Mac with its own new line converges on the same file.
        let c = [ts("09:00"), ts("10:00"), ts("11:45")].join("\n") + "\n";
        let mc = merge_jsonl(&m, &c);
        assert_eq!(merge_jsonl(&merge_jsonl(&a, &c), &b), mc);
        assert_eq!(merge_jsonl(&mc, &m), mc);
    }

    #[test]
    fn interleaving_is_idempotent() {
        let a = [ts("10:00"), ts("10:10"), ts("10:40")].join("\n") + "\n";
        let b = [ts("10:00"), ts("10:20"), ts("10:30")].join("\n") + "\n";
        let m = merge_jsonl(&a, &b);
        assert_eq!(merge_jsonl(&m, &a), m);
        assert_eq!(merge_jsonl(&a, &m), m);
        assert_eq!(merge_jsonl(&m, &b), m);
        assert_eq!(merge_jsonl(&b, &m), m);
    }

    #[test]
    fn a_line_without_timestamp_stays_after_its_predecessor() {
        let plain = r#"{"type":"summary"}"#;
        let a = [ts("10:00"), ts("10:10"), plain.into(), ts("10:40")].join("\n") + "\n";
        let b = [ts("10:00"), ts("10:20")].join("\n") + "\n";
        let want = [ts("10:00"), ts("10:10"), plain.into(), ts("10:20"), ts("10:40")].join("\n") + "\n";
        assert_eq!(merge_jsonl(&a, &b), want);
        assert_eq!(merge_jsonl(&b, &a), want);
    }

    #[test]
    fn memory_index_union_is_symmetric() {
        let a = "- [A](a.md) — one\n- [B](b.md) — two\n";
        let b = "- [A](a.md) — one\n- [C](c.md) — three\n";
        let m = merge_line_union(a, b);
        assert_eq!(m, merge_line_union(b, a));
        for needle in ["a.md", "b.md", "c.md"] {
            assert_eq!(m.matches(needle).count(), 1, "{needle} in {m}");
        }
    }

    #[test]
    fn last_writer_wins_is_a_total_order() {
        assert!(last_writer_wins(2, b"x", 1, b"y"));
        assert!(!last_writer_wins(1, b"x", 2, b"y"));
        assert_ne!(
            last_writer_wins(1, b"x", 1, b"y"),
            last_writer_wins(1, b"y", 1, b"x")
        );
    }

    #[test]
    fn strategies() {
        assert_eq!(strategy_for("history.jsonl"), Strategy::Jsonl);
        assert_eq!(
            strategy_for("projects/k/memory/MEMORY.md"),
            Strategy::LineUnion
        );
        assert_eq!(
            strategy_for("projects/k/memory/note.md"),
            Strategy::LastWriterWins
        );
    }
}
