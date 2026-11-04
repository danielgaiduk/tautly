//! Resume, fork, undo and the user's compaction commands (spec 6.4, 6.5, 7.12.1).
//!
//! # Rebuild
//!
//! [`rebuild`] replays a session log into the [`Context`] the live session had, record by
//! record, so the next request is the one the live session would have sent:
//!
//! - `TurnStart` pushes the user message (a message is logged as a command when it
//!   arrives, but its turn starts only after the turn before it ended).
//! - A step is its `Request` and the `Provider` events of its response, rebuilt as the
//!   live loop builds it, with the step's `ToolResult`s replayed through the attempt
//!   ledger, `apply_effects` and `push_tool_result`. A call without a result (the log
//!   ends mid-step) gets a synthetic `cancelled` result.
//! - `GcApplied` (`reapply_gc`), `Compaction` (`reapply_compaction`), `PinsFolded`,
//!   `MemoryIndexApplied`, `PrefixApplied` (AGENTS.md and the repo map as logged, never
//!   read from disk again; with a calibration, the requests after it are assembled with
//!   the tools it logged, MCP tools included), `StartPins`, `Uncompacted` and `Undone` are redone where they
//!   happened. Tool results are read back from their blobs; only references and the
//!   latest compaction stay in memory.
//! - `Pin`/`Unpin` sent while a turn runs reach it at its next step start, as they did
//!   live (at the step's first epoch record, its request, or the turn's end).
//! - `Forked` (a branch's first own record) starts the spend over: the usage logged
//!   before it is the parent's.
//! - `ModelSet` switches the model as the live switch did: it strips another provider's
//!   reasoning, restarts the estimator's calibration on a change of provider, and arms
//!   the one retry without reasoning for a switch within one provider; `ReasoningStripped`
//!   strips all reasoning where the live retry did. A step's `Request` strips reasoning of
//!   another provider than its model's, as every live step start does. The resumed (or
//!   forked) session continues on the model the log ends on (`Session::restore_model`).
//!
//! `rebuild(reader, Some(n))` stops at the end of step `n` (spec 7.12.1: steps 1..=n kept;
//! steps are numbered by the context turn counter, so user messages count too). It cuts
//! only at step boundaries: everything from the first record that belongs to a later
//! step (its user message, an epoch at its start, its request) is left out. The state at
//! the cut is what step `n + 1` would be built from, so pins sent during step `n` are in
//! it (live, they reach the next step start, or the turn's end); a fork's branch closes a
//! cut turn with `TurnEnd{cancelled}`, which applies them in the same place. For the same
//! reason what happened between turns after step `n` (commands such as `Pin` and
//! `SetBudget`, a switch, a manual compaction) is in it, in any order, as at the tip.
//!
//! The read tracker is not rebuilt from the log (what it remembers lives in spill files):
//! a resumed session starts with no re-read baselines, so a re-read returns the full
//! content, and the stale-read guard is restored only for files whose content on disk is
//! still what the session last saw. A fork starts with a fresh tracker.
//!
//! # Undo
//!
//! [`undo`] (re-exported from [`crate::undo`], which has the details) reverts one user
//! turn: its tool edits from their pre-images and, with git checkpoints, its commands'
//! changes too, never overwriting what it did not write.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, PoisonError};

use tautly_context::TokenEstimator;

use tautly_context::pinned::{self, PinSource};
use tautly_context::{
    CompactionRecord, Context, ContextStatus, GcElision, GcRecord, MutationReason, Turn,
};
use tautly_memory::MemoryChange;
use tautly_provider::{
    Capabilities, Catalog, Content, Message, ModelId, RawUsage, Role, ToolSpec, cost_usd,
};
use tautly_session::{BlobRef, ConfineError, HeaderInit, Origin, SessionReader, SessionWriter};
use tautly_tools::{
    AppliedEffect, ContextOp, PersistedEffect, ReadTracker, Tool, ToolResult, registry,
};

use crate::attempts::AttemptLedger;
use crate::budget::Budget;
use crate::dispatch::signature;
use crate::ledger::{LEDGER_FILE, Ledger};
use crate::mcp::MCP_TOOL_PREFIX;
use crate::protocol::{BudgetScope, Command, ErrorKind, Event, FinishReason};
use crate::provider_setup::provider_label;
use crate::record::SessionRecord;
use crate::session::{PrefixLog, Session};
use crate::turn::{Response, TurnControl};

/// The result of a call the log has no result for: the session stopped while it was
/// pending or running.
pub const OPEN_CALL_RESULT: &str = "cancelled: the session stopped before this call returned \
     a result; it may not have run, or may have changed files before it stopped";

/// A session's context rebuilt from its log, with what resuming needs besides.
pub struct Rebuilt {
    pub context: Context,
    /// The log ends (or was cut) inside a user turn: no `TurnEnd` follows its `TurnStart`.
    pub incomplete_turn: bool,
    /// User turns started.
    pub turn_count: u32,
    /// Records that could not be replayed as they were written.
    pub warnings: Vec<String>,
    pub(crate) attempts: AttemptLedger,
    pub(crate) latest_compaction: Option<CompactionRecord>,
    /// The last `PrefixApplied`.
    pub(crate) prefix: Option<SessionRecord>,
    /// Valid `SetBudget` commands, in order.
    pub(crate) budgets: Vec<(BudgetScope, Budget)>,
    /// The usage of every logged response (and of the attempts discarded before it).
    pub(crate) usage: Vec<RawUsage>,
    /// The last record that is part of the state: commands that change nothing (the
    /// `Fork` itself, a message whose turn has not started) are left out after it. None:
    /// no record is.
    pub(crate) end_offset: Option<u64>,
    /// The model the log ends on: the last `ModelSet`'s, else the header's.
    pub(crate) model: Option<ModelId>,
    /// The log ends on a switch within one provider with no request since: the next
    /// request, if refused for its reasoning, is sent again once without it.
    pub(crate) reasoning_retry: bool,
    /// What the session last saw of each file: the hash its last read showed or its last
    /// edit wrote.
    pub(crate) seen: HashMap<PathBuf, blake3::Hash>,
    /// A `StartPins` record was replayed: the session's start pins are the logged ones.
    pub(crate) start_pins: bool,
    /// Memory proposals no decision followed, oldest first.
    pub(crate) memory_proposals: Vec<(String, MemoryChange)>,
    /// Memory proposals logged (their ids are numbered).
    pub(crate) memory_seq: u32,
    /// The git checkpoints the log names.
    pub(crate) checkpoints: LoggedCheckpoints,
}

