//! The client protocol: [`Event`]s the engine emits and [`Command`]s it accepts.
//!
//! Both are internally tagged (`{"type": "tool_finished", ...}`) with snake_case names.
//! Event and field additions are backwards compatible; removing or changing the meaning of
//! one is a major-version bump of [`PROTOCOL_VERSION`].
//!
//! # Compatibility
//!
//! Readers ignore fields they do not know (no wire type denies unknown fields), so adding
//! a field is a minor-version change; every new field that is not an `Option` must carry
//! `#[serde(default)]`. An event type, [`EffectView`] kind, [`ErrorKind`],
//! [`FinishReason`], [`UsageRole`] or [`DiffStatus`] value a reader does not know reads as
//! its `Unknown` variant instead of failing the message, so those can also grow in minor
//! versions (match with a wildcard arm). New values of enums defined in other crates
//! ([`MutationReason`], `Shown`, `MemoryKind`) are a major-version change. An unknown
//! [`Command`] is an error: the engine cannot ignore what it was asked to do.
//! Timestamps are unix seconds (`u64`).
//!
//! An event never carries a session id: the transport (JSON-RPC) adds it per notification.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tautly_context::{Breakdown, ContextStatus, MutationReason};
pub use tautly_mcp::McpState;
use tautly_memory::MemoryChange;
use tautly_provider::{ModelId, Quota};
use tautly_tools::{AppliedEffect, DiffPreview, LineRange, Shown, ToolMeta};

use crate::evidence::TestEvidence;

/// `(major, minor)` of this protocol.
pub const PROTOCOL_VERSION: (u32, u32) = (1, 0);

/// Which request a [`Event::Usage`] belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageRole {
    /// The model driving the conversation.
    Main,
    /// A side model (compaction summaries, memory consolidation, `web_fetch` questions).
    Side,
    /// A role this version does not know.
    #[serde(other)]
    Unknown,
}

/// Why a turn ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    Completed,
    Cancelled,
    Budget,
    MaxTurns,
    Timeout,
    Error,
    /// A reason this version does not know.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    ProviderTransient,
    ProviderFatal,
    ContextOverflow,
    Tool,
    Policy,
    Internal,
    /// A kind this version does not know.
    #[serde(other)]
    Unknown,
}

/// What a finished tool call changed, as shown to clients. File contents (pre-images) are
/// left out, and a path that is not valid UTF-8 appears lossily (invalid bytes become
/// U+FFFD), so serializing an event can never fail on a file name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EffectView {
    Read {
        #[serde(with = "lossy_path")]
        path: PathBuf,
        range: LineRange,
        shown: Shown,
    },
    Edited {
        #[serde(with = "lossy_path")]
        path: PathBuf,
        /// The write created the file.
        created: bool,
        /// blake3 of the file as written, lowercase hex.
        post_hash: String,
    },
    Deleted {
        #[serde(with = "lossy_path")]
        path: PathBuf,
    },
    Exec {
        may_write: bool,
    },
    /// An effect kind this version does not know.
    #[serde(other)]
    Unknown,
}

impl From<&AppliedEffect> for EffectView {
    fn from(effect: &AppliedEffect) -> Self {
        match effect {
            AppliedEffect::Read {
                path, range, shown, ..
            } => EffectView::Read {
                path: path.clone(),
                range: *range,
                shown: *shown,
            },
            AppliedEffect::Edited {
                path,
                pre_image,
                post_hash,
                ..
            } => EffectView::Edited {
                path: path.clone(),
                created: pre_image.is_none(),
                post_hash: post_hash.to_hex().to_string(),
            },
            AppliedEffect::Deleted { path, .. } => EffectView::Deleted { path: path.clone() },
            AppliedEffect::Exec { may_write } => EffectView::Exec {
                may_write: *may_write,
            },
        }
    }
}

/// [`ToolMeta`] in wire form.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolMetaView {
    /// A command's exit code (signals as 128 + n).
    pub exit_code: Option<i32>,
    /// A stable name for why the call failed (`not_found`, `stale`, ...).
    pub error_kind: Option<String>,
}

impl From<&ToolMeta> for ToolMetaView {
    fn from(meta: &ToolMeta) -> Self {
        ToolMetaView {
            exit_code: meta.exit_code,
            error_kind: meta.error_kind.map(str::to_string),
        }
    }
}

