//! Machine-independent ("canonical") names and contents.
//!
//! The Macs keep sources and Claude config in different places (see
//! [`crate::layout`]), and Claude bakes those absolute paths into project
//! folder names and into transcripts. Before anything leaves a Mac it is
//! rewritten into canonical form, with the local paths replaced by tokens;
//! the receiving Mac turns the tokens back into its own paths.
//!
//! | Local (arrakis)                             | Canonical            |
//! |---------------------------------------------|----------------------|
//! | `/Users/pplanel/src/pplanel/.claude`        | `@CFG@`              |
//! | `/Users/pplanel/src/pplanel`                | `@SRC@`              |
//! | `projects/-Users-pplanel-src-pplanel-x`     | `projects/@SRCKEY@-x`|
//!
//! Comparing canonical hashes is what lets the same conversation on two Macs
//! count as "the same file".

use crate::layout::{Host, project_key};

pub const CFG: &str = "@CFG@";
pub const SRC: &str = "@SRC@";
pub const SRCKEY: &str = "@SRCKEY@";

/// File extensions whose contents get path rewriting. Everything else is
/// copied byte for byte.
const TEXT_EXTENSIONS: &[&str] = &["jsonl", "json", "md", "txt"];

#[derive(Clone, Debug)]
pub struct Canon {
    src: String,
    config: String,
    src_key: String,
    config_key: String,
}

impl Canon {
    pub fn new(host: &Host) -> Self {
        let src = host.src.to_string_lossy().trim_end_matches('/').to_string();
        let config = host
            .config
            .to_string_lossy()
            .trim_end_matches('/')
            .to_string();
        Canon {
            src_key: project_key(&src),
            config_key: project_key(&config),
            src,
            config,
        }
    }

    /// A path relative to the Claude config folder, in canonical form.
    /// Only the project folder name (`projects/<key>/…`) depends on the Mac.
    pub fn rel_to_canonical(&self, rel: &str) -> String {
        match split_project(rel) {
            Some((key, rest)) => format!("projects/{}{}", self.key_to_canonical(key), rest),
            None => rel.to_string(),
        }
    }

    pub fn rel_from_canonical(&self, rel: &str) -> String {
        match split_project(rel) {
            Some((key, rest)) => format!("projects/{}{}", self.key_from_canonical(key), rest),
            None => rel.to_string(),
        }
    }

    fn key_to_canonical(&self, key: &str) -> String {
        // The config folder can live under the source folder (arrakis), so its
        // key is the longer match and goes first.
        if let Some(rest) = strip_key_prefix(key, &self.config_key) {
            return format!("@CFGKEY@{rest}");
        }
        if let Some(rest) = strip_key_prefix(key, &self.src_key) {
            return format!("{SRCKEY}{rest}");
        }
        key.to_string()
    }

    fn key_from_canonical(&self, key: &str) -> String {
        if let Some(rest) = key.strip_prefix("@CFGKEY@") {
            return format!("{}{rest}", self.config_key);
        }
        if let Some(rest) = key.strip_prefix(SRCKEY) {
            return format!("{}{rest}", self.src_key);
        }
        key.to_string()
    }

    pub fn is_text(rel: &str) -> bool {
        rel.rsplit_once('.')
            .is_some_and(|(_, ext)| TEXT_EXTENSIONS.contains(&ext))
    }

    /// File contents in canonical form. Binary files pass through unchanged.
    pub fn content_to_canonical(&self, rel: &str, bytes: &[u8]) -> Vec<u8> {
        if !Self::is_text(rel) {
            return bytes.to_vec();
        }
        let Ok(text) = std::str::from_utf8(bytes) else {
            return bytes.to_vec();
        };
        // Longest first: on arrakis the config path contains the source path.
        let text = text
            .replace(
                &format!("projects/{}", self.src_key),
                &format!("projects/{SRCKEY}"),
            )
            .replace(&self.config, CFG)
            .replace(&self.src, SRC);
        text.into_bytes()
    }

    pub fn content_from_canonical(&self, rel: &str, bytes: &[u8]) -> Vec<u8> {
        if !Self::is_text(rel) {
            return bytes.to_vec();
        }
        let Ok(text) = std::str::from_utf8(bytes) else {
            return bytes.to_vec();
        };
        let text = text
            .replace(
                &format!("projects/{SRCKEY}"),
                &format!("projects/{}", self.src_key),
            )
            .replace(CFG, &self.config)
            .replace(SRC, &self.src);
        text.into_bytes()
    }
}

/// `projects/<key>/rest` → `(key, "/rest")`; `projects/<key>` → `(key, "")`.
fn split_project(rel: &str) -> Option<(&str, &str)> {
    let after = rel.strip_prefix("projects/")?;
    match after.find('/') {
        Some(i) => Some((&after[..i], &after[i..])),
        None => Some((after, "")),
    }
}

/// `key` equals `prefix` or continues it at a `-` boundary.
fn strip_key_prefix<'a>(key: &'a str, prefix: &str) -> Option<&'a str> {
    let rest = key.strip_prefix(prefix)?;
    (rest.is_empty() || rest.starts_with('-')).then_some(rest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::find;

    fn arrakis() -> Canon {
        Canon::new(&find("arrakis").unwrap())
    }
    fn imperium() -> Canon {
        Canon::new(&find("imperium").unwrap())
    }

    #[test]
    fn project_folders_map_between_layouts() {
        let a = "projects/-Users-pplanel-src-pplanel-shift-compute/abc.jsonl";
        let canon = arrakis().rel_to_canonical(a);
        assert_eq!(canon, "projects/@SRCKEY@-shift-compute/abc.jsonl");
        assert_eq!(
            imperium().rel_from_canonical(&canon),
            "projects/-Users-pplanel-src-shift-compute/abc.jsonl"
        );
        assert_eq!(arrakis().rel_from_canonical(&canon), a);
    }

    #[test]
    fn home_project_is_the_same_everywhere() {
        let rel = "projects/-Users-pplanel/memory/MEMORY.md";
        assert_eq!(arrakis().rel_to_canonical(rel), rel);
        assert_eq!(imperium().rel_from_canonical(rel), rel);
    }

    #[test]
    fn src_key_needs_a_boundary() {
        // "-Users-pplanel-srcfoo" is not under the source folder.
        let rel = "projects/-Users-pplanel-srcfoo/x.jsonl";
        assert_eq!(imperium().rel_to_canonical(rel), rel);
    }

    #[test]
    fn content_round_trips_across_layouts() {
        let on_arrakis = r#"{"cwd":"/Users/pplanel/src/pplanel/shift-compute","m":"/Users/pplanel/src/pplanel/.claude/projects/-Users-pplanel-src-pplanel-shift-compute/memory/x.md"}"#;
        let canon = arrakis().content_to_canonical("a.jsonl", on_arrakis.as_bytes());
        let on_imperium = imperium().content_from_canonical("a.jsonl", &canon);
        assert_eq!(
            String::from_utf8(on_imperium).unwrap(),
            r#"{"cwd":"/Users/pplanel/src/shift-compute","m":"/Users/pplanel/.claude/projects/-Users-pplanel-src-shift-compute/memory/x.md"}"#
        );
    }

    #[test]
    fn binary_files_are_untouched() {
        let bytes = b"/Users/pplanel/src/pplanel \xff";
        assert_eq!(arrakis().content_to_canonical("x.png", bytes), bytes);
    }
}