/// The checkpoints of a log (spec 7.11.11), for the checkpointer that continues it.
#[derive(Debug, Clone, Default)]
pub(crate) struct LoggedCheckpoints {
    /// Every `Checkpoint`'s `(n, commit)`, in order (a fork adopts them).
    pub all: Vec<(u32, String)>,
    /// The highest number used, backups (`undo-<n>`) included.
    pub max_n: u32,
    /// The latest one: `(n, commit, tree)`.
    pub last: Option<(u32, String, String)>,
}

#[derive(Debug, thiserror::Error)]
pub enum ResumeError {
    #[error("cannot read the session log: {0}")]
    Session(#[from] tautly_session::Error),
    #[error("there is no step {at} in this session; its last step is {last}")]
    NoSuchStep { at: u32, last: u32 },
}

pub use crate::undo::{UndoError, UndoReport, undo};

/// What the live loop assembled each request with, so a rebuild calibrates the token
/// estimator as the live session did (its ratio decides when collection and compaction
/// run, and how much of the working state is listed).
pub(crate) struct Calibration {
    /// Each request's model's capabilities come from here, as they did live.
    pub catalog: Arc<Catalog>,
    /// Tools offered besides the built-in ones (MCP).
    pub extra: Vec<Arc<dyn Tool>>,
}

impl Calibration {
    /// What a request on `model` was assembled with.
    fn assembly(&self, model: &ModelId) -> (Capabilities, Vec<ToolSpec>) {
        let caps = self.catalog.get(model).caps;
        let specs = registry(&caps, self.extra.clone())
            .iter()
            .map(|t| t.spec(&caps))
            .collect();
        (caps, specs)
    }
}

/// The context as it stood at the end of step `until` (None: the end of the log). An
/// `until` past the last step is [`ResumeError::NoSuchStep`]. Blocking I/O (the log and
/// its blobs).
pub fn rebuild(
    reader: &SessionReader<SessionRecord>,
    until: Option<u32>,
) -> Result<Rebuilt, ResumeError> {
    rebuild_with(reader, until, None)
}

/// [`rebuild`], calibrating the token estimator when `calibration` is given.
pub(crate) fn rebuild_with(
    reader: &SessionReader<SessionRecord>,
    until: Option<u32>,
    calibration: Option<&Calibration>,
) -> Result<Rebuilt, ResumeError> {
    let root = reader.header().root.clone();
    let model = reader.header().model.clone().map(ModelId::from);
    let mut r = Replay {
        reader,
        until,
        calibration,
        assemblies: HashMap::new(),
        out: Rebuilt {
            context: Context {
                root: Some(root.clone()),
                // As the session started: the history holds its first model's reasoning.
                reasoning_provider: model.as_ref().map(provider_label),
                ..Context::default()
            },
            incomplete_turn: false,
            turn_count: 0,
            warnings: Vec::new(),
            attempts: AttemptLedger::default(),
            latest_compaction: None,
            prefix: None,
            budgets: Vec::new(),
            usage: Vec::new(),
            end_offset: None,
            model,
            reasoning_retry: false,
            checkpoints: LoggedCheckpoints::default(),
            seen: HashMap::new(),
            start_pins: false,
            memory_proposals: Vec::new(),
            memory_seq: 0,
        },
        root,
        step: None,
        running: false,
        started: false,
        pending: Vec::new(),
        results: HashMap::new(),
        logged_tools: None,
    };
    let mut stopped = false;
    for rec in reader.records() {
        let (offset, rec) = rec?;
        match r.apply(rec) {
            Flow::Go => r.out.end_offset = Some(offset),
            Flow::Skip => {}
            Flow::Stop => {
                stopped = true;
                break;
            }
        }
    }
    r.close_step();
    // Pins of a turn the log ends in (or the cut closes) reach the context as its
    // leftovers would have.
    r.apply_pending();
    if !stopped {
        let last = r.out.context.next_turn() - 1;
        if let Some(at) = until.filter(|&at| at > last) {
            return Err(ResumeError::NoSuchStep { at, last });
        }
    }
    r.out.incomplete_turn = r.running;
    Ok(r.out)
}

#[derive(Debug, PartialEq, Eq)]
enum Flow {
    /// Replayed: part of the state.
    Go,
    /// Replayed, but changes nothing (a command such as `Fork` or `SendMessage`).
    Skip,
    /// Past the cut: the replay ends before this record.
    Stop,
}

/// A step being rebuilt: its response so far and the results of its calls.
struct OpenStep {
    number: u32,
    resp: Response,
    responded: bool,
    results: HashMap<String, Content>,
    /// The request as assembled from the rebuilt context (with a calibration).
    status: Option<ContextStatus>,
}

struct Replay<'a> {
    reader: &'a SessionReader<SessionRecord>,
    until: Option<u32>,
    calibration: Option<&'a Calibration>,
    /// What requests were assembled with, per model (with a calibration).
    assemblies: HashMap<ModelId, (Capabilities, Vec<ToolSpec>)>,
    out: Rebuilt,
    root: PathBuf,
    step: Option<OpenStep>,
    /// Between a `TurnStart` and its `TurnEnd`.
    running: bool,
    /// A `TurnStart` was replayed: records before the first one are session set-up, which
    /// no cut leaves out.
    started: bool,
    /// `Pin`/`Unpin` sent while the turn ran, for its next step start.
    pending: Vec<Command>,
    /// The result blob of every call, for an `Uncompacted` restore.
    results: HashMap<String, BlobRef>,
    /// The tools the last `PrefixApplied` logged (with a calibration): the requests after
    /// it were assembled with them. None: assembled from the registry (an older log).
    logged_tools: Option<Vec<ToolSpec>>,
}

