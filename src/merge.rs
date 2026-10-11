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
/// * They diverged → the shared prefix, then each side's new lines as a
///   block, blocks ordered by their first timestamp (ties by content), with
///   lines already present skipped. Claude's transcripts are trees linked by
///   `parentUuid`, so two branches side by side are still a valid file.
pub fn merge_jsonl(a: &str, b: &str) -> String {
    if a == b {
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

    let (ra, rb) = (&la[common..], &lb[common..]);
    let mut blocks = [ra, rb];
    blocks.sort_by_key(|blk| (first_timestamp(blk), blk.join("\n")));

    let mut seen: HashSet<&str> = la[..common].iter().copied().collect();
    let mut out: Vec<&str> = la[..common].to_vec();
    for blk in blocks {
        for line in blk {
            if seen.insert(line) {
                out.push(line);
            }
        }
    }
    let mut s = out.join("\n");
    s.push('\n');
    s
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

/// The `"timestamp":"…"` of the first line that has one, as text (ISO 8601
/// strings sort chronologically); numeric timestamps (`history.jsonl`) are
/// zero-padded so they sort too.
fn first_timestamp(block: &[&str]) -> String {
    for line in block {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        match v.get("timestamp") {
            Some(serde_json::Value::String(s)) => return s.clone(),
            Some(serde_json::Value::Number(n)) => {
                return format!("{:020}", n.as_u64().unwrap_or(0));
            }
            _ => {}
        }
    }
    String::new()
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
