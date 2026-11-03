use std::path::PathBuf;
use std::time::SystemTime;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::value::RawValue;

/// Current value of [`Header::schema_version`]. Still 1: the time-travel fields (record
/// timestamps, header fields) were added before schema 1 was released.
pub const SCHEMA_VERSION: u32 = 1;

/// First line of every session log.
///
/// Every field after `parent` is `#[serde(default)]`, so a header written without it
/// still reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Header {
    pub schema_version: u32,
    pub session_id: String,
    pub created: SystemTime,
    /// The canonical work tree root (for a worktree: the worktree, not the main checkout).
    /// Paths in records are relative to it.
    pub root: PathBuf,
    /// For a fork: the parent session id and the parent record offset the
    /// fork branches from.
    pub parent: Option<(String, u64)>,
    /// [`project_id`](crate::paths::project_id) of the root: shared by all worktrees of
    /// one repository.
    #[serde(default)]
    pub project: String,
    /// The branch name; a later `Labeled` record (written by the engine) renames it.
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub origin: Origin,
    /// Inside git: the `HEAD` commit at session start, read from the git files. `dirty`
    /// is unknown here (it needs git); the session-start checkpoint record has it.
    #[serde(default)]
    pub git: Option<GitState>,
    /// The Tautly version that wrote the log.
    #[serde(default)]
    pub tautly_version: String,
    /// The model the session started with.
    #[serde(default)]
    pub model: Option<String>,
    /// blake3 of the effective configuration.
    #[serde(default)]
    pub config_hash: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
}

/// Why a session exists.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    /// Started by the user.
    #[default]
    User,
    /// A conversation-only branch (`Fork`).
    Fork,
    /// A branch made by rewinding files and conversation.
    Rewind,
    /// A fork at a step into an isolated worktree.
    WhatIf,
    /// Run by the eval runner.
    Eval,
    /// Imported from a bundle.
    Import,
    /// An origin this version does not know.
    #[serde(other)]
    Unknown,
}

/// The repository state a header records.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitState {
    pub head: Option<String>,
    pub dirty: Option<bool>,
}

/// What the creator of a session decides about its header; the writer fills in the rest
/// (id, time, root, project, git state, version).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HeaderInit {
    /// For a fork: (parent session id, parent record offset).
    pub parent: Option<(String, u64)>,
    pub label: Option<String>,
    pub origin: Origin,
    pub model: Option<String>,
    pub config_hash: Option<String>,
    pub tags: Vec<String>,
}

/// One record as read back, with where and when it was written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry<R> {
    /// 0-based position in the log.
    pub offset: u64,
    /// When it was appended, in Unix milliseconds; 0 when the line has no `t`.
    pub t_ms: u64,
    pub rec: R,
}

/// Borrowed line form used when writing.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum LineRef<'a, R> {
    Header(&'a Header),
    Record { offset: u64, t: u64, data: &'a R },
}

/// A parsed log line. Record payloads stay raw so the caller can deserialize
/// them as its own `R` and tell "structurally torn" from "not an `R`".
pub(crate) enum Parsed {
    Header(Box<Header>),
    Record {
        offset: u64,
        t_ms: u64,
        data: Box<RawValue>,
    },
    /// A line type this version does not know; skipped.
    Unknown,
}

#[derive(Deserialize)]
struct Envelope {
    #[serde(rename = "type")]
    ty: String,
    offset: Option<u64>,
    #[serde(default)]
    t: Option<u64>,
    #[serde(default, deserialize_with = "some_raw")]
    data: Option<Box<RawValue>>,
}

/// Distinguishes an explicit `null` payload from a missing one.
fn some_raw<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Box<RawValue>>, D::Error> {
    Box::<RawValue>::deserialize(d).map(Some)
}

/// Structural parse of one line (trailing newline allowed). Failure means the
/// line is not a complete, well-formed log line: valid JSON object with a
/// `type`, a well-formed header, or a record carrying `offset` and `data`.
pub(crate) fn parse_line(bytes: &[u8]) -> Result<Parsed, serde_json::Error> {
    let env: Envelope = serde_json::from_slice(bytes)?;
    match env.ty.as_str() {
        "header" => Ok(Parsed::Header(serde_json::from_slice(bytes)?)),
        "record" => match (env.offset, env.data) {
            (Some(offset), Some(data)) => Ok(Parsed::Record {
                offset,
                t_ms: env.t.unwrap_or(0),
                data,
            }),
            _ => Err(serde::de::Error::custom("record line lacks offset or data")),
        },
        _ => Ok(Parsed::Unknown),
    }
}
