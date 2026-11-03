//! The client protocol: [`Event`]s the engine emits and [`Command`]s it accepts.
//!
//! Both are internally tagged (`{"type": "tool_finished", ...}`) with snake_case names.
//! Event and field additions are backwards compatible; removing or changing the meaning of
//! one is a major-version bump of [`PROTOCOL_VERSION`].
//!
//! An event never carries a session id: the transport (JSON-RPC) adds it per notification.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tautly_context::{Breakdown, ContextStatus, MutationReason};
use tautly_memory::MemoryChange;
use tautly_provider::Quota;
use tautly_tools::{AppliedEffect, LineRange, Shown, ToolMeta};

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
    },
    Deleted {
        #[serde(with = "lossy_path")]
        path: PathBuf,
    },
    Exec {
        may_write: bool,
    },
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
                path, pre_image, ..
            } => EffectView::Edited {
                path: path.clone(),
                created: pre_image.is_none(),
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
    },
    Usage {
        role: UsageRole,
        tokens_in: u64,
        tokens_out: u64,
        cache_read: u64,
        cache_write: u64,
        /// `None` when the model has no known price.
        cost_usd: Option<f64>,
        quota: Option<Quota>,
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
        change: MemoryChange,
    },
    SessionChanged {
        session_id: String,
    },
    TurnFinished {
        reason: FinishReason,
    },
    Error {
        kind: ErrorKind,
        message: String,
        retryable: bool,
        hint: Option<String>,
    },
    /// Something the user should know that does not end the turn (a skipped non-UTF-8
    /// path, a malformed AGENTS.md section, memory entries that failed to load).
    Warning {
        message: String,
    },
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
    /// Allow the call behind a [`Event::ToolApprovalRequested`].
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
    Fork,
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