impl Replay<'_> {
    fn ctx(&mut self) -> &mut Context {
        &mut self.out.context
    }

    fn warn(&mut self, message: String) {
        self.out.warnings.push(message);
    }

    /// The boundary of `until` lies behind: the next step would be past it.
    fn beyond(&self) -> bool {
        self.until.is_some_and(|n| self.out.context.next_turn() > n)
    }

    fn apply(&mut self, rec: SessionRecord) -> Flow {
        match rec {
            SessionRecord::Provider(ev) => {
                if let Some(step) = &mut self.step {
                    step.resp.push(ev);
                    step.responded = true;
                }
            }
            SessionRecord::ToolResult {
                id,
                result_blob,
                is_error,
                effects,
                meta,
                ran,
                blocked,
            } => self.tool_result(id, result_blob, is_error, &effects, meta, ran, blocked),
            SessionRecord::Command(cmd) => return self.command(cmd),
            SessionRecord::TurnStart { turn, text } => {
                self.close_step();
                if self.beyond() {
                    return Flow::Stop;
                }
                self.ctx().history.push(Turn {
                    messages: vec![Message {
                        role: Role::User,
                        content: vec![Content::Text(text)],
                    }],
                });
                self.out.turn_count = self.out.turn_count.max(turn);
                self.running = true;
                self.started = true;
            }
            SessionRecord::TurnEnd { .. } => {
                self.close_step();
                self.apply_pending();
                self.running = false;
            }
            SessionRecord::MemoryProposal { id, change } => {
                self.out.memory_seq += 1;
                self.out.memory_proposals.push((id, change));
            }
            SessionRecord::MemoryDecision { id, .. } => {
                self.out.memory_proposals.retain(|(p, _)| *p != id);
            }
            SessionRecord::Forked => {
                self.close_step();
                self.out.usage.clear();
            }
            SessionRecord::ModelSet { model } => {
                self.close_step();
                if self.running && self.beyond() {
                    return Flow::Stop;
                }
                self.apply_pending();
                self.switch(model);
            }
            // The refused request's retry is sent without any reasoning.
            SessionRecord::ReasoningStripped => self.ctx().strip_reasoning(None),
            SessionRecord::Request { step, model, .. } => {
                self.close_step();
                if self.beyond() {
                    return Flow::Stop;
                }
                self.apply_pending();
                let next = self.out.context.next_turn();
                if step != next {
                    self.warn(format!(
                        "the log's step {step} was rebuilt as step {next}; later step numbers \
                         may not match the log"
                    ));
                }
                // Every step start strips another provider's reasoning, and takes the
                // switch's retry and forced cold epoch.
                let ctx = &mut self.out.context;
                ctx.strip_reasoning(Some(&provider_label(&model)));
                ctx.cold_epoch_forced = false;
                self.out.reasoning_retry = false;
                let status = self.calibration.map(|c| {
                    let (caps, specs) = self
                        .assemblies
                        .entry(model.clone())
                        .or_insert_with(|| c.assembly(&model));
                    let specs = self.logged_tools.as_ref().unwrap_or(specs);
                    self.out.context.assemble(caps, specs, &model).1
                });
                self.step = Some(OpenStep {
                    number: next,
                    resp: Response::default(),
                    responded: false,
                    results: HashMap::new(),
                    status,
                });
            }
            rec @ (SessionRecord::GcApplied { .. }
            | SessionRecord::Compaction { .. }
            | SessionRecord::PinsFolded
            | SessionRecord::MemoryIndexApplied { .. }
            | SessionRecord::PrefixApplied { .. }
            | SessionRecord::StartPins { .. }
            | SessionRecord::Uncompacted { .. }
            | SessionRecord::Undone { .. }) => {
                self.close_step();
                // An epoch at the start of a step past the cut; one between turns (or
                // before the first) is part of the cut.
                if self.running && self.beyond() {
                    return Flow::Stop;
                }
                self.apply_pending();
                self.mutation(rec);
            }
            SessionRecord::Checkpoint {
                n, commit, tree, ..
            } => {
                let logged = &mut self.out.checkpoints;
                logged.max_n = logged.max_n.max(n);
                logged.all.push((n, commit.clone()));
                logged.last = Some((n, commit, tree));
            }
            SessionRecord::ToolCall { .. }
            | SessionRecord::Edit { .. }
            | SessionRecord::ApprovalRequested { .. }
            | SessionRecord::CheckpointSkipped { .. }
            | SessionRecord::Labeled { .. }
            | SessionRecord::EngineError { .. }
            | SessionRecord::Unknown => {}
        }
        Flow::Go
    }

    fn command(&mut self, cmd: Command) -> Flow {
        match cmd {
            Command::SetBudget {
                scope,
                max_usd,
                max_tokens,
            } if scope != BudgetScope::Unknown
                && max_usd.is_none_or(|usd| usd.is_finite() && usd >= 0.0) =>
            {
                self.out.budgets.push((
                    scope,
                    Budget {
                        max_usd,
                        max_tokens,
                    },
                ));
            }
            cmd @ (Command::Pin { .. } | Command::Unpin { .. }) if self.running => {
                self.pending.push(cmd);
            }
            // Sent between turns: part of what the next step is built from.
            cmd @ (Command::Pin { .. } | Command::Unpin { .. }) => {
                self.close_step();
                context_command(self.ctx(), cmd);
            }
            _ => return Flow::Skip,
        }
        Flow::Go
    }

    /// A `ModelSet`, as `Session::set_model` changed the context.
    fn switch(&mut self, model: ModelId) {
        let label = provider_label(&model);
        let same_provider = self.out.model.as_ref().map(provider_label) == Some(label.clone());
        let ctx = &mut self.out.context;
        ctx.strip_reasoning(Some(&label));
        if !same_provider {
            ctx.tokens = TokenEstimator::default();
        }
        ctx.force_cold_epoch();
        self.out.reasoning_retry = same_provider;
        self.out.model = Some(model);
    }

    fn apply_pending(&mut self) {
        for cmd in std::mem::take(&mut self.pending) {
            context_command(&mut self.out.context, cmd);
        }
    }

    /// An epoch, set-up or user record that changes the context.
    fn mutation(&mut self, rec: SessionRecord) {
        match rec {
            SessionRecord::GcApplied { elided, .. } => {
                let unknown = self.ctx().reapply_gc(&elided);
                if unknown > 0 {
                    self.warn(format!(
                        "{unknown} collected results were not found in the rebuilt history"
                    ));
                }
            }
            SessionRecord::Compaction { removed, summary } => {
                match self.ctx().reapply_compaction(removed.len(), summary) {
                    Ok(rec) => self.out.latest_compaction = Some(rec),
                    Err(e) => self.warn(format!("a logged compaction was not redone: {e}")),
                }
            }
            SessionRecord::PinsFolded => {
                self.ctx().fold_session_pins();
            }
            SessionRecord::MemoryIndexApplied { text } => {
                self.ctx().memory_index = Some(text).filter(|t| !t.trim().is_empty());
            }
            SessionRecord::PrefixApplied {
                agents_md,
                repo_map,
                system_prompt_hash,
                tools_hash,
                tools,
            } => {
                let agents_text = self.text(agents_md.as_ref(), "AGENTS.md");
                let map_text = self.text(repo_map.as_ref(), "repo map");
                self.ctx().agents_md = agents_text;
                self.ctx().repo_map = map_text;
                if self.calibration.is_some() {
                    self.logged_tools = tools.as_ref().and_then(|b| self.tool_specs(b));
                }
                self.out.prefix = Some(SessionRecord::PrefixApplied {
                    agents_md,
                    repo_map,
                    system_prompt_hash,
                    tools_hash,
                    tools,
                });
            }
            SessionRecord::StartPins { pins } => {
                for p in &pins {
                    pinned::add(&mut self.out.context.pinned, p.into());
                }
                self.out.start_pins = true;
            }
            SessionRecord::Uncompacted {
                compaction,
                restored,
            } => {
                if compaction {
                    match self.out.latest_compaction.take() {
                        Some(rec) => {
                            if let Err(e) = self.ctx().uncompact(&rec, u32::MAX) {
                                self.warn(format!("a logged uncompact was not redone: {e}"));
                            }
                        }
                        None => self.warn("a logged uncompact has no compaction to undo".into()),
                    }
                }
                let mut elided = Vec::with_capacity(restored.len());
                for id in restored {
                    let original = match self.results.get(&id).map(|b| self.reader.read_blob(b)) {
                        Some(Ok(bytes)) => bytes,
                        Some(Err(e)) => {
                            self.warn(format!(
                                "the uncompacted result of call {id} could not be read ({e}); \
                                 it stays collected"
                            ));
                            continue;
                        }
                        None => {
                            self.warn(format!(
                                "the uncompacted result of call {id} is not in the log; it \
                                 stays collected"
                            ));
                            continue;
                        }
                    };
                    elided.push(GcElision {
                        call_id: id,
                        original: String::from_utf8_lossy(&original).into_owned(),
                        by_turn: 0,
                    });
                }
                restore_and_keep(self.ctx(), elided);
            }
            SessionRecord::Undone {
                files, backup_ref, ..
            } => {
                if let Some(n) = backup_ref
                    .as_deref()
                    .and_then(|r| r.rsplit_once("/undo-"))
                    .and_then(|(_, n)| n.parse::<u32>().ok())
                {
                    let logged = &mut self.out.checkpoints;
                    logged.max_n = logged.max_n.max(n);
                }
                for f in &files {
                    let Ok(path) = f.path.resolve(&self.root) else {
                        continue;
                    };
                    if let Some(note) = self.out.context.state.files.get_mut(&path) {
                        note.maybe_stale = true;
                    }
                    self.out.seen.remove(&path);
                }
                self.out
                    .attempts
                    .note_effects(&[AppliedEffect::Exec { may_write: true }]);
            }
            _ => {}
        }
    }

    /// The text of a prefix blob (none: absent or unreadable, which is said).
    fn text(&mut self, blob: Option<&BlobRef>, what: &str) -> Option<String> {
        match self.reader.read_blob(blob?) {
            Ok(bytes) => Some(String::from_utf8_lossy(&bytes).into_owned()),
            Err(e) => {
                self.warn(format!(
                    "the logged {what} could not be read ({e}); left out"
                ));
                None
            }
        }
    }

    /// The tool specs of a `PrefixApplied` blob (none: unreadable, which is said; the
    /// requests are then assembled from the registry).
    fn tool_specs(&mut self, blob: &BlobRef) -> Option<Vec<ToolSpec>> {
        let parsed = self
            .reader
            .read_blob(blob)
            .map_err(|e| e.to_string())
            .and_then(|b| serde_json::from_slice::<Vec<ToolSpec>>(&b).map_err(|e| e.to_string()));
        match parsed {
            Ok(specs) => Some(specs),
            Err(e) => {
                self.warn(format!(
                    "the logged tools could not be read ({e}); the estimate is rebuilt with \
                     this version's"
                ));
                None
            }
        }
    }

    /// A result of the open step, replayed as the live loop settled it.
    #[allow(clippy::too_many_arguments)]
    fn tool_result(
        &mut self,
        id: String,
        blob: BlobRef,
        is_error: bool,
        effects: &[PersistedEffect],
        meta: tautly_tools::ToolMeta,
        ran: bool,
        blocked: bool,
    ) {
        let Some(call) = self.step.as_ref().and_then(|s| {
            s.resp
                .calls
                .iter()
                .find(|c| c.id == id)
                .map(|c| (c.name.clone(), c.input()))
        }) else {
            self.warn(format!(
                "the result of call {id} has no call before it; left out"
            ));
            return;
        };
        let (name, args) = call;
        let output = match self.reader.read_blob(&blob) {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(e) => {
                self.warn(format!("the result of call {id} could not be read ({e})"));
                format!("[this result could not be read back from the session log: {e}]")
            }
        };
        self.results.insert(id.clone(), blob);
        // Only paths and hashes are replayed; the file contents stay in their blobs.
        let mut applied = Vec::with_capacity(effects.len());
        for e in effects {
            match e.to_applied(&self.root, &mut |_| Ok::<_, ConfineError>(Vec::new())) {
                Ok(a) => applied.push(a),
                Err(err) => self.warn(format!("an effect of call {id} was left out: {err}")),
            }
        }
        let number = self.step.as_ref().map_or(0, |s| s.number);
        let r = &mut self.out;
        // Live, every call that ran or was blocked went through `check` first, which also
        // spends the escape hatch. (A call the permission policy denied did too; the log
        // does not say which those were.)
        if ran || blocked {
            r.attempts.check(&name, &args);
        }
        if ran {
            if !is_error
                && let Some(op) = tautly_tools::parse_context_op(&name, &args)
                && !matches!(op, ContextOp::Remember(_))
            {
                let _ = r.context.apply_op(op);
            }
            r.attempts.note_effects(&applied);
            let result = ToolResult {
                output: output.clone(),
                is_error,
                effects: Vec::new(),
                meta,
                applied_diff: None,
            };
            r.attempts.record(
                &name,
                &args,
                number,
                !is_error,
                &signature(&name, &result),
                &output,
            );
        }
        for e in &applied {
            match e {
                AppliedEffect::Read {
                    path, file_hash, ..
                } => {
                    r.seen.insert(path.clone(), *file_hash);
                }
                AppliedEffect::Edited {
                    path, post_hash, ..
                } => {
                    r.seen.insert(path.clone(), *post_hash);
                }
                AppliedEffect::Deleted { path, .. } => {
                    r.seen.remove(path);
                }
                AppliedEffect::Exec { .. } => {}
            }
        }
        r.context.apply_effects(&applied, number);
        r.context
            .push_tool_result(&id, &name, &args, &applied, is_error, number);
        if let Some(step) = &mut self.step {
            step.results.insert(
                id.clone(),
                Content::ToolResult {
                    id,
                    output,
                    is_error,
                },
            );
        }
    }

    /// The open step is over: push its turn, as the live loop did once its calls ran.
    fn close_step(&mut self) {
        let Some(mut step) = self.step.take() else {
            return;
        };
        if !step.responded {
            // The request got no complete response: the step left nothing behind.
            return;
        }
        let resp = step.resp.finish();
        let r = &mut self.out;
        if let Some(u) = resp.usage {
            if let Some(status) = &step.status {
                r.context.calibrate(status, &u);
            }
            r.usage.push(u);
        }
        r.usage.extend(resp.discarded.iter().copied());
        let assistant = Message {
            role: Role::Assistant,
            content: resp.content,
        };
        if resp.calls.is_empty() {
            if !assistant.content.is_empty() {
                r.context.history.push(Turn {
                    messages: vec![assistant],
                });
            }
            return;
        }
        let mut results = Vec::with_capacity(resp.calls.len());
        for c in &resp.calls {
            match step.results.remove(&c.id) {
                Some(content) => results.push(content),
                None => {
                    r.context
                        .push_tool_result(&c.id, &c.name, &c.input(), &[], true, step.number);
                    results.push(Content::ToolResult {
                        id: c.id.clone(),
                        output: OPEN_CALL_RESULT.to_string(),
                        is_error: true,
                    });
                }
            }
        }
        r.context.history.push(Turn {
            messages: vec![
                assistant,
                Message {
                    role: Role::User,
                    content: results,
                },
            ],
        });
        r.context.state.failed_attempts = r.attempts.listed();
    }
}

