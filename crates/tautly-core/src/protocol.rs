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
//! [`FinishReason`], [`UsageRole`], [`DiffStatus`], `Shown`, [`MutationReason`] or
//! [`McpState`] value a reader does not know reads as its `Unknown` variant instead of
//! failing the message, so those can also grow in minor versions (match with a wildcard
//! arm). An unknown `MemoryKind` reads as `fact`. A [`Command`] or [`BudgetScope`] the
//! engine does not know reads as `Unknown` too (so a session log holding one stays
//! readable), and the engine answers it with an `Error` (a `Warning` for `SetBudget`): it
//! cannot ignore what it was asked to do.
//! Timestamps are unix seconds (`u64`).
//!
//! An event never carries a session id: the transport (JSON-RPC) adds it per notification.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tautly_context::{Breakdown, ContextStatus, MutationReason};
pub use tautly_mcp::McpState;
use tautly_memory::{MemoryChange, MemoryEntry};
use tautly_provider::{ModelId, Quota};
use tautly_tools::{AppliedEffect, DiffPreview, LineRange, Saved, Shown, ToolMeta};

use crate::evidence::TestEvidence;
use crate::reasoning::ReasoningLevel;
use crate::receipt::Savings;
pub use crate::timetravel::MaterializeReport;

/// `(major, minor)` of this protocol.
pub const PROTOCOL_VERSION: (u32, u32) = (1, 0);

/// Most entries an [`Event::MemoryProposed`] shows in `replaces`.
pub const MAX_REPLACED_SHOWN: usize = 16;

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

/// What kind of failure an [`Event::Error`] reports.
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
    /// What the result saved against a plain harness (an estimate, spec 7.11.1): an
    /// output filter's summary, a re-read diff, an index answer. Sent only when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub saved: Option<Saved>,
    /// The stage that located a repaired edit (`normalized`, `whitespace`, `indentation`,
    /// `anchor`, `structural`). Sent only when set; absent in older event streams.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repair: Option<String>,
}

