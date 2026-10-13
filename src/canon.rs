//! Machine-independent ("canonical") names and contents.
//!
//! The Macs keep sources and Claude config in different places (see
//! [`crate::layout`]), and Claude bakes those absolute paths into project
//! folder names and into transcripts. Before anything leaves a Mac it is
//! rewritten into canonical form, with the local paths replaced by tokens;
//! the receiving Mac turns the tokens back into its own paths.
//!
//! | Local (arrakis)                                   | Canonical                |
//! |---------------------------------------------------|--------------------------|
//! | `/Users/pplanel/src/pplanel/.claude` (in text)    | [`CFG`] `@CFG@`          |
//! | `/Users/pplanel/src/pplanel` (in text)            | [`SRC`] `@SRC@`          |
//! | `projects/-Users-pplanel-src-pplanel-x` (folder)  | `projects/@SRCKEY@-x`    |
//! | `projects/-Users-pplanel-src-pplanel--claude-x`   | `projects/@CFGKEY@-x`    |
//!
//! On caladan the same session lives in `projects/-Users-pplanel-src-x` and
//! mentions `/Users/pplanel/src/x`; both sides produce the same canonical
//! bytes, so the same conversation on two Macs has the same hash and counts
//! as "the same file".
//!
//! Two parts, two rules:
//!
//! * **Names** ([`Canon::rel_to_canonical`]): only the project folder under
//!   `projects/` depends on the Mac, and only at a `-` boundary, so
//!   `-Users-pplanel-srcfoo` is never mistaken for the source folder.
//! * **Contents** ([`Canon::content_to_canonical`]): text files only
//!   (`.jsonl`, `.json`, `.md`, `.txt`), as plain string replacement, longest
//!   path first, because on arrakis the config folder is inside the source
//!   folder. Anything else is copied byte for byte.
//!
//! # When it isn't reversible
//!
//! Replacing text is not a bijection. On arrakis, `@SRC@/.claude` and
//! `@CFG@` both turn into `/Users/pplanel/src/pplanel/.claude`, and text that
//! *quotes* another Mac's path (`/Users/pplanel/src/.claude` is plain text
//! on arrakis, `@SRC@/.claude` on caladan) reads back differently than it was
//! written. So a file written from canonical bytes `c` can read back as
//! canonical bytes `c' != c`. [`crate::sync`] never assumes the round trip:
//! it hashes what it reads back, compares files on disk, and keeps the peer's
//! copy as the base for tails. The quoted path itself may end up rewritten;
//! that's the documented limit of textual rewriting.

use std::borrow::Cow;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};

use crate::layout::{Host, project_key};

/// Stands for this Mac's Claude config folder in canonical contents.
pub const CFG: &str = "@CFG@";
/// Stands for this Mac's source folder in canonical contents.
pub const SRC: &str = "@SRC@";
/// Stands for the source folder's project key (`-Users-pplanel-src…`), in
/// canonical project folder names and in `projects/…` paths inside contents.
pub const SRCKEY: &str = "@SRCKEY@";
/// Stands for the config folder's project key, in canonical project folder
/// names (sessions started inside the config folder itself).
pub const CFGKEY: &str = "@CFGKEY@";

/// File extensions whose contents get path rewriting. Everything else is
/// copied byte for byte.
const TEXT_EXTENSIONS: &[&str] = &["jsonl", "json", "md", "txt"];

/// One Mac's translation to and from canonical form, built from its
/// [`Host`] entry.
///
/// ```ignore
/// let arrakis = Canon::new(&layout::find("arrakis").unwrap());
/// assert_eq!(
///     arrakis.rel_to_canonical("projects/-Users-pplanel-src-pplanel-x/s.jsonl"),
///     "projects/@SRCKEY@-x/s.jsonl"
/// );
/// ```
#[derive(Clone, Debug)]
pub struct Canon {
    /// The source folder, without a trailing `/`.
    src: String,
    /// The Claude config folder, without a trailing `/`.
    config: String,
    /// [`project_key`] of `src`, e.g. `-Users-pplanel-src-pplanel`.
    src_key: String,
    /// [`project_key`] of `config`, e.g. `-Users-pplanel-src-pplanel--claude`.
    config_key: String,
}

impl Canon {
    /// The translation for `host`'s source and config folders.
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

    /// The inverse of [`Canon::rel_to_canonical`]: a canonical path relative
    /// to the config folder, as it is named on this Mac.
    pub fn rel_from_canonical(&self, rel: &str) -> String {
        match split_project(rel) {
            Some((key, rest)) => format!("projects/{}{}", self.key_from_canonical(key), rest),
            None => rel.to_string(),
        }
    }

