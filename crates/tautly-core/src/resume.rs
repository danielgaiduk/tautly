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
//!   read from disk again), `StartPins`, `Uncompacted` and `Undone` are redone where they
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
//! [`undo`] restores the files Tautly's tools changed in one user turn to their content
//! (and mode) before it, newest change first, deleting files the turn created, and logs
//! `Undone` with each file's content just before (H7), so the undone state can be
//! restored in turn. It never overwrites what it did not write (spec 7.12): a file whose
//! content or mode differs from what the log says it is now (a human edit or `chmod`, or
//! a command) is [`UndoError::Changed`], and a file a later turn changed is
//! [`UndoError::LaterTurns`] until that turn is undone; either way nothing is written. A
//! file already back at its content before the turn is logged as undone without a write,
//! so the turns before it can be undone in turn.

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
use tautly_sandbox::{Sandbox, SandboxError};
use tautly_session::{
    BlobRef, ConfineError, HeaderInit, Origin, RelPath, SessionReader, SessionWriter,
};
use tautly_tools::{
    AppliedEffect, ContextOp, PersistedEffect, ReadTracker, Tool, ToolResult, registry,
};

use crate::attempts::AttemptLedger;
use crate::budget::Budget;
use crate::dispatch::signature;
use crate::ledger::{LEDGER_FILE, Ledger};
use crate::mcp::MCP_TOOL_PREFIX;
use crate::protocol::{BudgetScope, Command, Event, FinishReason};
use crate::provider_setup::provider_label;
use crate::record::{SessionRecord, UndoneFile};
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
}