impl From<&ToolMeta> for ToolMetaView {
    fn from(meta: &ToolMeta) -> Self {
        ToolMetaView {
            exit_code: meta.exit_code,
            error_kind: meta.error_kind.map(str::to_string),
            saved: meta.saved,
            repair: meta.repair.map(|s| s.name().to_string()),
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
    /// The end of a [`Command::Compact`], sent exactly once per command, last: `applied`
    /// when it collected or compacted anything (else it was cancelled, had nothing to do
    /// or failed, said by a `Warning` or `Error` before this), `tokens_freed` the context
    /// tokens it freed. Also sent after the `Error` answering a `Compact` that a lost
    /// session (a crashed turn) cannot run. Like `TurnFinished`, it waits a bounded time
    /// (5 s) for a client that is behind, then is dropped.
    CompactFinished {
        /// The `id` of the command this answers, when it carried one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        applied: bool,
        tokens_freed: u32,
    },
    /// A memory entry to write and/or entries to delete, waiting for
    /// [`Command::ApproveMemory`] or [`Command::RejectMemory`]. Sent only when the user is
    /// asked: a proposal saved without asking (`approve_memory`) or rejected because nobody
    /// can approve it (headless) is reported by a [`Event::Warning`] instead.
    MemoryProposed {
        proposal_id: String,
        #[serde(with = "memory_change_wire")]
        change: MemoryChange,
        /// The entries as they are now that the change would overwrite (the upserted id, if
        /// it exists) or delete, so a client can show the change as a diff; at most
        /// [`MAX_REPLACED_SHOWN`]. Absent when there are none, and in events from before
        /// this field existed.
        #[serde(
            default,
            skip_serializing_if = "Vec::is_empty",
            with = "memory_entries_wire"
        )]
        replaces: Vec<MemoryEntry>,
    },
    SessionChanged {
        /// The `id` of the command this answers, when it carried one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        session_id: String,
    },
    /// What user turn `turn` (1-based) spent and saved (spec 7.11.1), sent right before its
    /// `TurnFinished` whatever the reason. Token fields sum the main and side roles;
    /// `side_tokens` is the side share (input + output). `cache_hit_ratio` is
    /// `cache_read / (tokens_in + cache_read)`, absent when both are 0. `wall_ms` runs
    /// from `TurnStarted` to `TurnFinished`. `steps` counts distinct provider steps (a step
    /// sent again after an overflow counts once); `tool_calls` counts the calls that ran
    /// (not those refused, denied, blocked or cancelled first). Every number in `saved` is
    /// an estimate: clients label it "est.".
    TurnReceipt {
        turn: u32,
        model: ModelId,
        tokens_in: u64,
        tokens_out: u64,
        cache_read: u64,
        cache_write: u64,
        side_tokens: u64,
        /// The priced usage's cost (none: nothing was priced).
        cost_usd: Option<f64>,
        /// Some usage was on a model without a known price: `cost_usd` covers the rest.
        /// Sent only when true.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        cost_partial: bool,
        wall_ms: u64,
        cache_hit_ratio: Option<f64>,
        steps: u32,
        tool_calls: u32,
        saved: Savings,
        /// Some of the usage is the engine's estimate (a response that reported none,
        /// broke off or was cancelled). Sent only when true.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        estimated: bool,
    },
    TurnFinished {
        /// The `id` of the command this answers, when it carried one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        reason: FinishReason,
        /// The turn's last test run and whether it is still fresh (spec 7.11.7); absent
        /// when the turn ran none, and in events from before this field existed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        evidence: Option<TestEvidence>,
    },
    Error {
        /// The `id` of the command this answers, when it carried one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        kind: ErrorKind,
        message: String,
        retryable: bool,
        hint: Option<String>,
    },
    /// The answer to [`Command::Diff`]: the net change per file that Tautly's tools made in
    /// the session (`turn` None) or in user turn `turn`, with the concatenated diff (capped
    /// at 2,000 lines).
    SessionDiff {
        /// The `id` of the command this answers, when it carried one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        turn: Option<u32>,
        files: Vec<DiffFile>,
        unified: String,
        truncated: bool,
    },
    /// The answer to a successful [`Command::SetModel`]: the model requests go to from now
    /// on, and its provider (`anthropic`, `openai`, `google` or `compat:<name>`).
    ModelChanged {
        /// The `id` of the command this answers, when it carried one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
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
        /// The `id` of the command this answers, when it carried one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        turn: u32,
        restored: Vec<String>,
        deleted: Vec<String>,
        dry_run: bool,
        backup_ref: Option<String>,
        note: Option<String>,
    },
    /// The answer to [`Command::Rewind`] (spec 7.12.2): what materializing step `at`'s
    /// files changed (with `report.dry_run`, would change; with conflicts not listed in
    /// `overwrite`, nothing was written and `report.applied` is false). `branch` is the
    /// new session the engine continues on when it was applied (then
    /// [`Event::SessionChanged`] follows), else empty.
    Rewound {
        /// The `id` of the command this answers, when it carried one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        at: u32,
        branch: String,
        report: MaterializeReport,
    },
    /// The answer to [`Command::Switch`] (spec 7.12.3): `branch` is the session switched
    /// to (or, without `report.applied`, the one that would be); [`Event::SessionChanged`]
    /// follows when it was applied.
    Switched {
        /// The `id` of the command this answers, when it carried one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        branch: String,
        report: MaterializeReport,
    },
    /// An MCP server changed state; `tools` is how many of its tools are offered.
    McpServerStatus {
        server: String,
        state: McpState,
        tools: u32,
        message: Option<String>,
    },
    /// One event of a [`Command::Replay`] (spec 7.12.5): `event` as the logged stream
    /// had it, from the record at log `offset`, written at `t_ms` (Unix milliseconds).
    /// Nothing in it is live: its approval request and memory proposal ids are prefixed
    /// `replayed:` and can never be answered, and a client must not act on it.
    Replayed {
        offset: u64,
        t_ms: u64,
        event: Box<Event>,
    },
    /// The end of a [`Command::Replay`] of session `session_id` (the session replayed,
    /// which need not be the one the event is sent on): `events` [`Event::Replayed`] were
    /// sent; `cancelled` when a `Cancel` stopped it.
    ReplayFinished {
        /// The `id` of the command this answers, when it carried one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        session_id: String,
        events: u32,
        cancelled: bool,
    },
    /// The end of a [`Command::ConsolidateMemory`], sent exactly once per command, last
    /// (after its `MemoryProposed` events and warnings), waiting a bounded time like
    /// `CompactFinished`: `applied` changes were saved without asking (`approve_memory`),
    /// `failed` could not be saved or were refused (headless). Changes asked about were
    /// sent as `MemoryProposed` before this.
    ConsolidateFinished {
        /// The `id` of the command this answers, when it carried one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(default)]
        applied: u32,
        #[serde(default)]
        failed: u32,
    },
    /// The approval request `request_id` (a [`Event::ToolApprovalRequested`] or
    /// [`Event::McpServerApprovalRequested`]) is no longer waiting: its turn was cancelled
    /// (or timed out, or hit a budget) or the session is shutting down. A client closes
    /// the question it shows; an answer to it now only gets a warning.
    ApprovalWithdrawn {
        request_id: String,
    },
    /// The user's message that started a turn. Sent only inside [`Event::Replayed`] (a
    /// live client knows what it sent).
    UserMessage {
        text: String,
    },
    /// An event type this version does not know.
    #[serde(other)]
    Unknown,
}

