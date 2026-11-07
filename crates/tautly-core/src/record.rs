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
use tautly_context::pinned::{PinSource, Pinned};
use tautly_memory::MemoryChange;
use tautly_provider::{ModelId, ProviderEvent, RawUsage};
use tautly_session::{BlobRef, RelPath, SessionReader};
use tautly_tools::{PersistedEffect, ToolMeta};

use crate::evidence::TestEvidence;
use crate::protocol::{Command, FinishReason, UsageRole};

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
        /// The tool specs as sent (the serialized `tools` of the request, so the blob's
        /// hash is `tools_hash`), MCP tools included: a rebuild assembles each logged step
        /// with the tools in force at it. None in logs written before this field.
        #[serde(default)]
        tools: Option<BlobRef>,
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
    /// Usage no logged response carries, as charged (ledger, budgets, `Usage`): a
    /// side-model request (a compaction summary, a consolidation, an episode, a
    /// `web_fetch` answer), or the main model's usage for a response that reported none,
    /// broke off or was cancelled (estimated, or as reported), and what the retry wrapper
    /// discarded for it. With the `Provider(Usage)` and `Provider(DiscardedUsage)` events
    /// of complete responses (spent on their step's `Request.model`) it is the session's
    /// whole spend, so resume and receipts can price it without the cost ledger.
    Charged {
        role: UsageRole,
        model: ModelId,
        usage: RawUsage,
        estimated: bool,
    },
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
        /// The call ran (it was not refused, denied, blocked or cancelled first): the
        /// attempt ledger counts it, and resume replays it there.
        #[serde(default)]
        ran: bool,
        /// The loop breaker refused to repeat the call.
        #[serde(default)]
        blocked: bool,
    },
    /// A file a tool changed, with its content before the change (none: it was created)
    /// and after it (H7; none: deleted, or the content could not be captured), at provider
    /// step `step` (not a user turn: map steps to user turns with the `Request` records).
    Edit {
        path: RelPath,
        pre_image: Option<BlobRef>,
        post_image_blob: Option<BlobRef>,
        #[serde(alias = "turn")]
        step: u32,
        /// Not a tool's change: a rewind's branch (outside git) asserting the content
        /// it continues from (`pre_image` and `post_image_blob` the same, spec 7.12).
        /// Turn diffs, undo and receipts leave it out.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        asserted: bool,
    },
    /// Writes whose outcome is unknown: abandoned at their bound after a cancel, or found
    /// changed later (the abandoned write landed). The files may hold anything; the write
    /// generation moves, as it did live (a rebuild replays it into the attempt ledger).
    /// Logged where it was noticed: inside the step whose write was abandoned, or (`late`)
    /// at the start of a later step, which a cut before that step leaves out.
    WritesLost {
        paths: Vec<RelPath>,
        #[serde(default)]
        late: bool,
    },
    /// An undo of user turn `turn` (H7, written by Task 26): each file's content just
    /// before the undo (`from_blob`; none: it did not exist) and what it was set to (`to`;
    /// none: deleted), so the undone state can be restored.
    Undone {
        turn: u32,
        files: Vec<UndoneFile>,
        backup_ref: Option<String>,
    },
    /// An `Uncompact` that changed the context: the latest compaction was undone
    /// (`compaction`), and the elisions of these results (call ids) were restored, their
    /// outputs read back from their `ToolResult` blobs.
    Uncompacted {
        compaction: bool,
        restored: Vec<String>,
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
    /// A memory change shown to the user (`MemoryProposed`), logged before its event; it
    /// stays pending (and is shown again on resume) until a `MemoryDecision` names it.
    MemoryProposal {
        id: String,
        #[serde(with = "crate::protocol::memory_change_wire")]
        change: MemoryChange,
    },
    /// The proposal `id` was approved (its change was written to the store) or rejected.
    MemoryDecision { id: String, approved: bool },
    /// The session pins moved into the pinned block here.
    PinsFolded,
    /// The pins the session started with (configuration, AGENTS.md / CLAUDE.md sections),
    /// logged once at session start: a resumed session's pinned block is the logged one,
    /// never re-read from disk.
    StartPins { pins: Vec<LoggedPin> },
    /// A question to the user (H8), logged before its event is shown.
    ApprovalRequested {
        request_id: String,
        kind: ApprovalKind,
        /// The tool call it is about (tool approvals).
        call_id: Option<String>,
        reason: String,
    },
    /// A git checkpoint of the work tree (written by Task 47): `n` is its number in the
    /// session, `turn` the user turn and `step` the provider step it was taken at, `point`
    /// where (a `BeforeExec` one sits inside its step; spec 7.12.1 finds the files at a
    /// step boundary by log offset, not by `step`).
    Checkpoint {
        n: u32,
        commit: String,
        tree: String,
        head: Option<String>,
        dirty: bool,
        turn: u32,
        step: u32,
        /// None in logs written before this field.
        #[serde(default)]
        point: Option<CheckpointPoint>,
    },
    /// A checkpoint that was not taken, and why (written by Task 47), at `point`.
    CheckpointSkipped {
        reason: String,
        turn: u32,
        step: u32,
        /// None in logs written before this field.
        #[serde(default)]
        point: Option<CheckpointPoint>,
    },
    /// The session is renamed; the last one wins over the header's label.
    Labeled { label: String },
    /// A rewind or switch is about to write files (spec 7.12.2): the backup snapshot's
    /// ref (with git checkpoints), and outside git each path's content before (`files`),
    /// so a write cut off by a crash can be put back. A resume warns when no
    /// `RestoreEnded` follows it.
    RestoreBegun {
        backup_ref: Option<String>,
        #[serde(default)]
        files: Vec<BackupFile>,
        /// Every path it is about to write (the snapshot holds the rest of the tree too).
        #[serde(default)]
        paths: Vec<RelPath>,
        /// What wrote it. One `tautly sessions restore-backup` wrote (what it replaced) is
        /// put back by a later `restore-backup` only when asked (`--include-own`, Task 49
        /// M-R3b). Logs of the builds before this field (`restore_backup: bool`) read as
        /// `restore_backup` or `unknown`.
        #[serde(default)]
        by: RestoreBy,
    },
    /// The end of the restore a `RestoreBegun` started.
    RestoreEnded {
        #[serde(default)]
        outcome: RestoreOutcome,
    },
    /// The session switched to `model` (`Command::SetModel`, spec 7.11.5); requests from
    /// here on go to it. The last one wins over the header's model.
    ModelSet { model: ModelId },
    /// Every reasoning item (`Reasoning`, `RedactedReasoning`) was removed from the history
    /// here: the first request after a switch within one provider was refused for the
    /// reasoning carried over from the previous model, and is sent again without it. A
    /// rebuild must apply it at this position (`Context::strip_reasoning(None)`), or the
    /// resumed session sends the refused reasoning again.
    ReasoningStripped,
    /// An engine bug (a panic in the turn task), with its message; a `TurnEnd{Error}`
    /// follows.
    EngineError { message: String },
    /// User turn `turn` (1-based) started with the message `text`. A message sent while
    /// another turn runs is logged as a command when it arrives, and its turn starts only
    /// after that turn's `TurnEnd`; rebuild pushes the user turn here.
    TurnStart { turn: u32, text: String },
    /// The first record a fork writes itself: every record before it was copied from the
    /// parent session (the header's `parent`). The usage logged before it is the parent's
    /// spend, not the branch's.
    Forked,
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

/// Where in the session a [`SessionRecord::Checkpoint`] (or `CheckpointSkipped`) was
/// taken (spec 7.11.11).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointPoint {
    /// A new session's start: the files as the session found them.
    SessionStart,
    /// A resumed session's start: the files now, including whatever changed while no
    /// session ran.
    ResumeStart,
    /// Before a command (`bash`, a stdio MCP call) of step `step`, after that step's
    /// earlier runs.
    BeforeExec,
    /// At the end of a user turn whose last step is `step`.
    TurnEnd,
    /// Before a rewind or branch switch writes (spec 7.12; written by Task 49): the tip as
    /// it was.
    BeforeRestore,
    /// At a user turn's start, before anything else, when the previous turn's end
    /// checkpoint was skipped and could not be taken after it either (Task 49 N1).
    TurnStart,
    /// The files a rewind or switch materialized, as the branch continues from them
    /// (spec 7.12; written by Task 49): exactly `state_at` of the step it branched at, or
    /// the tip switched to. Older builds read it as `Unknown`.
    Restored,
    /// A point this version does not know (read only).
    #[serde(other)]
    Unknown,
}

