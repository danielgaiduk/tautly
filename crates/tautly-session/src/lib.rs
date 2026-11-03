//! Append-only JSONL session store.
//!
//! Layout under the data home (see [`paths::data_home`]):
//!
//! ```text
//! <data_home>/sessions/<project_hash>/<session-id>.jsonl
//! <data_home>/sessions/<project_hash>/blobs/<first2>/<rest>
//! ```
//!
//! Each log line is one JSON object. The first is the header
//! (`{"type":"header", ...}`, carrying `schema_version`); every following
//! line is `{"type":"record","offset":<n>,"data":<R>}` where `offset` is the
//! record's 0-based index. The store is generic over the record type `R`.
//! Lines whose `type` is unknown are skipped by readers, so newer writers can
//! add record kinds without breaking older readers.
//!
//! Secrets: nothing in this crate redacts. Callers must not put raw API keys
//! into records or blobs.

mod blobs;
mod error;
mod reader;
mod record;
mod writer;

pub mod paths;

pub use blobs::BlobRef;
pub use error::{Error, Result};
pub use reader::{SessionReader, SessionSummary, list, list_in};
pub use record::{Header, SCHEMA_VERSION};
pub use writer::SessionWriter;