/// Engine to client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// A user message started a turn.
    TurnStarted,
    TextDelta {
        text: String,
    },
    ReasoningDelta {
        text: String,
    },
    ToolCallProposed {
        call_id: String,
        tool: String,
        args: Value,
    },
    /// The policy said `ask`. The call waits for [`Command::Approve`] or [`Command::Deny`]
    /// naming `request_id`.
    ToolApprovalRequested {
        request_id: String,
        call_id: String,
        tool: String,
        args: Value,
        reason: String,
        /// What the call would change (edits and writes); the approved diff is the applied
        /// one, since `run` refuses if a file changed after this preview.
        #[serde(default)]
        diff: Option<DiffPreview>,
    },
    ToolStarted {
        call_id: String,
        tool: String,
    },
    ToolOutputDelta {
        call_id: String,
        text: String,
    },
    ToolFinished {
        call_id: String,
        tool: String,
        output: String,
        is_error: bool,
        /// The loop breaker refused to repeat a failed attempt; the call did not run.
        blocked: bool,
        effects: Vec<EffectView>,
        meta: ToolMetaView,
        /// The diff a successful edit or write applied.
        #[serde(default)]
        diff: Option<DiffPreview>,
    },
    Usage {
        role: UsageRole,
        tokens_in: u64,
        tokens_out: u64,
        cache_read: u64,
        cache_write: u64,
        /// `None` when the model has no known price.
        cost_usd: Option<f64>,
        #[serde(default, with = "quota_wire")]
        quota: Option<Quota>,
        /// Some of the numbers are the engine's estimate: the response broke off or was
        /// cancelled before the provider said what it billed. Sent only when true.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        estimated: bool,
    },
    /// Emitted every turn.
    ContextStatus {
        used: u32,
        budget: u32,
        breakdown: Breakdown,
        warnings: Vec<String>,
    },
    Compacted {
        removed_turns: u32,
        summary: String,
        reason: MutationReason,
    },
    ContextCollected {
        elided_results: u32,
        tokens_freed: u32,
        reason: MutationReason,
    },
    /// A memory entry to write and/or entries to delete, waiting for
    /// [`Command::ApproveMemory`] or [`Command::RejectMemory`].
    MemoryProposed {
        proposal_id: String,
        #[serde(with = "memory_change_wire")]
        change: MemoryChange,
    },
    SessionChanged {
        session_id: String,
    },
    TurnFinished {
        reason: FinishReason,
        /// The turn's last test run and whether it is still fresh (spec 7.11.7); absent
        /// when the turn ran none, and in events from before this field existed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        evidence: Option<TestEvidence>,
    },
    Error {
        kind: ErrorKind,
        message: String,
        retryable: bool,
        hint: Option<String>,
    },
    /// The answer to [`Command::Diff`]: the net change per file that Tautly's tools made in
    /// the session (`turn` None) or in user turn `turn`, with the concatenated diff (capped
    /// at 2,000 lines).
    SessionDiff {
        turn: Option<u32>,
        files: Vec<DiffFile>,
        unified: String,
        truncated: bool,
    },
    /// The answer to a successful [`Command::SetModel`]: the model requests go to from now
    /// on, and its provider (`anthropic`, `openai`, `google` or `compat:<name>`).
    ModelChanged {
        model: ModelId,
        provider: String,
    },
    /// Something the user should know that does not end the turn (a skipped non-UTF-8
    /// path, a malformed AGENTS.md section, memory entries that failed to load).
    Warning {
        message: String,
    },
    /// A repository-declared MCP server (`source`: `project` or `mcp_json`) wants to start
    /// `launch` (its command line or URL, and the names of its variables or headers). It
    /// starts only after [`Command::Approve`] naming `request_id`, which is remembered;
    /// [`Command::Deny`] keeps it stopped for the session.
    McpServerApprovalRequested {
        request_id: String,
        server: String,
        source: String,
        launch: String,
    },
    /// The answer to [`Command::Undo`] (spec 7.11.11): the root-relative paths user turn
    /// `turn`'s undo restored and deleted (with `dry_run`, would have; nothing changed),
    /// the ref of the snapshot taken just before (`git` can restore it), and a `note` on
    /// what was not undone.
    Undone {
        turn: u32,
        restored: Vec<String>,
        deleted: Vec<String>,
        dry_run: bool,
        backup_ref: Option<String>,
        note: Option<String>,
    },
    /// An MCP server changed state; `tools` is how many of its tools are offered.
    McpServerStatus {
        server: String,
        state: McpState,
        tools: u32,
        message: Option<String>,
    },
    /// An event type this version does not know.
    #[serde(other)]
    Unknown,
}

impl Event {
    /// A [`Event::ContextStatus`] from the context crate's status.
    pub fn context_status(status: &ContextStatus) -> Event {
        Event::ContextStatus {
            used: status.used,
            budget: status.budget,
            breakdown: status.breakdown,
            warnings: status.warnings.clone(),
        }
    }
}

/// One file of an [`Event::SessionDiff`]: its root-relative path and line counts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffFile {
    pub path: String,
    pub status: DiffStatus,
    pub added: u32,
    pub removed: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffStatus {
    Added,
    Modified,
    Deleted,
    /// A status this version does not know.
    #[serde(other)]
    Unknown,
}

/// What a budget limit applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetScope {
    Turn,
    Session,
}