/// What a [`SessionRecord::RestoreBegun`] was written by.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestoreBy {
    Rewind,
    Switch,
    /// `tautly sessions restore-backup`.
    RestoreBackup,
    /// Not known: written before the field (a rewind or a switch), or by a newer version.
    #[default]
    #[serde(other)]
    Unknown,
}

/// How a restore ended ([`SessionRecord::RestoreEnded`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestoreOutcome {
    /// The files are now the target's.
    Applied,
    /// It failed, and the files written were put back.
    RolledBack,
    /// It failed, and the files written were not (all) put back.
    Failed,
    /// An outcome this version does not know (read only).
    #[default]
    #[serde(other)]
    Unknown,
}

/// One file of a [`SessionRecord::RestoreBegun`]: its content before the restore wrote
/// it (`from_*`) and what the restore wrote (`to*`), named as [`UndoneFile`]'s are. Logs of
/// the builds before the rename (`blob`, `mode`, `wrote`, `wrote_mode`) still read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupFile {
    pub path: RelPath,
    /// It existed (with `from_blob` none: it could not be kept, a link or over 64 MiB).
    pub existed: bool,
    #[serde(alias = "blob")]
    pub from_blob: Option<BlobRef>,
    #[serde(default, alias = "mode")]
    pub from_mode: Option<u32>,
    /// What the restore wrote there (none: not known), so putting the backup back can
    /// tell a later human edit (Task 49 N10).
    #[serde(default, alias = "wrote")]
    pub to: Option<Wrote>,
    /// The permission bits the restore wrote there (none: not known), so a later
    /// mode-only change is told apart too (Task 49 M-R3c).
    #[serde(default, alias = "wrote_mode")]
    pub to_mode: Option<u32>,
}

