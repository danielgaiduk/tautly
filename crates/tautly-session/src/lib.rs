//! Append-only JSONL session store.
//!
//! Layout under the data home (see [`paths::data_home`]):
//!
//! ```text
//! <data_home>/sessions/<project_id>/<session-id>.jsonl
//! <data_home>/sessions/<project_id>/blobs/<first2>/<rest>
//! ```
//!
//! `<project_id>` ([`paths::project_id`]) is shared by every worktree of a git repository,
//! so their sessions are listed together and share blobs.
//!
//! Each log line is one JSON object. The first is the header
//! (`{"type":"header", ...}`, carrying `schema_version`); every following
//! line is `{"type":"record","offset":<n>,"t":<unix ms>,"data":<R>}` where `offset` is
//! the record's 0-based index and `t` when it was appended, in Unix milliseconds,
//! non-decreasing within a log (clamped when the clock steps back; a fork's copied
//! records keep their original `t`, see [`SessionWriter::copy_from`]). The store is generic over the
//! record type `R`; paths inside records are [`RelPath`]s relative to the header's root.
//! Lines whose `type` is unknown are skipped by readers, so newer writers can
//! add record kinds without breaking older readers.
//!
//! Secrets: nothing in this crate redacts. Callers must not put raw API keys
//! into records or blobs.

mod blobs;
mod error;
mod git;
mod reader;
mod record;
mod relpath;
mod writer;

pub mod paths;

pub use blobs::BlobRef;
pub use error::{Error, Result};
pub use git::{GitDirs, git_dirs_at, git_head};
pub use reader::{SessionReader, SessionSummary, list, list_here, list_here_in, list_in};
pub use record::{Entry, GitState, Header, HeaderInit, Origin, SCHEMA_VERSION};
pub use relpath::{ConfineError, RelPath};
pub use writer::{SessionWriter, new_session_id};
