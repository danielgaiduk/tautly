//! Entry types, ids and title rules. The file format is in `frontmatter.rs`.

use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::MemoryError;
use crate::frontmatter;

/// Longest entry body, in tokens.
pub const MAX_BODY_TOKENS: u32 = 300;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind {
    Convention,
    Fact,
    Gotcha,
    Command,
}

impl MemoryKind {
    pub fn as_str(self) -> &'static str {
        match self {
            MemoryKind::Convention => "convention",
            MemoryKind::Fact => "fact",
            MemoryKind::Gotcha => "gotcha",
            MemoryKind::Command => "command",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "convention" => Some(MemoryKind::Convention),
            "fact" => Some(MemoryKind::Fact),
            "gotcha" => Some(MemoryKind::Gotcha),
            "command" => Some(MemoryKind::Command),
            _ => None,
        }
    }
}

/// A piece of code an entry is about: a file (`symbol` None) or one definition in it, with
/// the hash it had when the entry was written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemAnchor {
    pub path: PathBuf,
    pub symbol: Option<String>,
    pub hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub id: String,
    pub title: String,
    pub kind: MemoryKind,
    /// Obsidian tags, without `#`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    pub anchors: Vec<MemAnchor>,
    pub created: SystemTime,
    pub verified: Option<SystemTime>,
    pub body: String,
    /// The `[[wikilink]]` targets of the body, as written (label and heading left out),
    /// each once. Derived from the body, never stored apart from it.
    #[serde(skip)]
    pub links: Vec<String>,
    /// An anchor's code changed since the entry was written. Never stored in the file or in
    /// a serialized proposal; it reads back `false`.
    #[serde(skip)]
    pub stale: bool,
}

impl MemoryEntry {
    /// The file text: YAML frontmatter (Obsidian properties), then the body.
    pub fn to_markdown(&self) -> String {
        frontmatter::render(self)
    }

    /// Parse an entry file: the YAML frontmatter [`to_markdown`](Self::to_markdown)
    /// writes, a hand-edited one, or the first version's `key: <JSON>` lines.
    pub fn from_markdown(text: &str) -> Result<Self, String> {
        frontmatter::parse(text)
    }
}

/// What the model proposes through `remember`: anchors are `(path, symbol)` without hashes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryDraft {
    /// An existing entry to update; None creates one.
    pub id: Option<String>,
    pub title: String,
    pub kind: MemoryKind,
    #[serde(default)]
    pub tags: Vec<String>,
    pub anchors: Vec<(PathBuf, Option<String>)>,
    pub body: String,
}

/// An entry to write and/or entry ids to delete.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryChange {
    pub upsert: Option<MemoryEntry>,
    pub delete: Vec<String>,
}

/// Whether `id` is safe as a file stem: lowercase ASCII letters, digits, `-`, `_`; 1-64 chars.
pub(crate) fn valid_id(id: &str) -> bool {
    (1..=64).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

/// Whether `id` is safe as an episode file stem: ASCII letters, digits, `-`, `_`; 1-128 chars.
pub(crate) fn valid_session_id(id: &str) -> bool {
    (1..=128).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// `title` as one safe line for the memory index and prompts.
pub fn sanitize_title(title: &str) -> String {
    let flat: String = title
        .chars()
        .map(|c| if is_unsafe(c) { ' ' } else { c })
        .collect();
    let flat = flat.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= MAX_TITLE_CHARS {
        return flat;
    }
    let mut out: String = flat.chars().take(MAX_TITLE_CHARS - 1).collect();
    out.push('…');
    out
}

/// Longest title [`sanitize_title`] keeps.
const MAX_TITLE_CHARS: usize = 120;

/// The characters tautly-context's `sanitize` refuses in prompt text (controls, the Unicode line
/// and paragraph separators, angle brackets). tautly-memory cannot depend on tautly-context, so the
/// rule is copied; unlike there, a title is cleaned rather than dropped, because an entry
/// must stay visible in the index.
fn is_unsafe(c: char) -> bool {
    c.is_control() || matches!(c, '\u{2028}' | '\u{2029}' | '<' | '>')
}

/// `path` as a clean relative path (no `.`, no `..`, not absolute), or why not.
pub(crate) fn normalize_rel(path: &Path) -> Result<PathBuf, &'static str> {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::Normal(p) => out.push(p),
            Component::CurDir => {}
            _ => return Err("the path must be relative to the project root and stay inside it"),
        }
    }
    if out.as_os_str().is_empty() {
        return Err("the path must be relative to the project root and stay inside it");
    }
    Ok(out)
}

pub(crate) fn check_id(id: &str) -> Result<(), MemoryError> {
    if valid_id(id) {
        Ok(())
    } else {
        Err(MemoryError::BadId(id.to_string()))
    }
}