/// What a restore wrote at a path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Wrote {
    /// This content.
    Blob(BlobRef),
    /// It deleted the file.
    Absent,
    /// A form this version does not know (read only): treated as not known.
    Unknown,
}

impl<'de> Deserialize<'de> for Wrote {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "snake_case")]
        enum Known {
            Blob(BlobRef),
            Absent,
        }
        Ok(match serde_json::from_value(Value::deserialize(d)?) {
            Ok(Known::Blob(b)) => Wrote::Blob(b),
            Ok(Known::Absent) => Wrote::Absent,
            Err(_) => Wrote::Unknown,
        })
    }
}

/// One file of an [`SessionRecord::Undone`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UndoneFile {
    pub path: RelPath,
    pub from_blob: Option<BlobRef>,
    /// The file's mode before the undo (none: it did not exist).
    #[serde(default)]
    pub from_mode: Option<u32>,
    pub to: Option<BlobRef>,
    /// The mode it was set to (none: deleted).
    #[serde(default)]
    pub to_mode: Option<u32>,
}

/// One pin of a [`SessionRecord::StartPins`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoggedPin {
    pub text: String,
    pub source: PinOrigin,
}

/// Who pinned a [`LoggedPin`] (the context's `PinSource`, in log form).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PinOrigin {
    User,
    Config,
    Model,
    AgentsMd,
    ClaudeMd,
    /// A source this version does not know (read back as a configuration pin).
    #[serde(other)]
    Unknown,
}

impl From<&Pinned> for LoggedPin {
    fn from(p: &Pinned) -> Self {
        LoggedPin {
            text: p.text.clone(),
            source: match p.source {
                PinSource::User => PinOrigin::User,
                PinSource::Config => PinOrigin::Config,
                PinSource::Model => PinOrigin::Model,
                PinSource::AgentsMd => PinOrigin::AgentsMd,
                PinSource::ClaudeMd => PinOrigin::ClaudeMd,
            },
        }
    }
}

impl From<&LoggedPin> for Pinned {
    fn from(p: &LoggedPin) -> Self {
        let source = match p.source {
            PinOrigin::User => PinSource::User,
            PinOrigin::Model => PinSource::Model,
            PinOrigin::AgentsMd => PinSource::AgentsMd,
            PinOrigin::ClaudeMd => PinSource::ClaudeMd,
            PinOrigin::Config | PinOrigin::Unknown => PinSource::Config,
        };
        Pinned::new(p.text.clone(), source)
    }
}

/// What an [`SessionRecord::ApprovalRequested`] asks about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalKind {
    Tool,
    McpServer,
    Memory,
    /// A kind this version does not know (read only).
    #[serde(other)]
    Unknown,
}