/// `Pin`/`Unpin` as the session applies them (refusals change nothing).
fn context_command(ctx: &mut Context, cmd: Command) {
    match cmd {
        Command::Pin { text } => {
            let _ = ctx.pin(text, PinSource::User);
        }
        Command::Unpin { text } => {
            ctx.pinned.retain(|p| p.text != text);
            ctx.session_pins.retain(|p| p.text != text);
        }
        _ => {}
    }
}

/// Put collected outputs back and keep them from being collected again for what
/// superseded them so far (a later turn's result can); the call ids restored.
fn restore_and_keep(ctx: &mut Context, elided: Vec<GcElision>) -> Vec<String> {
    let now = ctx.next_turn().saturating_sub(1);
    let ids: HashSet<String> = elided.iter().map(|e| e.call_id.clone()).collect();
    ctx.restore_gc(&GcRecord {
        elided,
        tokens_freed: 0,
        reason: MutationReason::Manual,
        forgotten_paths: Vec::new(),
    });
    let mut restored = Vec::new();
    for meta in ctx.results.iter_mut() {
        if ids.contains(&meta.call_id) && !meta.elided {
            meta.kept_until = Some(now);
            restored.push(meta.call_id.clone());
        }
    }
    restored
}

/// The original outputs of `ids`, from their `ToolResult` blobs in the log at `path`.
fn originals(path: &std::path::Path, ids: &[String]) -> Result<Vec<GcElision>, ResumeError> {
    let reader = SessionReader::<SessionRecord>::open(path)?;
    let wanted: HashSet<&str> = ids.iter().map(String::as_str).collect();
    let mut blobs: HashMap<String, BlobRef> = HashMap::new();
    for rec in reader.records() {
        if let (
            _,
            SessionRecord::ToolResult {
                id, result_blob, ..
            },
        ) = rec?
            && wanted.contains(id.as_str())
        {
            blobs.insert(id, result_blob);
        }
    }
    let mut out = Vec::with_capacity(blobs.len());
    for id in ids {
        if let Some(blob) = blobs.get(id) {
            out.push(GcElision {
                call_id: id.clone(),
                original: String::from_utf8_lossy(&reader.read_blob(blob)?).into_owned(),
                by_turn: 0,
            });
        }
    }
    Ok(out)
}