/// Client to engine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
    SendMessage {
        text: String,
    },
    /// Allow the call behind a [`Event::ToolApprovalRequested`], or the server behind a
    /// [`Event::McpServerApprovalRequested`] (request ids are unique across both).
    Approve {
        request_id: String,
    },
    Deny {
        request_id: String,
        reason: Option<String>,
    },
    Cancel,
    SetBudget {
        scope: BudgetScope,
        max_usd: Option<f64>,
        max_tokens: Option<u64>,
    },
    Compact,
    Uncompact,
    Pin {
        text: String,
    },
    /// Remove the pin whose text is exactly `text`.
    Unpin {
        text: String,
    },
    ApproveMemory {
        proposal_id: String,
    },
    RejectMemory {
        proposal_id: String,
    },
    ConsolidateMemory,
    /// Branch the conversation (spec 7.12.3): a new session whose log is this one's
    /// records up to the end of step `at` (default: the tip), copied verbatim, with
    /// `origin: fork` and `label`. Files are not touched. Answered with
    /// [`Event::SessionChanged`] naming the branch, on which the engine continues.
    Fork {
        #[serde(default)]
        at: Option<u32>,
        #[serde(default)]
        label: Option<String>,
    },
    /// Answered with [`Event::SessionDiff`]: the whole session (None) or user turn `turn`
    /// (1-based).
    Diff {
        turn: Option<u32>,
    },
    /// Switch the session to `model` (spec 7.11.5): at once when idle, else at the start of
    /// the next user turn. Answered with [`Event::ModelChanged`], or an `Error` that leaves
    /// the model as it was.
    SetModel {
        model: ModelId,
    },
    /// Revert the file changes of user turn `turn` (1-based; default: the last), its
    /// commands' too where git checkpoints were taken (spec 7.11.11). Waits for a running
    /// turn to end. Answered with [`Event::Undone`], or an `Error` (nothing changed) when a
    /// file was changed since by someone else or by a later turn that is not undone.
    Undo {
        #[serde(default)]
        turn: Option<u32>,
        #[serde(default)]
        dry_run: bool,
    },
    Shutdown,
}

/// `#[serde(with = "...")]` for a `PathBuf` that must serialize even when it is not valid
/// UTF-8: the text form replaces invalid bytes with U+FFFD, so it is for display only and
/// does not round-trip such a path.
pub mod lossy_path {
    use std::path::{Path, PathBuf};

    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(path: &Path, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&path.to_string_lossy())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<PathBuf, D::Error> {
        Ok(PathBuf::from(String::deserialize(d)?))
    }
}

fn unix_secs(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn from_unix_secs(secs: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(secs)
}

/// `#[serde(with = "...")]` for [`Quota`], whose `reset_at` goes on the wire as unix
/// seconds instead of Rust's `SystemTime` form.
mod quota_wire {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use tautly_provider::Quota;

    use super::{from_unix_secs, unix_secs};

    #[derive(Serialize, Deserialize)]
    struct Wire {
        remaining_requests: Option<u64>,
        remaining_tokens: Option<u64>,
        #[serde(default)]
        reset_at: Option<u64>,
    }

    pub fn serialize<S: Serializer>(quota: &Option<Quota>, s: S) -> Result<S::Ok, S::Error> {
        quota
            .as_ref()
            .map(|q| Wire {
                remaining_requests: q.remaining_requests,
                remaining_tokens: q.remaining_tokens,
                reset_at: q.reset_at.map(unix_secs),
            })
            .serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Quota>, D::Error> {
        Ok(Option::<Wire>::deserialize(d)?.map(|w| Quota {
            remaining_requests: w.remaining_requests,
            remaining_tokens: w.remaining_tokens,
            reset_at: w.reset_at.map(from_unix_secs),
        }))
    }
}

/// `#[serde(with = "...")]` for [`MemoryChange`], whose entry timestamps (`created`,
/// `verified`) go on the wire as unix seconds; the entry's `stale` flag is not sent.
mod memory_change_wire {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use tautly_memory::{MemAnchor, MemoryChange, MemoryEntry, MemoryKind};

    use super::{from_unix_secs, unix_secs};

    #[derive(Serialize, Deserialize)]
    struct Wire {
        upsert: Option<EntryWire>,
        #[serde(default)]
        delete: Vec<String>,
    }

    #[derive(Serialize, Deserialize)]
    struct EntryWire {
        id: String,
        title: String,
        kind: MemoryKind,
        anchors: Vec<MemAnchor>,
        created: u64,
        #[serde(default)]
        verified: Option<u64>,
        body: String,
    }

    pub fn serialize<S: Serializer>(change: &MemoryChange, s: S) -> Result<S::Ok, S::Error> {
        Wire {
            upsert: change.upsert.as_ref().map(|e| EntryWire {
                id: e.id.clone(),
                title: e.title.clone(),
                kind: e.kind,
                anchors: e.anchors.clone(),
                created: unix_secs(e.created),
                verified: e.verified.map(unix_secs),
                body: e.body.clone(),
            }),
            delete: change.delete.clone(),
        }
        .serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<MemoryChange, D::Error> {
        let w = Wire::deserialize(d)?;
        Ok(MemoryChange {
            upsert: w.upsert.map(|e| MemoryEntry {
                id: e.id,
                title: e.title,
                kind: e.kind,
                anchors: e.anchors,
                created: from_unix_secs(e.created),
                verified: e.verified.map(from_unix_secs),
                body: e.body,
                stale: false,
            }),
            delete: w.delete,
        })
    }
}
