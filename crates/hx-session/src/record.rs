use std::path::PathBuf;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

/// Current value of [`Header::schema_version`].
pub const SCHEMA_VERSION: u32 = 1;

/// First line of every session log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Header {
    pub schema_version: u32,
    pub session_id: String,
    pub created: SystemTime,
    pub root: PathBuf,
    /// For a fork: the parent session id and the parent record offset the
    /// fork branches from.
    pub parent: Option<(String, u64)>,
}

/// Borrowed line form used when writing.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum LineRef<'a, R> {
    Header(&'a Header),
    Record { offset: u64, data: &'a R },
}

/// Owned line form used when reading. Unknown line types map to `Unknown`.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum Line<R> {
    Header(Header),
    Record {
        offset: u64,
        data: R,
    },
    #[serde(other)]
    Unknown,
}
