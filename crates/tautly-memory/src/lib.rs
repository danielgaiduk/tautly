//! Code-anchored project memory, episodic memory and `recall` search (spec 7.4).
//!
//! Project memory is one markdown file per entry in `<project>/.tautly/memory/`, named
//! after its title (`<title-slug>.md`, `-2`, `-3`... on collision) and shareable via git.
//! The directory opens as an Obsidian vault: the frontmatter is YAML properties (the
//! stable `id` among them, so renaming a file breaks nothing), entries have `tags`, and
//! bodies link each other with `[[title]]` (written as `[[<file stem>|title]]`, which
//! Obsidian resolves), and `recall` follows links one hop. Episodes are private session
//! summaries in
//! `<data_home>/memory/<project_id>/episodes/<session-id>.md`. Every file is written
//! atomically (temp file + rename) with mode 0600 in directories of mode 0700.

mod consolidate;
mod entry;
mod episodes;
mod frontmatter;
mod search;
mod stale;
mod store;
mod vault;

use std::path::PathBuf;

pub use entry::{
    MAX_BODY_TOKENS, MemAnchor, MemoryChange, MemoryDraft, MemoryEntry, MemoryKind, sanitize_title,
};
pub use search::{Hit, HitSource};
pub use store::MemoryStore;
pub use vault::parse_links;

/// About four bytes per token, the rule a draft is checked with wherever it is counted
/// (the `remember` tool, the store, consolidation and the engine must agree).
pub fn approx_tokens(s: &str) -> u32 {
    s.len().div_ceil(4) as u32
}

#[derive(Debug, thiserror::Error)]
pub enum MemoryError {
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{} is a symlink or not a directory; Tautly will not read or write memory through it", .0.display())]
    UnsafeDir(PathBuf),
    #[error("no usable data directory: {0}")]
    DataHome(String),
    #[error("code index error: {0}")]
    Index(String),
    #[error("invalid id `{0}`: use lowercase letters, digits, `-` and `_` only")]
    BadId(String),
    #[error("no memory entry with id `{0}`")]
    UnknownId(String),
    #[error("the title is empty")]
    EmptyTitle,
    #[error("the body is empty")]
    EmptyBody,
    #[error("the body is about {tokens} tokens; the limit is {max}")]
    TooLong { tokens: u32, max: u32 },
    #[error("cannot anchor to {anchor}: {reason}")]
    BadAnchor { anchor: String, reason: String },
    #[error("invalid memory change: {0}")]
    InvalidChange(String),
    #[error("cannot read the consolidation reply: {0}")]
    BadConsolidation(String),
}