/// Reopen the log of session `id` in the work tree `root` for resuming: the writer lock
/// is taken and a torn tail cut off, and every record is checked. The rebuild follows in
/// the session's actor ([`Session::resume`]). Only a session of this very work tree is
/// found ("continue here").
pub(crate) fn open(
    data_home: &std::path::Path,
    root: &std::path::Path,
    id: &str,
) -> Result<SessionWriter<SessionRecord>, OpenError> {
    let found = tautly_session::list_here_in(data_home, root)?
        .into_iter()
        .find(|s| s.id == id)
        .ok_or_else(|| OpenError::NotFound(id.to_string()))?;
    Ok(SessionWriter::<SessionRecord>::reopen(&found.path)?)
}

/// Why [`open`] failed.
#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    #[error("no session {0} ran in this work tree")]
    NotFound(String),
    #[error("{0}")]
    Session(#[from] tautly_session::Error),
}

/// What a resumed session still has to do once it took the rebuilt state.
pub(crate) struct Resumed {
    /// The model the log ends on.
    model: Option<ModelId>,
    prefix: Option<SessionRecord>,
    usage: Vec<RawUsage>,
    seen: HashMap<PathBuf, blake3::Hash>,
    warnings: Vec<String>,
    start_pins: bool,
}

impl Session {
    /// Session start of a resumed session (in the actor, after the first event; in place
    /// of [`Session::init`]): rebuild the context from the log, off the async workers,
    /// and close a turn the log ends in (a crash) with its missing `TurnEnd` before
    /// anything else is written; then take the rebuilt state and restore the rest
    /// ([`resume_start`](Self::resume_start)). An error leaves the session unusable.
    pub(crate) async fn resume(
        &mut self,
        config_pins: &[String],
        stop: impl std::future::Future<Output = ()>,
    ) -> Result<(), String> {
        let path = self.log.path();
        let log = self.log.clone();
        let calibration = self.calibration();
        let (rebuilt, closed) = tokio::task::spawn_blocking(move || -> Result<_, String> {
            let reader = SessionReader::<SessionRecord>::open(&path).map_err(err)?;
            let rebuilt = rebuild_with(&reader, None, Some(&calibration)).map_err(err)?;
            let closed = if rebuilt.incomplete_turn {
                log.append(&SessionRecord::TurnEnd {
                    reason: FinishReason::Error,
                    evidence: None,
                })
                .and_then(|()| log.checkpoint())
                .err()
            } else {
                None
            };
            Ok((rebuilt, closed))
        })
        .await
        .map_err(err)??;
        if let Some(failure) = closed {
            self.report(failure).await;
        }
        let logged = rebuilt.checkpoints.clone();
        if let Some(c) = &self.checkpoints {
            c.lock().await.cp.continue_after(logged.max_n, logged.last);
        }
        let mut resumed = self.adopt(rebuilt);
        if let Some(model) = resumed.model.take() {
            self.restore_model(model, stop).await;
        }
        self.resume_start(resumed, config_pins).await;
        self.show_pending_proposals().await;
        Ok(())
    }