    /// A project folder name in canonical form: the config key or the source
    /// key, at a `-` boundary, replaced by its token; anything else as is
    /// (e.g. `-Users-pplanel`, the home folder, is the same everywhere).
    fn key_to_canonical(&self, key: &str) -> String {
        // The config folder can live under the source folder (arrakis), so its
        // key is the longer match and goes first.
        if let Some(rest) = strip_key_prefix(key, &self.config_key) {
            return format!("{CFGKEY}{rest}");
        }
        if let Some(rest) = strip_key_prefix(key, &self.src_key) {
            return format!("{SRCKEY}{rest}");
        }
        key.to_string()
    }

    /// The inverse of [`Canon::key_to_canonical`].
    fn key_from_canonical(&self, key: &str) -> String {
        if let Some(rest) = key.strip_prefix(CFGKEY) {
            return format!("{}{rest}", self.config_key);
        }
        if let Some(rest) = key.strip_prefix(SRCKEY) {
            return format!("{}{rest}", self.src_key);
        }
        key.to_string()
    }

    /// Whether `rel`'s contents get path rewriting (its extension is one of
    /// `TEXT_EXTENSIONS`).
    pub fn is_text(rel: &str) -> bool {
        rel.rsplit_once('.')
            .is_some_and(|(_, ext)| TEXT_EXTENSIONS.contains(&ext))
    }

    /// File contents in canonical form: this Mac's `projects/<src key>`,
    /// config folder and source folder replaced by their tokens, longest
    /// first. Binary files, and text that isn't UTF-8, pass through unchanged.
    pub fn content_to_canonical(&self, rel: &str, bytes: &[u8]) -> Vec<u8> {
        if !Self::is_text(rel) {
            return bytes.to_vec();
        }
        let Ok(text) = std::str::from_utf8(bytes) else {
            return bytes.to_vec();
        };
        self.text_to_canonical(text).into_owned().into_bytes()
    }

    /// The BLAKE3 hash of [`Canon::content_to_canonical`]`(rel, contents)`,
    /// read from `reader` without holding the whole file in memory: text
    /// is rewritten line by line, anything else hashed in blocks. Equal to
    /// hashing the whole canonical contents because no path or token
    /// contains a newline, and a newline byte never sits inside a UTF-8
    /// sequence. A text file that turns out not to be UTF-8 is hashed again
    /// from the start, unchanged, as `content_to_canonical` passes it.
    pub fn hash_canonical<R: Read + Seek>(
        &self,
        rel: &str,
        mut reader: R,
    ) -> std::io::Result<blake3::Hash> {
        let mut hasher = blake3::Hasher::new();
        if Self::is_text(rel) {
            let mut lines = BufReader::new(&mut reader);
            let mut line = Vec::new();
            loop {
                line.clear();
                if lines.read_until(b'\n', &mut line)? == 0 {
                    return Ok(hasher.finalize());
                }
                let Ok(text) = std::str::from_utf8(&line) else {
                    break;
                };
                hasher.update(self.text_to_canonical(text).as_bytes());
            }
            drop(lines);
            hasher.reset();
            reader.seek(SeekFrom::Start(0))?;
        }
        hasher.update_reader(reader)?;
        Ok(hasher.finalize())
    }

    /// Call `f` with the canonical contents of the first `limit` bytes of
    /// `reader` (from its start), in order: a line at a time for text, a
    /// block at a time for anything else. The same bytes as
    /// [`Canon::content_to_canonical`], without holding the file in memory.
    /// Text is checked for UTF-8 first (a file that isn't passes through
    /// unchanged), then read again to rewrite it; `limit` makes both reads,
    /// and two calls, see the same contents while the file is appended to.
    pub fn for_each_canonical<R: Read + Seek>(
        &self,
        rel: &str,
        mut reader: R,
        limit: u64,
        mut f: impl FnMut(&[u8]) -> std::io::Result<()>,
    ) -> std::io::Result<()> {
        let mut line = Vec::new();
        if Self::is_text(rel) {
            let mut utf8 = true;
            let mut lines = BufReader::new((&mut reader).take(limit));
            while {
                line.clear();
                lines.read_until(b'\n', &mut line)? > 0
            } {
                if std::str::from_utf8(&line).is_err() {
                    utf8 = false;
                    break;
                }
            }
            drop(lines);
            reader.seek(SeekFrom::Start(0))?;
            if utf8 {
                let mut lines = BufReader::new((&mut reader).take(limit));
                loop {
                    line.clear();
                    if lines.read_until(b'\n', &mut line)? == 0 {
                        return Ok(());
                    }
                    // Valid on the first read; if the file changed since,
                    // pass the line as is rather than fail.
                    match std::str::from_utf8(&line) {
                        Ok(text) => f(self.text_to_canonical(text).as_bytes())?,
                        Err(_) => f(&line)?,
                    }
                }
            }
        }
        let mut block = reader.take(limit);
        line.resize(64 << 10, 0);
        loop {
            let n = block.read(&mut line)?;
            if n == 0 {
                return Ok(());
            }
            f(&line[..n])?;
        }
    }

