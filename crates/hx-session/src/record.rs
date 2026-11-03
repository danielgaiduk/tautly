use std::path::PathBuf;
use std::time::SystemTime;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::value::RawValue;

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

/// A parsed log line. Record payloads stay raw so the caller can deserialize
/// them as its own `R` and tell "structurally torn" from "not an `R`".
pub(crate) enum Parsed {
    Header(Header),
    Record {
        offset: u64,
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
            (Some(offset), Some(data)) => Ok(Parsed::Record { offset, data }),
            _ => Err(serde::de::Error::custom("record line lacks offset or data")),
        },
        _ => Ok(Parsed::Unknown),
    }
}