    /// What a rebuild calibrates the estimator with: this session's catalog and its MCP
    /// tools.
    fn calibration(&self) -> Calibration {
        Calibration {
            catalog: Arc::clone(&self.setup.catalog),
            extra: self
                .tools
                .iter()
                .filter(|t| t.name().starts_with(MCP_TOOL_PREFIX))
                .cloned()
                .collect(),
        }
    }

    /// Take the state `r` rebuilt (a resume or a fork) in place of the session's own (the
    /// model it ends on is the caller's: [`restore_model`](Self::restore_model)). The
    /// read tracker starts fresh. Without a logged prefix (no request was sent yet) the
    /// session keeps its own AGENTS.md text and repo map.
    pub(crate) fn adopt(&mut self, r: Rebuilt) -> Resumed {
        let mut context = r.context;
        if r.prefix.is_none() {
            context.agents_md = self.context.agents_md.take();
            context.repo_map = self.context.repo_map.take();
            context.load_warnings = std::mem::take(&mut self.context.load_warnings);
        }
        context.root = Some(self.root().to_path_buf());
        self.context = context;
        self.ledger = r.attempts;
        self.latest_compaction = r.latest_compaction;
        self.user_turn = r.turn_count;
        self.prefix_log = PrefixLog::default();
        if let Some(p) = r.prefix.clone() {
            self.prefix_log.logged(p);
        }
        self.reasoning_retry = r.reasoning_retry;
        self.memory.pending = r.memory_proposals;
        self.memory.seq = r.memory_seq;
        self.memory.task = None;
        if let Some(text) = crate::memory::first_task(&self.context.history) {
            self.memory.note_task(&text);
        }
        self.reads = Arc::new(ReadTracker::default());
        self.evidence = self.evidence.fresh();
        {
            let mut b = self.budgets.lock().unwrap_or_else(PoisonError::into_inner);
            for (scope, limit) in &r.budgets {
                b.set(*scope, *limit);
            }
        }
        Resumed {
            model: r.model,
            prefix: r.prefix,
            usage: r.usage,
            seen: r.seen,
            warnings: r.warnings,
            start_pins: r.start_pins,
        }
    }

    /// The rest of a resumed session's start: the spend so far, the stale-read guard of
    /// files unchanged since, and (at the first request, once the MCP tools have joined) a
    /// warning when this build's system prompt or tools differ from the logged ones. AGENTS.md and the repo map are the logged ones; a
    /// session that never sent a request starts up as a new one, keeping the start pins
    /// it logged.
    async fn resume_start(&mut self, resumed: Resumed, config_pins: &[String]) {
        for w in resumed.warnings {
            self.warn(format!("resume: {w}")).await;
        }
        match &resumed.prefix {
            None => self.init_with(config_pins, resumed.start_pins).await,
            Some(_) => {
                self.start_housekeeping();
                // Compared at the first request, once the MCP tools have joined.
                self.prefix_log.check_next();
                // The logged index stands until the store is checked again (once the code
                // index is ready), which an epoch then applies.
                self.memory.live = true;
                self.memory.refresh_due = true;
            }
        }
        self.restore_spend(resumed.usage).await;
        self.restore_guard(resumed.seen).await;
    }

