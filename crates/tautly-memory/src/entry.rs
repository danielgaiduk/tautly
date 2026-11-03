//! Entry types and the markdown file format.

use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::MemoryError;

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
    pub anchors: Vec<MemAnchor>,
    pub created: SystemTime,
    pub verified: Option<SystemTime>,
    pub body: String,
    /// An anchor's code changed since the entry was written. Never stored in the file or in
    /// a serialized proposal; it reads back `false`.
    #[serde(skip)]
    pub stale: bool,
}

impl MemoryEntry {
    /// The file text: YAML frontmatter, then the body.
    pub fn to_markdown(&self) -> String {
        // Every value is a JSON scalar or flow sequence, which is valid YAML, so quoting
        // and escaping never need a YAML library.
        let json = |v: &str| serde_json::to_string(v).unwrap_or_default();
        let anchors: Vec<FileAnchor> = self
            .anchors
            .iter()
            .map(|a| FileAnchor {
                path: a.path.to_string_lossy().into_owned(),
                symbol: a.symbol.clone(),
                hash: a.hash.clone(),
            })
            .collect();
        let mut out = String::from("---\n");
        out.push_str(&format!("id: {}\n", json(&self.id)));
        out.push_str(&format!("title: {}\n", json(&self.title)));
        out.push_str(&format!("kind: {}\n", json(self.kind.as_str())));
        out.push_str(&format!(
            "anchors: {}\n",
            serde_json::to_string(&anchors).unwrap_or_else(|_| "[]".into())
        ));
        out.push_str(&format!("created: {}\n", json(&format_time(self.created))));
        if let Some(v) = self.verified {
            out.push_str(&format!("verified: {}\n", json(&format_time(v))));
        }
        out.push_str("---\n");
        out.push_str(&self.body);
        out.push('\n');
        out
    }

    /// Parse a file written by [`to_markdown`](Self::to_markdown) (or by hand).
    pub fn from_markdown(text: &str) -> Result<Self, String> {
        let rest = text
            .strip_prefix("---\n")
            .ok_or("missing `---` frontmatter")?;
        let (front, body) = split_frontmatter(rest).ok_or("unterminated frontmatter")?;
        let (mut id, mut title, mut kind, mut created) = (None, None, None, None);
        let (mut verified, mut anchors) = (None, Vec::new());
        for line in front.lines().filter(|l| !l.trim().is_empty()) {
            let (key, value) = line
                .split_once(':')
                .ok_or_else(|| format!("bad frontmatter line `{line}`"))?;
            match key.trim() {
                "id" => id = Some(scalar(value)?),
                "title" => title = Some(scalar(value)?),
                "kind" => {
                    let k = scalar(value)?;
                    kind =
                        Some(MemoryKind::parse(&k).ok_or_else(|| format!("unknown kind `{k}`"))?);
                }
                "created" => created = Some(parse_time(&scalar(value)?)?),
                "verified" => verified = Some(parse_time(&scalar(value)?)?),
                "anchors" => anchors = parse_anchors(value)?,
                _ => {} // Unknown keys are kept out of the entry but not an error.
            }
        }
        let id = id.ok_or("missing `id`")?;
        if !valid_id(&id) {
            return Err(format!("invalid id `{id}`"));
        }
        Ok(MemoryEntry {
            id,
            title: title.ok_or("missing `title`")?,
            kind: kind.ok_or("missing `kind`")?,
            anchors,
            created: created.ok_or("missing `created`")?,
            verified,
            body: body.strip_suffix('\n').unwrap_or(body).to_string(),
            stale: false,
        })
    }
}

/// What the model proposes through `remember`: anchors are `(path, symbol)` without hashes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryDraft {
    /// An existing entry to update; None creates one.
    pub id: Option<String>,
    pub title: String,
    pub kind: MemoryKind,
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

#[derive(Serialize, Deserialize)]
struct FileAnchor {
    path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    symbol: Option<String>,
    hash: String,
}

fn format_time(t: SystemTime) -> String {
    humantime::format_rfc3339_seconds(t).to_string()
}

fn parse_time(s: &str) -> Result<SystemTime, String> {
    humantime::parse_rfc3339(s).map_err(|e| format!("bad timestamp `{s}`: {e}"))
}

/// The text before the closing `---` line, and the text after it.
fn split_frontmatter(rest: &str) -> Option<(&str, &str)> {
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim_end() == "---" {
            return Some((&rest[..offset], &rest[offset + line.len()..]));
        }
        offset += line.len();
    }
    None
}

/// A YAML scalar: double-quoted (JSON escapes), single-quoted, or plain.
fn scalar(raw: &str) -> Result<String, String> {
    let raw = raw.trim();
    if raw.starts_with('"') {
        return serde_json::from_str(raw).map_err(|e| format!("bad string {raw}: {e}"));
    }
    if raw.len() >= 2 && raw.starts_with('\'') && raw.ends_with('\'') {
        return Ok(raw[1..raw.len() - 1].replace("''", "'"));
    }
    Ok(raw.to_string())
}

fn parse_anchors(raw: &str) -> Result<Vec<MemAnchor>, String> {
    let list: Vec<FileAnchor> =
        serde_json::from_str(raw.trim()).map_err(|e| format!("bad anchors: {e}"))?;
    list.into_iter()
        .map(|a| {
            let path = normalize_rel(Path::new(&a.path))
                .map_err(|why| format!("bad anchor path `{}`: {why}", a.path))?;
            Ok(MemAnchor {
                path,
                symbol: a.symbol,
                hash: a.hash,
            })
        })
        .collect()
}

pub(crate) fn check_id(id: &str) -> Result<(), MemoryError> {
    if valid_id(id) {
        Ok(())
    } else {
        Err(MemoryError::BadId(id.to_string()))
    }
}