#[derive(Debug, thiserror::Error)]
pub enum ResumeError {
    #[error("cannot read the session log: {0}")]
    Session(#[from] tautly_session::Error),
    #[error("there is no step {at} in this session; its last step is {last}")]
    NoSuchStep { at: u32, last: u32 },
}

#[derive(Debug, thiserror::Error)]
pub enum UndoError {
    #[error("cannot read or write the session log: {0}")]
    Session(#[from] tautly_session::Error),
    #[error("there is no turn {turn}; this session has {turns}")]
    NoSuchTurn { turn: u32, turns: u32 },
    #[error("a path in the session log cannot be used here: {0}")]
    Path(#[from] ConfineError),
    #[error("{path}: {source}")]
    Disk { path: String, source: SandboxError },
    #[error("changed since the turn: {}", list(paths))]
    Changed { paths: Vec<PathBuf> },
    #[error("changed by later turns: {}", list(paths))]
    LaterTurns {
        turns: Vec<u32>,
        paths: Vec<PathBuf>,
    },
}

fn list(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

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
            SessionRecord::ToolCall { .. }
            | SessionRecord::Edit { .. }
            | SessionRecord::ApprovalRequested { .. }
            | SessionRecord::Checkpoint { .. }
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
            } if max_usd.is_none_or(|usd| usd.is_finite() && usd >= 0.0) => {
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
            } => {
                let agents_text = self.text(agents_md.as_ref(), "AGENTS.md");
                let map_text = self.text(repo_map.as_ref(), "repo map");
                self.ctx().agents_md = agents_text;
                self.ctx().repo_map = map_text;
                self.out.prefix = Some(SessionRecord::PrefixApplied {
                    agents_md,
                    repo_map,
                    system_prompt_hash,
                    tools_hash,
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
            SessionRecord::Undone { files, .. } => {
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
    pub(crate) async fn resume(&mut self, config_pins: &[String]) -> Result<(), String> {
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
        let mut resumed = self.adopt(rebuilt);
        if let Some(model) = resumed.model.take() {
            self.restore_model(model).await;
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
    /// files unchanged since, and a warning when this build's system prompt or tools
    /// differ from the logged ones. AGENTS.md and the repo map are the logged ones; a
    /// session that never sent a request starts up as a new one, keeping the start pins
    /// it logged.
    async fn resume_start(&mut self, resumed: Resumed, config_pins: &[String]) {
        for w in resumed.warnings {
            self.warn(format!("resume: {w}")).await;
        }
        match &resumed.prefix {
            None => self.init_with(config_pins, resumed.start_pins).await,
            Some(prefix) => {
                self.start_housekeeping();
                self.warn_if_prefix_changed(prefix).await;
                // The logged index stands until the store is checked again (once the code
                // index is ready), which an epoch then applies.
                self.memory.live = true;
                self.memory.refresh_due = true;
            }
        }
        self.restore_spend(resumed.usage).await;
        self.restore_guard(resumed.seen).await;
    }

    async fn warn_if_prefix_changed(&self, logged: &SessionRecord) {
        let (req, _) = self.assemble();
        let due = PrefixLog::default().due(
            self.context.agents_md.as_deref(),
            self.context.repo_map.as_deref(),
            &req,
        );
        let (
            Some((
                SessionRecord::PrefixApplied {
                    system_prompt_hash: system,
                    tools_hash: tools,
                    ..
                },
                _,
            )),
            SessionRecord::PrefixApplied {
                system_prompt_hash: logged_system,
                tools_hash: logged_tools,
                ..
            },
        ) = (due, logged)
        else {
            return;
        };
        let what = match (system != *logged_system, tools != *logged_tools) {
            (true, true) => "system prompt and tools differ",
            (true, false) => "system prompt differs",
            (false, true) => "tools differ",
            (false, false) => return,
        };
        self.warn(format!(
            "this version's {what} from the ones this session was logged with: the \
             provider's prompt cache will be cold for the next request"
        ))
        .await;
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
            self.restore_model(model).await;
        }
        Ok((id, path))
    }

    /// `Command::Compact` (between turns, as a task the user can cancel): collect and
    /// compact now, as one epoch, reason `manual`.
    pub(crate) async fn compact_command(&mut self, ctl: &TurnControl) {
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

/// One file to undo: its log path, what it is restored to (none: deleted), and what the
/// log says its content is now (blake3 hex; none: absent) and its mode (none: unknown, as
/// for a file a tool created).
struct Target {
    path: RelPath,
    pre: Option<BlobRef>,
    mode: Option<u32>,
    tip: Option<String>,
    tip_mode: Option<u32>,
}

/// Undo the file changes Tautly's tools made in user turn `turn` (default: the last one
/// that started): each file the turn changed gets the content and mode it had before the
/// turn's first change of it, newest change first; a file the turn created is deleted.
/// Each file's content just before the undo is stored as a blob first, and the undo is
/// logged as `Undone`, so it can itself be reverted. Changes made by commands are not
/// undone. A file already as it was before the turn (reverted by hand, say) is not written
/// but is listed in `Undone` too, so earlier turns that changed it can be undone next.
/// Returns the paths written (none: nothing needed a write; nothing is logged when nothing
/// was left to undo either).
///
/// Nothing the undo did not write is overwritten (spec 7.12), and every check comes before
/// the first write, so a refusal writes nothing:
/// - [`UndoError::LaterTurns`]: a later turn changed one of the files and is not undone;
///   undo the later turns first (newest first).
/// - [`UndoError::Changed`]: a file's content or mode is not what the log says the session
///   left it at (the last tool edit or delete, or undo, of it): a human edit or `chmod`, or
///   a command. (Tools keep a file's mode, so an edit leaves the mode it found.)
///
/// The log is reopened for writing, so the session must not be running in an engine
/// ([`tautly_session::Error::AlreadyOpen`]). Blocking file I/O on the log.
pub async fn undo(
    reader: &SessionReader<SessionRecord>,
    sandbox: &dyn Sandbox,
    turn: Option<u32>,
) -> Result<Vec<PathBuf>, UndoError> {
    let plan = undo_plan(reader, turn)?;
    let turn = plan.turn;
    if !plan.later.is_empty() {
        return Err(UndoError::LaterTurns {
            turns: plan.later_turns,
            paths: plan
                .later
                .iter()
                .map(|p| p.resolve(sandbox.root()))
                .collect::<Result<_, _>>()?,
        });
    }
    let mut writer = SessionWriter::<SessionRecord>::reopen(reader.path())?;
    let disk = |path: &PathBuf, source: SandboxError| UndoError::Disk {
        path: path.display().to_string(),
        source,
    };
    // First what every file is now, checked against what the session left it at.
    let mut planned = Vec::with_capacity(plan.targets.len());
    let mut already = Vec::new();
    let mut changed = Vec::new();
    for t in plan.targets {
        let full = t.path.resolve(sandbox.root())?;
        let now = match sandbox.read(&full).await {
            Ok(bytes) => Some(bytes),
            Err(SandboxError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(disk(&full, e)),
        };
        let now_mode = match &now {
            Some(_) => Some(
                sandbox
                    .metadata(&full)
                    .await
                    .map_err(|e| disk(&full, e))?
                    .mode
                    & 0o7777,
            ),
            None => None,
        };
        let to = t.pre.as_ref().map(|b| reader.read_blob(b)).transpose()?;
        if now == to && (to.is_none() || now_mode == t.mode) {
            // Nothing to write; logged as undone all the same (its content is the blob
            // it is restored to).
            already.push(UndoneFile {
                path: t.path,
                from_blob: t.pre.clone(),
                from_mode: now_mode,
                to_mode: to.as_ref().and(t.mode),
                to: t.pre,
            });
            continue;
        }
        let now_hash = now.as_deref().map(|b| blake3::hash(b).to_hex().to_string());
        let mode_kept = now.is_none() || t.tip_mode.is_none() || now_mode == t.tip_mode;
        if now_hash != t.tip || !mode_kept {
            changed.push(full);
            continue;
        }
        planned.push((t, full, now, to, now_mode));
    }
    if !changed.is_empty() {
        changed.sort();
        return Err(UndoError::Changed { paths: changed });
    }
    if planned.is_empty() && already.is_empty() {
        return Ok(Vec::new());
    }
    // Then their current content, as blobs.
    let mut blobbed = Vec::with_capacity(planned.len());
    for (t, full, now, to, now_mode) in planned {
        let from_blob = now.as_deref().map(|b| writer.blob(b)).transpose()?;
        blobbed.push((t, full, to, from_blob, now_mode));
    }
    let mut files = already;
    let mut done = Vec::with_capacity(blobbed.len());
    let mut failed = None;
    for (t, full, to, from_blob, from_mode) in blobbed {
        let written = match &to {
            Some(bytes) => sandbox.write_atomic(&full, bytes, t.mode).await,
            None if from_blob.is_some() => sandbox.remove_file(&full).await,
            None => Ok(()),
        };
        if let Err(e) = written {
            failed = Some(disk(&full, e));
            break;
        }
        files.push(UndoneFile {
            path: t.path,
            from_blob,
            from_mode,
            to: t.pre,
            to_mode: to.as_ref().and(t.mode),
        });
        done.push(full);
    }
    writer.append(&SessionRecord::Undone {
        turn,
        files,
        backup_ref: None,
    })?;
    writer.checkpoint()?;
    match failed {
        Some(e) => Err(e),
        None => Ok(done),
    }
}

/// What [`undo`] does for one user turn.
struct UndoPlan {
    turn: u32,
    /// Its files, newest change first; changes an earlier undo of the turn reverted are
    /// left out.
    targets: Vec<Target>,
    /// Of those, the files a later turn changed (and was not undone for), and those turns.
    later: Vec<RelPath>,
    later_turns: Vec<u32>,
}

/// The user turn to undo, its files and what the log says of them since.
fn undo_plan(
    reader: &SessionReader<SessionRecord>,
    turn: Option<u32>,
) -> Result<UndoPlan, UndoError> {
    let mut current = 0u32;
    let mut turns = 0u32;
    // Per user turn: each path's earliest pre-image, in order of its latest change.
    let mut changes: HashMap<u32, Vec<Target>> = HashMap::new();
    // Per path: its content (and mode, if known) as the session last left it, and the
    // turns that changed it (in order), less those an undo reverted.
    let mut tips: HashMap<RelPath, (Option<String>, Option<u32>)> = HashMap::new();
    let mut changed_in: HashMap<RelPath, Vec<u32>> = HashMap::new();
    for rec in reader.records() {
        match rec?.1 {
            SessionRecord::TurnStart { turn, .. } => {
                current = turn;
                turns = turns.max(turn);
            }
            SessionRecord::ToolResult { effects, .. } => {
                for e in effects {
                    // An edit keeps the mode it found (none: it created the file, whose
                    // mode is not logged).
                    let (path, pre, mode, tip) = match e {
                        PersistedEffect::Edited {
                            path,
                            pre_image_blob,
                            pre_mode,
                            post_hash,
                            ..
                        } => (
                            path,
                            pre_image_blob,
                            pre_mode,
                            (Some(post_hash.to_hex().to_string()), pre_mode),
                        ),
                        PersistedEffect::Deleted {
                            path,
                            pre_image_blob,
                            pre_mode,
                        } => (path, Some(pre_image_blob), Some(pre_mode), (None, None)),
                        _ => continue,
                    };
                    let pre = pre.map(|b| BlobRef::parse(&b)).transpose()?;
                    tips.insert(path.clone(), tip);
                    let by = changed_in.entry(path.clone()).or_default();
                    if by.last() != Some(&current) {
                        by.push(current);
                    }
                    let list = changes.entry(current).or_default();
                    let first = match list.iter().position(|t| t.path == path) {
                        Some(i) => list.remove(i),
                        None => Target {
                            path,
                            pre,
                            mode,
                            tip: None,
                            tip_mode: None,
                        },
                    };
                    list.push(first);
                }
            }
            SessionRecord::Undone { turn, files, .. } => {
                for f in files {
                    // A blob reference is the blake3 hex of its content.
                    tips.insert(
                        f.path.clone(),
                        (f.to.map(|b| b.as_str().to_string()), f.to_mode),
                    );
                    if let Some(by) = changed_in.get_mut(&f.path) {
                        by.retain(|&t| t != turn);
                    }
                    if let Some(list) = changes.get_mut(&turn) {
                        list.retain(|t| t.path != f.path);
                    }
                }
            }
            _ => {}
        }
    }
    let turn = turn.unwrap_or(turns);
    if turn == 0 || turn > turns {
        return Err(UndoError::NoSuchTurn { turn, turns });
    }
    let mut targets = changes.remove(&turn).unwrap_or_default();
    targets.reverse();
    let mut later = Vec::new();
    let mut later_turns = Vec::new();
    for t in &mut targets {
        (t.tip, t.tip_mode) = tips.get(&t.path).cloned().unwrap_or_default();
        let after: Vec<u32> = changed_in
            .get(&t.path)
            .into_iter()
            .flatten()
            .copied()
            .filter(|&k| k > turn)
            .collect();
        if !after.is_empty() {
            later.push(t.path.clone());
            later_turns.extend(after);
        }
    }
    later.sort();
    later_turns.sort_unstable();
    later_turns.dedup();
    Ok(UndoPlan {
        turn,
        targets,
        later,
        later_turns,
    })
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