    /// Count what the session spent before against its budgets: the cost ledger's rows of
    /// this session (discarded attempts included), else the usage in the log.
    async fn restore_spend(&mut self, logged: Vec<RawUsage>) {
        let path = self.data_home.join(LEDGER_FILE);
        let id = self.session_id.clone();
        let totals = tokio::task::spawn_blocking(move || {
            Ledger::open(&path).and_then(|l| l.session_totals(&id))
        })
        .await;
        let (usage, usd) = match totals {
            Ok(Ok(t)) if t.requests > 0 || logged.is_empty() => (
                RawUsage {
                    input: t.input,
                    output: t.output,
                    cache_read: t.cache_read,
                    cache_write: t.cache_write,
                },
                t.usd,
            ),
            other => {
                let why = match other {
                    Ok(Err(e)) => e.to_string(),
                    Ok(Ok(_)) => "it has no rows of this session".to_string(),
                    Err(e) => e.to_string(),
                };
                self.warn(format!(
                    "the cost ledger could not give this session's spend ({why}); it is \
                     rebuilt from the session log, which lacks responses that broke off"
                ))
                .await;
                let mut sum = RawUsage::default();
                let mut usd = 0.0;
                for u in &logged {
                    sum.input += u.input;
                    sum.output += u.output;
                    sum.cache_read += u.cache_read;
                    sum.cache_write += u.cache_write;
                    usd += cost_usd(u, self.info.price.as_ref()).unwrap_or(0.0);
                }
                (sum, usd)
            }
        };
        let mut b = self.budgets.lock().unwrap_or_else(PoisonError::into_inner);
        b.spend(&usage, Some(usd));
        b.start_turn();
    }

    /// The stale-read guard of every file whose content is still what the session last
    /// saw, so the model can edit it without reading it again.
    async fn restore_guard(&self, seen: HashMap<PathBuf, blake3::Hash>) {
        for (path, hash) in seen {
            let (Ok(meta), Ok(bytes)) = (
                self.sandbox.metadata(&path).await,
                self.sandbox.read(&path).await,
            ) else {
                continue;
            };
            if blake3::hash(&bytes) == hash {
                self.reads.refresh(&path, &meta, hash);
            }
        }
    }

    /// `Command::Fork`, as a job the client can cancel while it waits for the
    /// session-start checkpoint (the branch copies the log) and while the branch's model is
    /// set up. Answered with `SessionChanged` (after `on_branch` is told the branch's id and
    /// log) or an `Error`.
    pub(crate) async fn fork_job(
        &mut self,
        at: Option<u32>,
        label: Option<String>,
        ctl: &TurnControl,
        on_branch: impl FnOnce(&str, &std::path::Path),
    ) {
        let unbound = self.events.clone();
        self.events = unbound.bound(ctl.cancel.clone());
        let made =
            if self.settle_start_checkpoint_or(&ctl.cancel).await && !ctl.cancel.is_cancelled() {
                self.fork(at, label, ctl.cancel.cancelled()).await
            } else {
                Err("cancelled; no branch was made".to_string())
            };
        match made {
            Ok((id, path)) => {
                on_branch(&id, &path);
                self.emit(Event::SessionChanged { session_id: id }).await;
            }
            Err(why) => {
                self.emit(Event::Error {
                    kind: ErrorKind::Internal,
                    message: format!("fork: {why}"),
                    retryable: false,
                    hint: None,
                })
                .await;
            }
        }
        self.events = unbound;
    }

    /// `Command::Fork`: a new session whose log is this one's up to the end of step `at`
    /// (default: the tip), copied verbatim, with `origin: fork`, `label` and the parent
    /// `(id, offset of the step's end)`; the session continues on it with the context
    /// rebuilt from it, a fresh read tracker and nothing spent. A cut turn is closed in the
    /// branch with `TurnEnd{cancelled}`. Files are not touched. Returns the branch's id and
    /// log.
    pub(crate) async fn fork(
        &mut self,
        at: Option<u32>,
        label: Option<String>,
        stop: impl std::future::Future<Output = ()>,
    ) -> Result<(String, PathBuf), String> {
        self.checkpoint().await;
        let parent_path = self.log.path();
        let parent_id = self.session_id.clone();
        let data_home = self.data_home.clone();
        let root = self.root().to_path_buf();
        let init = HeaderInit {
            label,
            origin: Origin::Fork,
            ..self.header_init.clone()
        };
        let calibration = self.calibration();
        let made = tokio::task::spawn_blocking(move || -> Result<_, String> {
            let parent = SessionReader::<SessionRecord>::open(&parent_path).map_err(err)?;
            let cut = rebuild(&parent, at).map_err(err)?;
            // The parent offset names the last record copied: with none, a branch would
            // read as one cut after record 0.
            let end = cut.end_offset.ok_or_else(|| {
                "there is nothing to branch from yet: the session has no records before \
                 that point"
                    .to_string()
            })?;
            // The branch's records start from the parent's first model.
            let init = HeaderInit {
                parent: Some((parent_id, end)),
                model: parent.header().model.clone(),
                ..init
            };
            let child = branch(
                &data_home,
                &root,
                init,
                &parent_path,
                end + 1,
                cut.incomplete_turn,
            )?;
            let rebuilt = SessionReader::<SessionRecord>::open(child.path())
                .map_err(ResumeError::from)
                .and_then(|reader| rebuild_with(&reader, None, Some(&calibration)));
            match rebuilt {
                Ok(rebuilt) => Ok((child, rebuilt)),
                Err(e) => {
                    discard(child);
                    Err(e.to_string())
                }
            }
        })
        .await
        .map_err(err)??;
        let (child, rebuilt) = made;
        let id = child.session_id().to_string();
        let path = child.path().to_path_buf();
        self.log.replace(child);
        self.session_id = id.clone();
        self.adopt_checkpoints(&id, &rebuilt.checkpoints).await;
        self.last_request_path = crate::engine::last_request_path(&path);
        let fresh = self
            .budgets
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .without_spend();
        *self.budgets.lock().unwrap_or_else(PoisonError::into_inner) = fresh;
        let mut resumed = self.adopt(rebuilt);
        for w in std::mem::take(&mut resumed.warnings) {
            self.warn(format!("fork: {w}")).await;
        }
        if let Some(model) = resumed.model {
            self.restore_model(model, stop).await;
        }
        Ok((id, path))
    }

