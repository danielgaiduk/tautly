//! The session log's record type and the helpers that keep a record serializable.
//!
//! Records are adjacently tagged (`{"type": "tool_result", "data": {...}}`), so a record
//! can wrap the internally tagged [`Command`] and [`ProviderEvent`] unchanged. A record
//! type this version does not know reads as [`SessionRecord::Unknown`].

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tautly_context::MutationReason;
use tautly_provider::ProviderEvent;
use tautly_session::BlobRef;
use tautly_tools::{PersistedEffect, ToolMeta};

use crate::evidence::TestEvidence;
use crate::protocol::{Command, FinishReason};

/// One line of a session log, in the order things happened.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    remote = "Self",
    tag = "type",
    content = "data",
    rename_all = "snake_case"
)]
pub enum SessionRecord {
    /// A command the engine received, as received.
    Command(Command),
    /// A provider request; the blob holds the serialized `CanonicalRequest`.
    Request { hash: BlobRef },
    /// What a completed provider response streamed. Consecutive text and reasoning deltas
    /// are merged into one event each; a response that broke off mid-stream (and was sent
    /// again) is not logged.
    Provider(ProviderEvent),
    /// A tool call of the response before it. `args` is the input as it stands in the
    /// history: an empty object when the model's JSON did not parse.
    ToolCall {
        id: String,
        name: String,
        args: Value,
    },
    /// The result returned to the model for call `id` (after the engine's own changes,
    /// e.g. a refused pin), with what the call did to files. Effects on paths that are not
    /// valid UTF-8 are left out (see [`utf8_effects`]).
    ToolResult {
        id: String,
        result_blob: BlobRef,
        is_error: bool,
        effects: Vec<PersistedEffect>,
        meta: ToolMeta,
    },
    /// A file a tool changed, with its content before the change (none: it was created).
    Edit {
        path: PathBuf,
        pre_image: Option<BlobRef>,
        turn: u32,
    },
    /// A compaction: each removed turn is a blob (a serialized context `Turn`).
    Compaction {
        removed: Vec<BlobRef>,
        summary: String,
    },
    /// A context collection: the elided results by call id and the turn of their stub
    /// (`[elided: superseded at turn N]`). The originals are the results' own blobs.
    GcApplied {
        elided: Vec<(String, u32)>,
        reason: MutationReason,
        tokens_freed: u32,
        forgotten_paths: Vec<PathBuf>,
    },
    /// The memory index text the stable prefix carries from here on.
    MemoryIndexApplied { text: String },
    /// The session pins moved into the pinned block here.
    PinsFolded,
    /// An engine bug (a panic in the turn task), with its message; a `TurnEnd{Error}`
    /// follows.
    EngineError { message: String },
    /// A user message's turn ended.
    TurnEnd {
        reason: FinishReason,
        /// The turn's last test run (spec 7.11.7), as `TurnFinished` carried it; kept here
        /// so a receipt on a resumed session can show it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        evidence: Option<TestEvidence>,
    },
    /// A record type this version does not know (read only; never written).
    #[serde(skip)]
    Unknown,
}

/// The `type` of every record this version writes.
const KNOWN_TYPES: &[&str] = &[
    "command",
    "request",
    "provider",
    "tool_call",
    "tool_result",
    "edit",
    "compaction",
    "gc_applied",
    "memory_index_applied",
    "pins_folded",
    "engine_error",
    "turn_end",
];

impl Serialize for SessionRecord {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        SessionRecord::serialize(self, s)
    }
}

impl<'de> Deserialize<'de> for SessionRecord {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(d)?;
        let known = value
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|t| KNOWN_TYPES.contains(&t));
        if !known {
            return Ok(SessionRecord::Unknown);
        }
        SessionRecord::deserialize(value).map_err(serde::de::Error::custom)
    }
}

/// Whether `path` can be stored in a record: serde writes paths as strings, so a path
/// that is not valid UTF-8 (possible on Linux) cannot be.
pub fn storable(path: &Path) -> bool {
    path.to_str().is_some()
}

/// The path a persisted effect names.
fn effect_path(effect: &PersistedEffect) -> Option<&Path> {
    match effect {
        PersistedEffect::Read { path, .. }
        | PersistedEffect::Edited { path, .. }
        | PersistedEffect::Deleted { path, .. } => Some(path),
        PersistedEffect::Exec { .. } => None,
    }
}

