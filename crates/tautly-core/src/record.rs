//! The session log's record type.
//!
//! Records are adjacently tagged (`{"type": "tool_result", "data": {...}}`), so a record
//! can wrap the internally tagged [`Command`] and [`ProviderEvent`] unchanged. A record
//! type this version does not know reads as [`SessionRecord::Unknown`].
//!
//! Paths in records are [`RelPath`]s, relative to the header's root (spec 6.5 H2); a path
//! that cannot be stored that way (outside the root, or not valid UTF-8) is left out of the
//! log with a warning.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tautly_context::MutationReason;
use tautly_provider::{ModelId, ProviderEvent};
use tautly_session::{BlobRef, RelPath, SessionReader};
use tautly_tools::{PersistedEffect, ToolMeta};

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
    /// The inputs of the stable prefix the following requests are built from (H5): written
    /// before the first request and again before a request whose inputs differ. The blobs
    /// hold the AGENTS.md text and the repo map as the context holds them (none: empty);
    /// the hashes are blake3 hex of the system prompt text and of the serialized tools.
    PrefixApplied {
        agents_md: Option<BlobRef>,
        repo_map: Option<BlobRef>,
        system_prompt_hash: String,
        tools_hash: String,
    },
    /// A provider request (H6): `hash` is the blake3 of the serialized `CanonicalRequest`
    /// (which is not stored), `turn` the user turn (1-based), `step` the absolute provider
    /// step, and `blocks` the hash of each block. A step is numbered by the context turn
    /// it builds (`Context::next_turn`, the counter `Edit.turn` and the read tracker use).
    /// Each user message is a context turn too, so steps increase but are not dense: the
    /// first message's steps are 2, 3, ..., the next message's start one after its turn.
    Request {
        hash: BlobRef,
        turn: u32,
        step: u32,
        model: ModelId,
        blocks: BlockHashes,
    },
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
    /// e.g. a refused pin), with what the call did to files. Effects on paths that cannot
    /// be stored are left out.
    ToolResult {
        id: String,
        result_blob: BlobRef,
        is_error: bool,
        effects: Vec<PersistedEffect>,
        meta: ToolMeta,
    },
    /// A file a tool changed, with its content before the change (none: it was created)
    /// and after it (H7; none: deleted, or the content could not be captured). `turn` is
    /// the step.
    Edit {
        path: RelPath,
        pre_image: Option<BlobRef>,
        post_image_blob: Option<BlobRef>,
        turn: u32,
    },
    /// An undo of user turn `turn` (H7, written by Task 26): each file's content just
    /// before the undo (`from_blob`; none: it did not exist) and what it was set to (`to`;
    /// none: deleted), so the undone state can be restored.
    Undone {
        turn: u32,
        files: Vec<UndoneFile>,
        backup_ref: Option<String>,
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
        forgotten_paths: Vec<RelPath>,
    },
    /// The memory index text the stable prefix carries from here on.
    MemoryIndexApplied { text: String },
    /// The session pins moved into the pinned block here.
    PinsFolded,
    /// A question to the user (H8), logged before its event is shown.
    ApprovalRequested {
        request_id: String,
        kind: ApprovalKind,
        /// The tool call it is about (tool approvals).
        call_id: Option<String>,
        reason: String,
    },
    /// A git checkpoint of the work tree (written by Task 47): `n` is its number in the
    /// session, `turn` the user turn and `step` the provider step it was taken at.
    Checkpoint {
        n: u32,
        commit: String,
        tree: String,
        head: Option<String>,
        dirty: bool,
        turn: u32,
        step: u32,
    },
    /// A checkpoint that was not taken, and why (written by Task 47).
    CheckpointSkipped {
        reason: String,
        turn: u32,
        step: u32,
    },
    /// The session is renamed; the last one wins over the header's label.
    Labeled { label: String },
    /// An engine bug (a panic in the turn task), with its message; a `TurnEnd{Error}`
    /// follows.
    EngineError { message: String },
    /// A user message's turn ended.
    TurnEnd { reason: FinishReason },
    /// A record type this version does not know (read only; never written).
    #[serde(skip)]
    Unknown,
}

/// blake3 hex of each part of a request (H6): `stable` the system blocks and tools,
/// `pinned` the pinned block, `history` the conversation, `tail` the volatile state
/// message. Equal hashes mean equal parts; they locate where two requests differ.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockHashes {
    pub stable: String,
    pub pinned: String,
    pub history: String,
    pub tail: String,
}