    /// A branch's own refs for the checkpoints its log copied (spec 7.11.11), and its
    /// later checkpoints under its own id.
    async fn adopt_checkpoints(&self, branch: &str, logged: &LoggedCheckpoints) {
        let Some(c) = &self.checkpoints else {
            return;
        };
        let mut st = c.lock().await;
        let adopted = st.cp.adopt(branch, &logged.all).await;
        // The branch reuses only a checkpoint it has a ref for: its log's latest.
        let last = adopted.is_ok().then(|| logged.last.clone()).flatten();
        st.cp.branch(branch.to_string(), logged.max_n, last);
        drop(st);
        if let Err(e) = adopted {
            self.warn(format!(
                "fork: the branch's checkpoint refs could not be made ({e}); deleting the \
                 parent session can make its checkpoints unavailable to the branch"
            ))
            .await;
        }
    }

    /// `Command::Compact` (between turns, as a task the user can cancel): collect and
    /// compact now, as one epoch, reason `manual`.
    pub(crate) async fn compact_command(&mut self, ctl: &TurnControl) {
        // Its answer waits for room until the user cancels (as a turn's events do).
        let unbound = self.events.clone();
        self.events = unbound.bound(ctl.cancel.clone());
        self.compact_now(ctl).await;
        self.events = unbound;
    }

    async fn compact_now(&mut self, ctl: &TurnControl) {
        let mut applied = false;
        if let Some(rec) = self.context.collect(Some(MutationReason::Manual)) {
            self.collected(rec).await;
            applied = true;
        }
        applied |= self.compact(MutationReason::Manual, ctl).await;
        if ctl.cancel.is_cancelled() {
            self.warn("compaction cancelled: the history is not compacted")
                .await;
        } else if !applied {
            self.warn("nothing to compact: the history is too short")
                .await;
        }
        if !applied {
            return;
        }
        if let Some(text) = self.pending_memory_index.take() {
            self.context.memory_index = Some(text.clone()).filter(|t| !t.trim().is_empty());
            self.log(SessionRecord::MemoryIndexApplied { text }).await;
        }
        if self.context.fold_session_pins() {
            self.log(SessionRecord::PinsFolded).await;
        }
        self.checkpoint().await;
        let (_, status) = self.assemble();
        self.emit(Event::context_status(&status)).await;
    }

    /// `Command::Uncompact` (between turns): undo the latest compaction if the context
    /// then fits the window (less the tools and the reply), then restore every collected
    /// result (from its logged blob) if that fits too. Restored results are not
    /// collected again. Logged as `Uncompacted`.
    pub(crate) async fn uncompact_command(&mut self) {
        self.checkpoint().await;
        let elided: Vec<String> = self
            .context
            .results
            .iter()
            .filter(|r| r.elided)
            .map(|r| r.call_id.clone())
            .collect();
        let path = self.log.path();
        let originals = tokio::task::spawn_blocking(move || originals(&path, &elided)).await;
        let originals = match originals {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => {
                self.warn(format!("uncompact: {e}")).await;
                Vec::new()
            }
            Err(e) => {
                self.warn(format!("uncompact: {e}")).await;
                Vec::new()
            }
        };
        let room = |s: &Session| {
            let (_, status) = s.assemble();
            let budget = status
                .budget
                .saturating_sub(status.breakdown.tools)
                .saturating_sub(s.info.caps.max_output);
            (status.used.saturating_sub(status.breakdown.tools), budget)
        };
        let mut compaction = false;
        if let Some(rec) = self.latest_compaction.take() {
            let (_, budget) = room(self);
            match self.context.uncompact(&rec, budget) {
                Ok(()) => compaction = true,
                Err(e) => {
                    self.warn(format!("uncompact: {e}")).await;
                    self.latest_compaction = Some(rec);
                }
            }
        }
        let extra: u32 = originals
            .iter()
            .map(|e| self.context.tokens.estimate(&e.original))
            .fold(0, u32::saturating_add);
        let (used, budget) = room(self);
        let restored = if originals.is_empty() {
            Vec::new()
        } else if used.saturating_add(extra) > budget {
            self.warn(format!(
                "uncompact: restoring the collected results needs about {extra} more tokens, \
                 over the window; they stay collected"
            ))
            .await;
            Vec::new()
        } else {
            restore_and_keep(&mut self.context, originals)
        };
        if !compaction && restored.is_empty() {
            self.warn("uncompact: nothing to restore").await;
            return;
        }
        self.log(SessionRecord::Uncompacted {
            compaction,
            restored,
        })
        .await;
        self.checkpoint().await;
        let (_, status) = self.assemble();
        self.emit(Event::context_status(&status)).await;
    }
}

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// A fork's log: the first `count` records of the log at `parent`, copied verbatim, then
/// `Forked` (what follows is the branch's own) and a `TurnEnd{cancelled}` when the copy
/// ends inside a turn (`close_turn`). On a failure the half-written log is removed, so no
/// broken branch is listed.
fn branch(
    data_home: &std::path::Path,
    root: &std::path::Path,
    init: HeaderInit,
    parent: &std::path::Path,
    count: u64,
    close_turn: bool,
) -> Result<SessionWriter<SessionRecord>, String> {
    let mut child =
        SessionWriter::<SessionRecord>::create_in(data_home, root, init).map_err(err)?;
    let written = (|| {
        child.copy_from(parent, count)?;
        child.append(&SessionRecord::Forked)?;
        if close_turn {
            child.append(&SessionRecord::TurnEnd {
                reason: FinishReason::Cancelled,
                evidence: None,
            })?;
        }
        child.checkpoint()
    })();
    match written {
        Ok(()) => Ok(child),
        Err(e) => {
            discard(child);
            Err(e.to_string())
        }
    }
}

/// Close and remove a branch's log that could not be completed.
fn discard(child: SessionWriter<SessionRecord>) {
    let path = child.path().to_path_buf();
    drop(child);
    let _ = std::fs::remove_file(path);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every `.jsonl` file below `dir`.
    fn logs(dir: &std::path::Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                out.extend(logs(&path));
            } else if path.extension().is_some_and(|x| x == "jsonl") {
                out.push(path);
            }
        }
        out
    }

    #[test]
    fn a_failed_branch_leaves_no_log_behind() {
        let home = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let missing = root.path().join("no-such-parent.jsonl");
        let made = branch(
            home.path(),
            root.path(),
            HeaderInit::default(),
            &missing,
            3,
            false,
        );
        assert!(made.is_err());
        assert_eq!(logs(home.path()), Vec::<PathBuf>::new());
    }
}