/// Split `effects` into those a record can hold and the paths of those it cannot.
pub fn utf8_effects(effects: Vec<PersistedEffect>) -> (Vec<PersistedEffect>, Vec<PathBuf>) {
    let mut kept = Vec::with_capacity(effects.len());
    let mut skipped = Vec::new();
    for e in effects {
        match effect_path(&e) {
            Some(p) if !storable(p) => skipped.push(p.to_path_buf()),
            _ => kept.push(e),
        }
    }
    (kept, skipped)
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    use super::*;

    #[test]
    fn effects_on_non_utf8_paths_are_split_off() {
        let bad = PathBuf::from(OsStr::from_bytes(b"/r/caf\xe9.txt"));
        let effects = vec![
            PersistedEffect::Deleted {
                path: bad.clone(),
                pre_image_blob: "x".into(),
                pre_mode: 0o644,
            },
            PersistedEffect::Exec { may_write: true },
            PersistedEffect::Deleted {
                path: "/r/ok.txt".into(),
                pre_image_blob: "y".into(),
                pre_mode: 0o644,
            },
        ];
        let (kept, skipped) = utf8_effects(effects);
        assert_eq!(skipped, vec![bad]);
        assert_eq!(kept.len(), 2);
        let rec = SessionRecord::ToolResult {
            id: "c".into(),
            result_blob: BlobRef::parse(&"a".repeat(64)).unwrap(),
            is_error: false,
            effects: kept,
            meta: ToolMeta::default(),
        };
        // What was kept serializes.
        serde_json::to_string(&rec).unwrap();
    }

    #[test]
    fn records_round_trip_and_unknown_types_are_tolerated() {
        let recs = vec![
            SessionRecord::Command(Command::SendMessage { text: "hi".into() }),
            SessionRecord::Provider(ProviderEvent::TextDelta("t".into())),
            SessionRecord::GcApplied {
                elided: vec![("c1".into(), 3)],
                reason: MutationReason::ColdCache,
                tokens_freed: 10,
                forgotten_paths: vec!["/r/a".into()],
            },
            SessionRecord::PinsFolded,
            SessionRecord::TurnEnd {
                reason: FinishReason::Completed,
                evidence: None,
            },
        ];
        for r in recs {
            let text = serde_json::to_string(&r).unwrap();
            assert_eq!(serde_json::from_str::<SessionRecord>(&text).unwrap(), r);
        }
        let future: SessionRecord =
            serde_json::from_str(r#"{"type":"from_the_future","data":{"x":1}}"#).unwrap();
        assert_eq!(future, SessionRecord::Unknown);
        // A known type with a bad payload is still an error.
        assert!(serde_json::from_str::<SessionRecord>(r#"{"type":"request","data":7}"#).is_err());
    }

    #[test]
    fn every_record_type_is_known() {
        let blob = BlobRef::parse(&"a".repeat(64)).unwrap();
        let all = [
            SessionRecord::Command(Command::Cancel),
            SessionRecord::Request { hash: blob.clone() },
            SessionRecord::Provider(ProviderEvent::TextDelta(String::new())),
            SessionRecord::ToolCall {
                id: String::new(),
                name: String::new(),
                args: Value::Null,
            },
            SessionRecord::ToolResult {
                id: String::new(),
                result_blob: blob.clone(),
                is_error: false,
                effects: vec![],
                meta: ToolMeta::default(),
            },
            SessionRecord::Edit {
                path: PathBuf::new(),
                pre_image: None,
                turn: 1,
            },
            SessionRecord::Compaction {
                removed: vec![blob],
                summary: String::new(),
            },
            SessionRecord::GcApplied {
                elided: vec![],
                reason: MutationReason::Manual,
                tokens_freed: 0,
                forgotten_paths: vec![],
            },
            SessionRecord::MemoryIndexApplied {
                text: String::new(),
            },
            SessionRecord::PinsFolded,
            SessionRecord::EngineError {
                message: String::new(),
            },
            SessionRecord::TurnEnd {
                reason: FinishReason::Error,
                evidence: None,
            },
        ];
        assert_eq!(all.len(), KNOWN_TYPES.len());
        for r in all {
            let v = serde_json::to_value(&r).unwrap();
            assert!(KNOWN_TYPES.contains(&v["type"].as_str().unwrap()), "{v}");
            assert_eq!(serde_json::from_value::<SessionRecord>(v).unwrap(), r);
        }
    }
}