    /// The path rewriting of [`Canon::content_to_canonical`], on text: this
    /// Mac's `projects/<src key>`, config folder and source folder replaced
    /// by their tokens, longest first (on arrakis the config path contains
    /// the source path). Borrowed when there's nothing to replace.
    fn text_to_canonical<'a>(&self, text: &'a str) -> Cow<'a, str> {
        let project = format!("projects/{}", self.src_key);
        if !text.contains(&project) && !text.contains(&self.config) && !text.contains(&self.src) {
            return Cow::Borrowed(text);
        }
        Cow::Owned(
            text.replace(&project, &format!("projects/{SRCKEY}"))
                .replace(&self.config, CFG)
                .replace(&self.src, SRC),
        )
    }

    /// Canonical contents in this Mac's form: the tokens replaced by its own
    /// paths. Not always the exact inverse of [`Canon::content_to_canonical`];
    /// see the module docs.
    pub fn content_from_canonical(&self, rel: &str, bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(bytes.len());
        self.for_each_from_canonical(rel, bytes, |b| {
            out.extend_from_slice(b);
            Ok(())
        })
        .expect("writing to a Vec doesn't fail");
        out
    }

    /// Call `f` with [`Canon::content_from_canonical`]`(rel, bytes)` a line
    /// at a time (or whole, for binary files and text that isn't UTF-8), so
    /// it can be hashed or written without a second copy of the file.
    pub fn for_each_from_canonical(
        &self,
        rel: &str,
        bytes: &[u8],
        mut f: impl FnMut(&[u8]) -> std::io::Result<()>,
    ) -> std::io::Result<()> {
        let text = match std::str::from_utf8(bytes) {
            Ok(text) if Self::is_text(rel) => text,
            _ => return f(bytes),
        };
        let project = format!("projects/{SRCKEY}");
        let local_project = format!("projects/{}", self.src_key);
        for line in text.split_inclusive('\n') {
            if !line.contains(&project) && !line.contains(CFG) && !line.contains(SRC) {
                f(line.as_bytes())?;
            } else {
                let line = line
                    .replace(&project, &local_project)
                    .replace(CFG, &self.config)
                    .replace(SRC, &self.src);
                f(line.as_bytes())?;
            }
        }
        Ok(())
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
    fn streaming_hash_matches_whole_file_hash() {
        let cases: &[(&str, &[u8])] = &[
            ("empty.jsonl", b""),
            (
                "a.jsonl",
                b"{\"cwd\":\"/Users/pplanel/src/pplanel/x\"}\n{\"m\":\"/Users/pplanel/src/pplanel/.claude/projects/-Users-pplanel-src-pplanel-x/m.md\"}\n",
            ),
            ("no-trailing-newline.md", b"line one\n/Users/pplanel/src/pplanel/y"),
            ("crlf.txt", b"/Users/pplanel/src/pplanel/a\r\nplain\r\n"),
            ("untouched.json", b"{\"k\":\"nothing to replace\"}\n"),
            (
                "not-utf8.jsonl",
                b"/Users/pplanel/src/pplanel/a\n\xff\xfe /Users/pplanel/src/pplanel/b\n",
            ),
            ("binary.png", b"/Users/pplanel/src/pplanel \xff\n"),
        ];
        for canon in [arrakis(), imperium()] {
            for (rel, bytes) in cases {
                let streamed = canon
                    .hash_canonical(rel, std::io::Cursor::new(bytes))
                    .unwrap();
                let whole = blake3::hash(&canon.content_to_canonical(rel, bytes));
                assert_eq!(streamed, whole, "{rel}");
            }
        }
    }

    #[test]
    fn streaming_hash_does_not_load_the_file() {
        // A 16 MiB transcript where every line has a path to rewrite.
        let line = r#"{"cwd":"/Users/pplanel/src/pplanel/shift-compute","m":"/Users/pplanel/src/pplanel/.claude/projects/-Users-pplanel-src-pplanel-shift-compute/x.md"}"#;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.jsonl");
        let mut contents = String::new();
        while contents.len() < 16 << 20 {
            contents.push_str(line);
            contents.push('\n');
        }
        std::fs::write(&path, &contents).unwrap();
        let size = contents.len();
        drop(contents);

        let canon = arrakis();
        let whole = crate::heap::peak_during(|| {
            let bytes = std::fs::read(&path).unwrap();
            blake3::hash(&canon.content_to_canonical("big.jsonl", &bytes))
        });
        let streamed = crate::heap::peak_during(|| {
            let file = std::fs::File::open(&path).unwrap();
            canon.hash_canonical("big.jsonl", file).unwrap()
        });
        eprintln!("file {size} B: whole-file peak {whole} B, streamed peak {streamed} B");

        assert!(whole > size, "the old way holds at least the whole file");
        assert!(streamed < 64 << 10, "streaming holds a line, not the file");
    }

    #[test]
    fn binary_files_are_untouched() {
        let bytes = b"/Users/pplanel/src/pplanel \xff";
        assert_eq!(arrakis().content_to_canonical("x.png", bytes), bytes);
    }
}