impl Event {
    /// The `id` echoed from the command this event answers.
    pub fn reply_id(&self) -> Option<&str> {
        match self {
            Event::CompactFinished { id, .. }
            | Event::SessionChanged { id, .. }
            | Event::Error { id, .. }
            | Event::ModelChanged { id, .. }
            | Event::Undone { id, .. }
            | Event::Rewound { id, .. }
            | Event::Switched { id, .. }
            | Event::ReplayFinished { id, .. }
            | Event::ConsolidateFinished { id, .. }
            | Event::SessionDiff { id, .. }
            | Event::TurnFinished { id, .. } => id.as_deref(),
            _ => None,
        }
    }

    /// Set the echoed `id` (when this event can carry one and has none yet).
    pub fn set_reply_id(&mut self, reply: &str) {
        if let Event::CompactFinished { id, .. }
        | Event::SessionChanged { id, .. }
        | Event::Error { id, .. }
        | Event::ModelChanged { id, .. }
        | Event::Undone { id, .. }
        | Event::Rewound { id, .. }
        | Event::Switched { id, .. }
        | Event::ReplayFinished { id, .. }
        | Event::ConsolidateFinished { id, .. }
        | Event::SessionDiff { id, .. }
        | Event::TurnFinished { id, .. } = self
            && id.is_none()
        {
            *id = Some(reply.to_string());
        }
    }

    /// Whether this answers the command whose correlation id is `want` (None: any).
    pub fn answers(&self, want: Option<&str>) -> bool {
        want.is_none() || self.reply_id() == want
    }

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

/// How a file changed in a session diff.
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
    /// A scope this version does not know: a `SetBudget` for it is ignored with a warning.
    #[serde(other)]
    Unknown,
}