/// One file of an [`SessionRecord::Undone`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UndoneFile {
    pub path: RelPath,
    pub from_blob: Option<BlobRef>,
    pub to: Option<BlobRef>,
}

/// What an [`SessionRecord::ApprovalRequested`] asks about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalKind {
    Tool,
    McpServer,
    Memory,
}

/// The `type` of every record this version writes.
const KNOWN_TYPES: &[&str] = &[
    "command",
    "prefix_applied",
    "request",
    "provider",
    "tool_call",
    "tool_result",
    "edit",
    "undone",
    "compaction",
    "gc_applied",
    "memory_index_applied",
    "pins_folded",
    "approval_requested",
    "checkpoint",
    "checkpoint_skipped",
    "labeled",
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

/// The session's name: the last `Labeled` record's, else the header's label.
pub fn session_label(
    reader: &SessionReader<SessionRecord>,
) -> tautly_session::Result<Option<String>> {
    let mut label = reader.header().label.clone();
    for rec in reader.records() {
        if let (_, SessionRecord::Labeled { label: l }) = rec? {
            label = Some(l);
        }
    }
    Ok(label)
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    use std::path::{Path, PathBuf};

    use tautly_tools::AppliedEffect;

    use super::*;

    #[test]
    fn effects_on_paths_a_record_cannot_hold_are_not_persisted() {
        let root = Path::new("/r");
        let bad = PathBuf::from(OsStr::from_bytes(b"/r/caf\xe9.txt"));
        let put = &mut |_: &[u8]| Ok::<_, ()>("x".to_string());
        for path in [bad, PathBuf::from("/elsewhere/a.txt")] {
            let e = AppliedEffect::Deleted {
                path,
                pre_image: b"x".to_vec(),
                pre_mode: 0o644,
            };
            assert_eq!(PersistedEffect::from_applied(&e, root, put), Ok(None));
        }
        let ok = AppliedEffect::Deleted {
            path: "/r/ok.txt".into(),
            pre_image: b"y".to_vec(),
            pre_mode: 0o644,
        };
        let kept = PersistedEffect::from_applied(&ok, root, put)
            .unwrap()
            .unwrap();
        let rec = SessionRecord::ToolResult {
            id: "c".into(),
            result_blob: BlobRef::parse(&"a".repeat(64)).unwrap(),
            is_error: false,
            effects: vec![kept, PersistedEffect::Exec { may_write: true }],
            meta: ToolMeta::default(),
        };
        let v = serde_json::to_value(&rec).unwrap();
        assert_eq!(v["data"]["effects"][0]["path"], "ok.txt");
    }

    fn rel(s: &str) -> RelPath {
        RelPath::from_abs(Path::new("/r"), &Path::new("/r").join(s)).unwrap()
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
                forgotten_paths: vec![rel("a")],
            },
            SessionRecord::PinsFolded,
            SessionRecord::TurnEnd {
                reason: FinishReason::Completed,
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
            SessionRecord::PrefixApplied {
                agents_md: Some(blob.clone()),
                repo_map: None,
                system_prompt_hash: "s".into(),
                tools_hash: "t".into(),
            },
            SessionRecord::Request {
                hash: blob.clone(),
                turn: 1,
                step: 2,
                model: ModelId::from("mock/m"),
                blocks: BlockHashes {
                    stable: "a".into(),
                    pinned: "b".into(),
                    history: "c".into(),
                    tail: "d".into(),
                },
            },
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
                path: rel("a"),
                pre_image: None,
                post_image_blob: Some(blob.clone()),
                turn: 1,
            },
            SessionRecord::Undone {
                turn: 1,
                files: vec![UndoneFile {
                    path: rel("a"),
                    from_blob: Some(blob.clone()),
                    to: None,
                }],
                backup_ref: Some("refs/tautly/checkpoints/s/undo-1".into()),
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
            SessionRecord::ApprovalRequested {
                request_id: "approval-1".into(),
                kind: ApprovalKind::McpServer,
                call_id: None,
                reason: "r".into(),
            },
            SessionRecord::Checkpoint {
                n: 1,
                commit: "c".into(),
                tree: "t".into(),
                head: None,
                dirty: true,
                turn: 1,
                step: 0,
            },
            SessionRecord::CheckpointSkipped {
                reason: "timeout".into(),
                turn: 1,
                step: 3,
            },
            SessionRecord::Labeled {
                label: "try-a".into(),
            },
            SessionRecord::EngineError {
                message: String::new(),
            },
            SessionRecord::TurnEnd {
                reason: FinishReason::Error,
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