/// The `type` of every record this version writes.
const KNOWN_TYPES: &[&str] = &[
    "command",
    "prefix_applied",
    "request",
    "provider",
    "charged",
    "tool_call",
    "tool_result",
    "edit",
    "writes_lost",
    "undone",
    "uncompacted",
    "compaction",
    "gc_applied",
    "memory_index_applied",
    "memory_proposal",
    "memory_decision",
    "pins_folded",
    "start_pins",
    "approval_requested",
    "checkpoint",
    "checkpoint_skipped",
    "labeled",
    "restore_begun",
    "restore_ended",
    "model_set",
    "reasoning_stripped",
    "engine_error",
    "turn_start",
    "forked",
    "turn_end",
];

impl Serialize for SessionRecord {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        SessionRecord::serialize(self, s)
    }
}

/// The record types written without a payload: a newer version may give them one, which
/// this version ignores (spec 6.5: unit records may gain fields).
const UNIT_TYPES: &[&str] = &["pins_folded", "reasoning_stripped", "forked"];

/// A line that is valid JSON reads as a record whatever its payload (spec 6.5): an unknown
/// type, or a known one whose payload this version cannot read (a newer shape, a value it
/// never writes), reads as `Unknown` (a command's as `Command::Unknown`), never as a
/// corrupt log. Payloads of the builds before the format freeze are upgraded first.
impl<'de> Deserialize<'de> for SessionRecord {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let mut value = Value::deserialize(d)?;
        let Some(ty) = value
            .get("type")
            .and_then(Value::as_str)
            .filter(|t| KNOWN_TYPES.contains(t))
            .map(str::to_string)
        else {
            return Ok(SessionRecord::Unknown);
        };
        if let Some(obj) = value.as_object_mut() {
            if UNIT_TYPES.contains(&ty.as_str()) {
                obj.remove("data");
            } else if let Some(Value::Object(data)) = obj.get_mut("data") {
                upgrade(&ty, data);
            }
        }
        Ok(match SessionRecord::deserialize(value) {
            Ok(rec) => rec,
            Err(_) if ty == "command" => SessionRecord::Command(Command::Unknown),
            Err(_) => SessionRecord::Unknown,
        })
    }
}