/// Client to engine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
    SendMessage {
        /// The client's correlation id, echoed as `id` on this command's answer.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        text: String,
    },
    /// Allow the call behind a [`Event::ToolApprovalRequested`], or the server behind a
    /// [`Event::McpServerApprovalRequested`] (request ids are unique across both).
    ///
    /// `session_rule` ("allow for this session", spec 7.11.8; tool calls only) is an allow
    /// rule (`bash(cargo test*)`, `edit(src/a.rs)`, `mcp__github__*`) added for as long as
    /// the engine runs. It is accepted only when it matches the pending call; otherwise the
    /// call is approved once with a `Warning` (an accepted one is announced with a
    /// `Warning` too: `allowed for this session: <rule>`). It never outranks a deny rule, a
    /// protected path or an unparseable command, never covers a write redirect it does not
    /// name, is never written to configuration or to the session log, and is gone after a
    /// resume. On an MCP server approval it is refused with an `Error` and the request stays
    /// pending ([`crate::exact_rule`] builds the rule a client offers).
    Approve {
        request_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_rule: Option<String>,
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
    Compact {
        /// The client's correlation id, echoed as `id` on this command's answer.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
    },
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
    ConsolidateMemory {
        /// The client's correlation id, echoed as `id` on this command's answer.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
    },
    /// Branch the conversation (spec 7.12.3): a new session whose log is this one's
    /// records up to the end of step `at` (default: the tip), copied verbatim, with
    /// `origin: fork` and `label`. Files are not touched. Answered with
    /// [`Event::SessionChanged`] naming the branch, on which the engine continues.
    Fork {
        /// The client's correlation id, echoed as `id` on this command's answer.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(default)]
        at: Option<u32>,
        #[serde(default)]
        label: Option<String>,
    },
    /// Answered with [`Event::SessionDiff`]: the whole session (None) or user turn `turn`
    /// (1-based).
    Diff {
        /// The client's correlation id, echoed as `id` on this command's answer.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        turn: Option<u32>,
    },
    /// Switch the session to `model` (spec 7.11.5): at once when idle, else at the start of
    /// the next user turn. Answered with [`Event::ModelChanged`], or an `Error` that leaves
    /// the model as it was.
    SetModel {
        /// The client's correlation id, echoed as `id` on this command's answer.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        model: ModelId,
    },
    /// How hard the main model thinks from its next request on (spec 4.7), even
    /// mid-turn; side-model requests stay without reasoning. No answer. Not replayed on
    /// resume, where the configured level applies again.
    SetReasoning {
        level: ReasoningLevel,
    },
    /// Revert the file changes of user turn `turn` (1-based; default: the last), its
    /// commands' too where git checkpoints were taken (spec 7.11.11). Waits for a running
    /// turn to end. Answered with [`Event::Undone`], or an `Error` (nothing changed) when a
    /// file was changed since by someone else or by a later turn that is not undone.
    Undo {
        /// The client's correlation id, echoed as `id` on this command's answer.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(default)]
        turn: Option<u32>,
        #[serde(default)]
        dry_run: bool,
    },
    /// Rewind files and conversation to step `at` as a new branch (spec 7.12.2): a
    /// checkpoint of the current tree is appended to this session first, then the branch
    /// (`origin: rewind`, `label`) gets this log's records up to step `at`, and the files
    /// become `state_at(at)`. A path that would change and whose content differs from what
    /// the agent left is a conflict: unless every conflict is listed in `overwrite`,
    /// nothing is written and no branch is made. A backup snapshot is taken before any
    /// write. Refused while another live session uses the working tree. Waits for a
    /// running turn to end. Answered with [`Event::Rewound`], or an `Error`.
    Rewind {
        /// The client's correlation id, echoed as `id` on this command's answer.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        at: u32,
        #[serde(default)]
        label: Option<String>,
        #[serde(default)]
        dry_run: bool,
        #[serde(default)]
        overwrite: Vec<String>,
    },
    /// Continue session `session_id` of this work tree instead (spec 7.12.3), with its tip
    /// materialized in place under the same conflict rules, backup and lease as
    /// [`Command::Rewind`]. Answered with [`Event::Switched`], or an `Error`.
    Switch {
        /// The client's correlation id, echoed as `id` on this command's answer.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        session_id: String,
        #[serde(default)]
        dry_run: bool,
        #[serde(default)]
        overwrite: Vec<String>,
    },
    /// Rename the current branch (a `Labeled` record).
    Label {
        label: String,
    },
    /// Play session `session_id` of this project back from its log (default: the current
    /// session; spec 7.12.5): each logged event as an [`Event::Replayed`], paced by the
    /// records' timestamps (each gap divided by `speed` and capped at 2 s; `speed` 0 is
    /// instant), from step `from_step` on (default: the start), then
    /// [`Event::ReplayFinished`]. Read-only: no provider request, no tool, no file
    /// access beyond the log and its blobs. Idle only: while anything runs (a turn, a job, the start-up, a `SetModel`
    /// provider build) it is answered with an `Error`. `Cancel` stops it.
    Replay {
        /// The client's correlation id, echoed as `id` on this command's answer.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(default)]
        session_id: Option<String>,
        #[serde(default = "one")]
        speed: f32,
        #[serde(default)]
        from_step: Option<u32>,
    },
    Shutdown,
    /// A command this version does not know (from a newer client, or read back from a
    /// newer log): answered with an `Error`, and skipped on resume.
    #[serde(other)]
    Unknown,
}

/// The default [`Command::Replay`] speed: real time.
fn one() -> f32 {
    1.0
}

impl Command {
    /// The client's correlation id, on the commands that have a terminal answer.
    pub fn id(&self) -> Option<&str> {
        match self {
            Command::SendMessage { id, .. }
            | Command::Diff { id, .. }
            | Command::Undo { id, .. }
            | Command::Fork { id, .. }
            | Command::Rewind { id, .. }
            | Command::Switch { id, .. }
            | Command::Replay { id, .. }
            | Command::Compact { id }
            | Command::SetModel { id, .. }
            | Command::ConsolidateMemory { id } => id.as_deref(),
            _ => None,
        }
    }

    /// Give the command the correlation id `default` when it takes one and has none (the
    /// RPC server's: the JSON-RPC request id).
    pub fn set_default_id(&mut self, default: String) {
        if let Command::SendMessage { id, .. }
        | Command::Diff { id, .. }
        | Command::Undo { id, .. }
        | Command::Fork { id, .. }
        | Command::Rewind { id, .. }
        | Command::Switch { id, .. }
        | Command::Replay { id, .. }
        | Command::Compact { id }
        | Command::SetModel { id, .. }
        | Command::ConsolidateMemory { id } = self
            && id.is_none()
        {
            *id = Some(default);
        }
    }

