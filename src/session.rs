//! `claude-mesh session`: a session's outline, to find your way back into it
//! days later.
//!
//! The transcript is read in file order (time order, once merged or
//! repaired) and cut into stretches by Mac and day: who it ran on, when,
//! how many prompts, and the first one, so it's recognizable. Context
//! compactions show as separators. The Mac comes from the origins registry
//! ([`crate::origin`]); lines from before it existed show as `?`.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::path::Path;

/// One stretch of a session: consecutive lines on the same Mac and day.
#[derive(Debug, PartialEq)]
pub struct Stretch {
    /// The Mac the lines were written on, or `?`.
    pub host: String,
    /// First and last timestamp (`YYYY-MM-DDTHH:MM`, UTC).
    pub from: String,
    /// See `from`.
    pub to: String,
    /// Prompts Pedro typed in it (not tool results, not meta lines).
    pub prompts: usize,
    /// The first of them, cut short.
    pub first: Option<String>,
}

/// One entry of the outline.
#[derive(Debug, PartialEq)]
pub enum Entry {
    /// A stretch of conversation.
    Stretch(Stretch),
    /// The context was compacted here.
    Compacted(String),
}

/// What [`outline`] needs from a transcript line; the rest is skipped
/// without being built.
#[derive(serde::Deserialize)]
struct Line {
    /// `user`, `assistant`, `system`, …
    #[serde(rename = "type")]
    kind: Option<String>,
    /// For `system` lines: `compact_boundary` marks a compaction.
    subtype: Option<String>,
    /// The line's id, the key of the origins registry.
    uuid: Option<String>,
    /// When it was written.
    timestamp: Option<String>,
    /// Lines Claude Code adds itself (caveats, command echoes).
    #[serde(rename = "isMeta")]
    is_meta: Option<bool>,
    /// The message, for a prompt's text.
    message: Option<Message>,
}

/// A line's message, for a user prompt's text.
#[derive(serde::Deserialize)]
struct Message {
    /// A string, or parts (text, tool results, images).
    content: Option<serde_json::Value>,
}

/// The text of a prompt Pedro typed, if `content` is one (a string, or text
/// parts; tool results don't count).
fn prompt_text(content: &serde_json::Value) -> Option<String> {
    match content {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Array(parts) => {
            if parts.iter().any(|p| p["type"] == "tool_result") {
                return None;
            }
            let text: Vec<&str> = parts.iter().filter_map(|p| p["text"].as_str()).collect();
            (!text.is_empty()).then(|| text.join(" "))
        }
        _ => None,
    }
    .map(|t| t.trim().to_string())
    .filter(|t| !t.is_empty() && !t.starts_with('<'))
}

/// The outline of the transcript read from `reader`, with line origins from
/// `origins` (uuid → host).
pub fn outline(reader: impl BufRead, origins: &BTreeMap<String, String>) -> Vec<Entry> {
    let mut out: Vec<Entry> = Vec::new();
    for line in reader.lines().map_while(Result::ok) {
        let Ok(l) = serde_json::from_str::<Line>(&line) else {
            continue;
        };
        let Some(ts) = l.timestamp.as_deref().filter(|t| t.len() >= 16) else {
            continue;
        };
        let minute = ts[..16].to_string();
        if l.kind.as_deref() == Some("system") && l.subtype.as_deref() == Some("compact_boundary") {
            out.push(Entry::Compacted(minute));
            continue;
        }
        let host = l
            .uuid
            .as_ref()
            .and_then(|u| origins.get(u))
            .cloned()
            .unwrap_or_else(|| "?".into());
        let prompt = (l.kind.as_deref() == Some("user") && l.is_meta != Some(true))
            .then(|| l.message.as_ref()?.content.as_ref().and_then(prompt_text))
            .flatten();
        let same = matches!(out.last(), Some(Entry::Stretch(s)) if s.host == host && s.from[..10] == minute[..10]);
        if !same {
            out.push(Entry::Stretch(Stretch {
                host,
                from: minute.clone(),
                to: minute.clone(),
                prompts: 0,
                first: None,
            }));
        }
        if let Some(Entry::Stretch(s)) = out.last_mut() {
            s.to = minute;
            if let Some(p) = prompt {
                s.prompts += 1;
                s.first.get_or_insert_with(|| p.chars().take(60).collect());
            }
        }
    }
    out
}

/// Print `path`'s outline (`session` is its id, for the origins registry).
pub fn print(config: &Path, session: &str, path: &Path) -> std::io::Result<()> {
    let file = std::fs::File::open(path)?;
    let entries = outline(BufReader::new(file), &crate::origin::read_registry(config, session));
    println!("{session}  {}", path.display());
    for e in entries {
        match e {
            Entry::Compacted(at) => println!("  {at}   ── context compacted ──"),
            Entry::Stretch(s) => {
                let to = if s.to[..10] == s.from[..10] { &s.to[11..] } else { &s.to[..] };
                println!(
                    "  {}–{to}  {:<9} {:>4} prompts  {}",
                    s.from,
                    s.host,
                    s.prompts,
                    s.first.map(|f| format!("\"{f}\"")).unwrap_or_default()
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stretches_by_mac_and_day_with_compactions() {
        let user = |u: &str, t: &str, text: &str| {
            format!(r#"{{"type":"user","uuid":"{u}","timestamp":"{t}:00.000Z","message":{{"role":"user","content":"{text}"}}}}"#)
        };
        let reply = |u: &str, t: &str| format!(r#"{{"type":"assistant","uuid":"{u}","timestamp":"{t}:05.000Z"}}"#);
        let tool = |u: &str, t: &str| {
            format!(r#"{{"type":"user","uuid":"{u}","timestamp":"{t}:06.000Z","message":{{"content":[{{"type":"tool_result","content":"x"}}]}}}}"#)
        };
        let compact = r#"{"type":"system","subtype":"compact_boundary","timestamp":"2026-10-12T03:00:00.000Z"}"#;
        let text = [
            user("u1", "2026-10-11T20:00", "pronto, vamos dar continuidade"),
            reply("r1", "2026-10-11T20:00"),
            tool("t1", "2026-10-11T20:01"),
            user("u2", "2026-10-11T21:30", "go go go"),
            compact.to_string(),
            user("u3", "2026-10-12T04:00", "bora"),
            reply("r3", "2026-10-12T04:00"),
            user("u4", "2026-10-13T01:19", "oie"),
        ]
        .join("\n");
        let origins: BTreeMap<String, String> = [("u3", "arrakis"), ("r3", "arrakis"), ("u4", "caladan")]
            .map(|(u, h)| (u.to_string(), h.to_string()))
            .into();
        let entries = outline(text.as_bytes(), &origins);
        let stretch = |host: &str, from: &str, to: &str, prompts, first: &str| {
            Entry::Stretch(Stretch {
                host: host.into(),
                from: from.into(),
                to: to.into(),
                prompts,
                first: Some(first.into()),
            })
        };
        assert_eq!(
            entries,
            vec![
                stretch("?", "2026-10-11T20:00", "2026-10-11T21:30", 2, "pronto, vamos dar continuidade"),
                Entry::Compacted("2026-10-12T03:00".into()),
                stretch("arrakis", "2026-10-12T04:00", "2026-10-12T04:00", 1, "bora"),
                stretch("caladan", "2026-10-13T01:19", "2026-10-13T01:19", 1, "oie"),
            ]
        );
    }
}