/// Payload fields written by builds before the format freeze, in their frozen form.
fn upgrade(ty: &str, data: &mut serde_json::Map<String, Value>) {
    let flag = |data: &mut serde_json::Map<String, Value>, k: &str| {
        data.remove(k).and_then(|v| v.as_bool()) == Some(true)
    };
    match ty {
        "restore_begun" if !data.contains_key("by") => {
            if flag(data, "restore_backup") {
                data.insert("by".into(), "restore_backup".into());
            }
        }
        "restore_ended" if !data.contains_key("outcome") && data.contains_key("applied") => {
            let outcome = match (flag(data, "applied"), flag(data, "rolled_back")) {
                (true, _) => "applied",
                (false, true) => "rolled_back",
                (false, false) => "failed",
            };
            data.insert("outcome".into(), outcome.into());
        }
        _ => {}
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

/// The model the session uses at the end of the log: the last `ModelSet` record's, else
/// the header's model. Resume (Task 26) starts the rebuilt session on it; while replaying,
/// a `ModelSet` to another provider strips the reasoning before it
/// (`Context::strip_reasoning(Some(provider))`), and a `ReasoningStripped` strips all of it
/// (`Context::strip_reasoning(None)`), each at its position, as the live session did.
pub fn session_model(
    reader: &SessionReader<SessionRecord>,
) -> tautly_session::Result<Option<ModelId>> {
    let mut model = reader.header().model.clone().map(ModelId::from);
    for rec in reader.records() {
        if let (_, SessionRecord::ModelSet { model: m }) = rec? {
            model = Some(m);
        }
    }
    Ok(model)
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
            ran: true,
            blocked: false,
        };
        let v = serde_json::to_value(&rec).unwrap();
        assert_eq!(v["data"]["effects"][0]["path"], "ok.txt");
    }

    fn rel(s: &str) -> RelPath {
        RelPath::from_abs(Path::new("/r"), &Path::new("/r").join(s)).unwrap()
    }

    #[test]
    fn kinds_reasons_and_effects_from_a_newer_version_read_as_unknown() {
        let approval: SessionRecord = serde_json::from_str(
            r#"{"type":"approval_requested","data":{"request_id":"r","kind":"from_the_future",
                "call_id":null,"reason":"x"}}"#,
        )
        .unwrap();
        assert!(matches!(
            approval,
            SessionRecord::ApprovalRequested {
                kind: ApprovalKind::Unknown,
                ..
            }
        ));
        let gc: SessionRecord = serde_json::from_str(
            r#"{"type":"gc_applied","data":{"elided":[],"reason":"from_the_future",
                "tokens_freed":0,"forgotten_paths":[]}}"#,
        )
        .unwrap();
        assert!(matches!(
            gc,
            SessionRecord::GcApplied {
                reason: MutationReason::Unknown,
                ..
            }
        ));
        let result: SessionRecord = serde_json::from_str(&format!(
            r#"{{"type":"tool_result","data":{{"id":"c","result_blob":"{}","is_error":false,
                "effects":[{{"type":"from_the_future","x":1}},{{"type":"exec","may_write":false}}],
                "meta":{{"exit_code":null,"error_kind":null}}}}}}"#,
            "a".repeat(64)
        ))
        .unwrap();
        let SessionRecord::ToolResult { effects, .. } = result else {
            panic!("{result:?}")
        };
        assert_eq!(
            effects,
            [
                PersistedEffect::Unknown,
                PersistedEffect::Exec { may_write: false }
            ]
        );
    }

    #[test]
    fn records_holding_values_from_a_newer_version_stay_readable() {
        let read = |line: &str| -> SessionRecord {
            serde_json::from_str(line).unwrap_or_else(|e| panic!("{line}: {e}"))
        };
        assert_eq!(
            read(r#"{"type":"command","data":{"type":"teleport","to":"mars"}}"#),
            SessionRecord::Command(Command::Unknown)
        );
        assert_eq!(
            read(
                r#"{"type":"command","data":{"type":"set_budget","scope":"week",
                    "max_usd":null,"max_tokens":5}}"#
            ),
            SessionRecord::Command(Command::SetBudget {
                scope: crate::protocol::BudgetScope::Unknown,
                max_usd: None,
                max_tokens: Some(5),
            })
        );
        assert_eq!(
            read(r#"{"type":"provider","data":{"citation":{"url":"u"}}}"#),
            SessionRecord::Provider(ProviderEvent::Unknown)
        );
        assert_eq!(
            read(r#"{"type":"provider","data":{"done":{"stop":{"paused":{"s":3}}}}}"#),
            SessionRecord::Provider(ProviderEvent::Done {
                stop: tautly_provider::StopReason::Unknown
            })
        );
        let result = read(&format!(
            r#"{{"type":"tool_result","data":{{"id":"c","result_blob":"{}","is_error":false,
                "effects":[{{"type":"read","path":"a.txt","range":{{"start":null,"end":null}},
                "file_hash":"{}","shown":{{"type":"folded","n":2}}}}],
                "meta":{{"exit_code":null,"error_kind":null}}}}}}"#,
            "a".repeat(64),
            "b".repeat(64)
        ));
        let SessionRecord::ToolResult { effects, .. } = result else {
            panic!("{result:?}")
        };
        assert!(
            matches!(
                effects[..],
                [PersistedEffect::Read {
                    shown: tautly_tools::Shown::Unknown,
                    ..
                }]
            ),
            "{effects:?}"
        );
        let proposal = read(
            r#"{"type":"memory_proposal","data":{"id":"mem-1","change":{"upsert":{"id":"x",
                "title":"t","kind":"pattern","anchors":[],"created":0,"body":"b"},"delete":[]}}}"#,
        );
        let SessionRecord::MemoryProposal { change, .. } = proposal else {
            panic!("{proposal:?}")
        };
        assert_eq!(change.upsert.unwrap().kind, tautly_memory::MemoryKind::Fact);
    }

    #[test]
    fn an_edit_written_with_turn_reads_as_its_step() {
        let rec: SessionRecord = serde_json::from_str(
            r#"{"type":"edit","data":{"path":"a.txt","pre_image":null,"post_image_blob":null,
                "turn":4}}"#,
        )
        .unwrap();
        assert!(
            matches!(rec, SessionRecord::Edit { step: 4, .. }),
            "{rec:?}"
        );
        let v = serde_json::to_value(&rec).unwrap();
        assert_eq!(v["data"]["step"], 4);
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
        // A known type with a payload this version cannot read is not a corrupt log.
        assert_eq!(
            serde_json::from_str::<SessionRecord>(r#"{"type":"request","data":7}"#).unwrap(),
            SessionRecord::Unknown
        );
        // A line that is not JSON is.
        assert!(serde_json::from_str::<SessionRecord>(r#"{"type":"request","#).is_err());
    }

    #[test]
    fn records_written_before_their_additive_fields_still_read() {
        let old: SessionRecord = serde_json::from_str(
            r#"{"type":"prefix_applied","data":{"agents_md":null,"repo_map":null,
                "system_prompt_hash":"s","tools_hash":"t"}}"#,
        )
        .unwrap();
        assert!(matches!(
            old,
            SessionRecord::PrefixApplied { tools: None, .. }
        ));
        let lost: SessionRecord =
            serde_json::from_str(r#"{"type":"writes_lost","data":{"paths":[]}}"#).unwrap();
        assert!(matches!(
            lost,
            SessionRecord::WritesLost { late: false, .. }
        ));
        let checkpoint: SessionRecord = serde_json::from_str(
            r#"{"type":"checkpoint","data":{"n":1,"commit":"c","tree":"t","head":null,
                "dirty":false,"turn":1,"step":2}}"#,
        )
        .unwrap();
        assert!(matches!(
            checkpoint,
            SessionRecord::Checkpoint { point: None, .. }
        ));
    }

    #[test]
    fn unit_records_and_nested_enums_accept_a_newer_versions_fields() {
        // cp5 review C I3: unit records may gain fields; `Wrote` may gain variants.
        let read = |line: &str| -> SessionRecord {
            serde_json::from_str(line).unwrap_or_else(|e| panic!("{line}: {e}"))
        };
        assert_eq!(
            read(r#"{"type":"forked","data":{"at":3}}"#),
            SessionRecord::Forked
        );
        assert_eq!(
            read(r#"{"type":"forked","data":{}}"#),
            SessionRecord::Forked
        );
        assert_eq!(
            read(r#"{"type":"pins_folded","data":{"n":1}}"#),
            SessionRecord::PinsFolded
        );
        assert_eq!(
            read(r#"{"type":"reasoning_stripped","data":{"why":"x"}}"#),
            SessionRecord::ReasoningStripped
        );
        let begun = read(
            r#"{"type":"restore_begun","data":{"backup_ref":null,"files":[{"path":"a",
                "existed":true,"from_blob":null,"to":{"link":"b"},"later":1}],"paths":[],
                "by":"teleport","extra":true}}"#,
        );
        let SessionRecord::RestoreBegun { files, by, .. } = begun else {
            panic!("{begun:?}")
        };
        assert_eq!(files[0].to, Some(Wrote::Unknown));
        assert_eq!(by, RestoreBy::Unknown);
        assert_eq!(
            read(r#"{"type":"restore_ended","data":{"outcome":"half","why":"x"}}"#),
            SessionRecord::RestoreEnded {
                outcome: RestoreOutcome::Unknown
            }
        );
        assert_eq!(
            read(r#"{"type":"command","data":{"type":"cancel","reason":"x"}}"#),
            SessionRecord::Command(Command::Cancel)
        );
    }

    #[test]
    fn a_payload_this_version_cannot_read_is_unknown_not_corrupt() {
        // cp5 review C I2: a non-finite speed was logged as `null`.
        let read = |line: &str| -> SessionRecord {
            serde_json::from_str(line).unwrap_or_else(|e| panic!("{line}: {e}"))
        };
        assert_eq!(
            read(r#"{"type":"command","data":{"type":"replay","session_id":null,"speed":null}}"#),
            SessionRecord::Command(Command::Unknown)
        );
        assert_eq!(
            read(r#"{"type":"turn_start","data":{"turn":"one"}}"#),
            SessionRecord::Unknown
        );
    }

    #[test]
    fn restore_records_of_the_builds_before_the_freeze_still_read() {
        // M8: renamed fields and the bools that became enums.
        let blob = "b".repeat(64);
        let begun: SessionRecord = serde_json::from_str(&format!(
            r#"{{"type":"restore_begun","data":{{"backup_ref":null,"files":[{{"path":"a",
                "existed":true,"blob":"{blob}","mode":420,"wrote":"absent","wrote_mode":384}}],
                "paths":["a"],"restore_backup":true}}}}"#
        ))
        .unwrap();
        let SessionRecord::RestoreBegun { files, by, .. } = begun else {
            panic!("{begun:?}")
        };
        assert_eq!(
            files[0],
            BackupFile {
                path: rel("a"),
                existed: true,
                from_blob: Some(BlobRef::parse(&blob).unwrap()),
                from_mode: Some(0o644),
                to: Some(Wrote::Absent),
                to_mode: Some(0o600),
            }
        );
        assert_eq!(by, RestoreBy::RestoreBackup);
        let unmarked: SessionRecord = serde_json::from_str(
            r#"{"type":"restore_begun","data":{"backup_ref":"r","files":[],"paths":[]}}"#,
        )
        .unwrap();
        assert!(matches!(
            unmarked,
            SessionRecord::RestoreBegun {
                by: RestoreBy::Unknown,
                ..
            }
        ));
        for (old, outcome) in [
            (r#"{"applied":true}"#, RestoreOutcome::Applied),
            (
                r#"{"applied":true,"rolled_back":false}"#,
                RestoreOutcome::Applied,
            ),
            (
                r#"{"applied":false,"rolled_back":true}"#,
                RestoreOutcome::RolledBack,
            ),
            (
                r#"{"applied":false,"rolled_back":false}"#,
                RestoreOutcome::Failed,
            ),
            (r#"{"applied":false}"#, RestoreOutcome::Failed),
        ] {
            let rec: SessionRecord =
                serde_json::from_str(&format!(r#"{{"type":"restore_ended","data":{old}}}"#))
                    .unwrap();
            assert_eq!(rec, SessionRecord::RestoreEnded { outcome }, "{old}");
        }
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
                tools: Some(blob.clone()),
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
            SessionRecord::Charged {
                role: UsageRole::Side,
                model: ModelId::from("mock/small"),
                usage: RawUsage::default(),
                estimated: false,
            },
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
                ran: true,
                blocked: false,
            },
            SessionRecord::Edit {
                path: rel("a"),
                pre_image: None,
                post_image_blob: Some(blob.clone()),
                step: 1,
                asserted: false,
            },
            SessionRecord::Undone {
                turn: 1,
                files: vec![UndoneFile {
                    path: rel("a"),
                    from_blob: Some(blob.clone()),
                    from_mode: Some(0o755),
                    to: None,
                    to_mode: None,
                }],
                backup_ref: Some("refs/tautly/checkpoints/s/undo-1".into()),
            },
            SessionRecord::Uncompacted {
                compaction: true,
                restored: vec!["c1".into()],
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
            SessionRecord::MemoryProposal {
                id: "mem-1".into(),
                change: MemoryChange {
                    upsert: None,
                    delete: vec!["old".into()],
                },
            },
            SessionRecord::MemoryDecision {
                id: "mem-1".into(),
                approved: true,
            },
            SessionRecord::PinsFolded,
            SessionRecord::StartPins {
                pins: vec![LoggedPin {
                    text: "use tabs".into(),
                    source: PinOrigin::AgentsMd,
                }],
            },
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
                point: Some(CheckpointPoint::SessionStart),
            },
            SessionRecord::CheckpointSkipped {
                reason: "timeout".into(),
                turn: 1,
                step: 3,
                point: Some(CheckpointPoint::BeforeExec),
            },
            SessionRecord::Labeled {
                label: "try-a".into(),
            },
            SessionRecord::RestoreBegun {
                backup_ref: None,
                paths: vec![rel("a")],
                files: vec![BackupFile {
                    path: rel("a"),
                    existed: true,
                    from_blob: Some(BlobRef::parse(&"b".repeat(64)).unwrap()),
                    from_mode: Some(0o644),
                    to: Some(Wrote::Absent),
                    to_mode: Some(0o600),
                }],
                by: RestoreBy::RestoreBackup,
            },
            SessionRecord::RestoreEnded {
                outcome: RestoreOutcome::RolledBack,
            },
            SessionRecord::WritesLost {
                paths: vec![rel("a.txt")],
                late: true,
            },
            SessionRecord::ModelSet {
                model: ModelId::from("openai/gpt-5"),
            },
            SessionRecord::ReasoningStripped,
            SessionRecord::EngineError {
                message: String::new(),
            },
            SessionRecord::TurnStart {
                turn: 1,
                text: "hi".into(),
            },
            SessionRecord::Forked,
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