    /// Whether an `Error` can be this command's answer (it then echoes the command's `id`):
    /// `Undo`, `Fork`, `Rewind`, `Switch`, `SetModel` and `Diff`. The others end with their
    /// own event (`TurnFinished`, `CompactFinished`, `ConsolidateFinished`,
    /// `ReplayFinished`), and an `Error` before it says why, without the id; a `Replay`
    /// refused because something runs is the exception (its `Error` is its answer).
    pub fn error_answers(&self) -> bool {
        matches!(
            self,
            Command::Undo { .. }
                | Command::Fork { .. }
                | Command::Rewind { .. }
                | Command::Switch { .. }
                | Command::SetModel { .. }
                | Command::Diff { .. }
        )
    }

    /// The command as the session log records it: an `Approve` without its
    /// `session_rule`, which is consent for this process only and never stored (spec 5.5).
    pub fn for_log(&self) -> Command {
        match self {
            Command::Approve { request_id, .. } => Command::Approve {
                request_id: request_id.clone(),
                session_rule: None,
            },
            other => other.clone(),
        }
    }
}

/// `#[serde(with = "...")]` for a `PathBuf` that must serialize even when it is not valid
/// UTF-8: the text form replaces invalid bytes with U+FFFD, so it is for display only and
/// does not round-trip such a path.
pub mod lossy_path {
    use std::path::{Path, PathBuf};

    use serde::{Deserialize, Deserializer, Serializer};

    /// The path as (lossy) text.
    pub fn serialize<S: Serializer>(path: &Path, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&path.to_string_lossy())
    }

    /// A path from its text.
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
pub(crate) mod memory_change_wire {
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
    pub(super) struct EntryWire {
        id: String,
        title: String,
        #[serde(deserialize_with = "kind_or_fact")]
        kind: MemoryKind,
        anchors: Vec<MemAnchor>,
        created: u64,
        #[serde(default)]
        verified: Option<u64>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tags: Vec<String>,
        body: String,
    }

    /// A kind this version does not know (from a newer engine or log) reads as a fact, so
    /// the message or record that holds it stays readable.
    fn kind_or_fact<'de, D: Deserializer<'de>>(d: D) -> Result<MemoryKind, D::Error> {
        Ok(MemoryKind::parse(&String::deserialize(d)?).unwrap_or(MemoryKind::Fact))
    }

    impl From<&MemoryEntry> for EntryWire {
        fn from(e: &MemoryEntry) -> Self {
            EntryWire {
                id: e.id.clone(),
                title: e.title.clone(),
                kind: e.kind,
                anchors: e.anchors.clone(),
                created: unix_secs(e.created),
                verified: e.verified.map(unix_secs),
                tags: e.tags.clone(),
                body: e.body.clone(),
            }
        }
    }

    impl From<EntryWire> for MemoryEntry {
        fn from(e: EntryWire) -> Self {
            MemoryEntry {
                id: e.id,
                title: e.title,
                kind: e.kind,
                anchors: e.anchors,
                created: from_unix_secs(e.created),
                verified: e.verified.map(from_unix_secs),
                tags: e.tags,
                links: tautly_memory::parse_links(&e.body),
                body: e.body,
                stale: false,
            }
        }
    }

    pub fn serialize<S: Serializer>(change: &MemoryChange, s: S) -> Result<S::Ok, S::Error> {
        Wire {
            upsert: change.upsert.as_ref().map(EntryWire::from),
            delete: change.delete.clone(),
        }
        .serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<MemoryChange, D::Error> {
        let w = Wire::deserialize(d)?;
        Ok(MemoryChange {
            upsert: w.upsert.map(MemoryEntry::from),
            delete: w.delete,
        })
    }
}

/// Memory entries on the wire, as [`memory_change_wire`] writes an upserted one.
pub(crate) mod memory_entries_wire {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use tautly_memory::MemoryEntry;

    use super::memory_change_wire::EntryWire;

    pub fn serialize<S: Serializer>(entries: &[MemoryEntry], s: S) -> Result<S::Ok, S::Error> {
        let wire: Vec<EntryWire> = entries.iter().map(EntryWire::from).collect();
        wire.serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<MemoryEntry>, D::Error> {
        let wire = Vec::<EntryWire>::deserialize(d)?;
        Ok(wire.into_iter().map(MemoryEntry::from).collect())
    }
}
