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
//!   the tools it logged, MCP tools included), `StartPins`, `Uncompacted` and `Undone`
//!   are redone where they happened. Tool results are read back from their blobs; only
//!   references and the latest compaction stay in memory.
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
//! `SetBudget`, a switch, a manual compaction) is in it, in any order, as at the tip, up
//! to an undo: the cut ends before an `Undone` after step `n`, since the files at step `n`
//! (spec 7.12.1 `state_at`) are the ones before that undo.
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

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use tautly_context::{CompactionRecord, Context, MutationReason};
use tautly_memory::MemoryChange;
use tautly_provider::{Capabilities, Catalog, ModelId, RawUsage, ToolSpec, cost_usd};
use tautly_session::{HeaderInit, Origin, SessionReader, SessionWriter};
use tautly_tools::{ReadTracker, Tool, registry};

use crate::attempts::AttemptLedger;
use crate::budget::Budget;
use crate::ledger::{LEDGER_FILE, Ledger};
use crate::mcp::MCP_TOOL_PREFIX;
use crate::model_switch::Stopped;
use crate::protocol::{BudgetScope, ErrorKind, Event, FinishReason};
use crate::record::SessionRecord;
use crate::session::{PrefixLog, Session};
use crate::turn::TurnControl;

mod rebuild;

pub(crate) use self::rebuild::rebuild_with;
use self::rebuild::{originals, restore_and_keep};

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
    /// The usage the log holds, with the model it was spent on: every complete response's
    /// (and the attempts discarded before it), and every `Charged` record's.
    pub(crate) usage: Vec<(ModelId, RawUsage)>,
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
    /// Each replayed collection: the call ids it elided and the tokens it freed (what
    /// `Session::gc_elided` holds live), so receipts keep crediting them (spec 7.11.1).
    pub(crate) gc_elided: Vec<(Vec<String>, u32)>,
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
        let specs = registry(self.extra.clone())
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
    usage: Vec<(ModelId, RawUsage)>,
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
                log.append_at_last(&SessionRecord::TurnEnd {
                    reason: FinishReason::Error,
                    evidence: None,
                    synthetic: true,
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
            self.restore_model(model, stop, Stopped::Leave).await;
        }
        self.resume_start(resumed, config_pins).await;
        self.show_pending_proposals().await;
        Ok(())
    }

    /// What a rebuild calibrates the estimator with: this session's catalog and its MCP
    /// tools.
    pub(crate) fn calibration(&self) -> Calibration {
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
        // Replaced, not extended: a fork or rewind continues its own log's collections.
        self.gc_elided = r.gc_elided;
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
            let mut b = crate::lock(&self.budgets);
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
    /// warning when this build's system prompt or tools differ from the logged ones.
    /// AGENTS.md and the repo map are the logged ones; a session that never sent a request
    /// starts up as a new one, keeping the start pins it logged.
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

    /// Count what the session spent before against its budgets: each figure the larger of
    /// the cost ledger's rows of this session (discarded attempts included) and the usage
    /// in the log, each priced by the model it was spent on.
    async fn restore_spend(&mut self, logged: Vec<(ModelId, RawUsage)>) {
        let path = self.data_home.join(LEDGER_FILE);
        let id = self.session_id.clone();
        let totals = tokio::task::spawn_blocking(move || {
            Ledger::open(&path).and_then(|l| l.session_totals(&id))
        })
        .await;
        let mut usage = RawUsage::default();
        let mut usd = 0.0;
        for (model, u) in &logged {
            usage.input += u.input;
            usage.output += u.output;
            usage.cache_read += u.cache_read;
            usage.cache_write += u.cache_write;
            let price = self.setup.catalog.get(model).price;
            usd += cost_usd(u, price.as_ref()).unwrap_or(0.0);
        }
        // Both are lower bounds: ledger rows are queued and may be lost at a crash (or
        // with a ledger moved aside), and the log holds no discarded attempts of a
        // response that broke off. Each figure is the larger (cp5 review C M3).
        match totals {
            Ok(Ok(t)) if t.requests > 0 || logged.is_empty() => {
                usage.input = usage.input.max(t.input);
                usage.output = usage.output.max(t.output);
                usage.cache_read = usage.cache_read.max(t.cache_read);
                usage.cache_write = usage.cache_write.max(t.cache_write);
                usd = usd.max(t.usd);
            }
            other => {
                let why = match other {
                    Ok(Err(e)) => e.to_string(),
                    Ok(Ok(_)) => "it has no rows of this session".to_string(),
                    Err(e) => e.to_string(),
                };
                self.warn(format!(
                    "the cost ledger could not give this session's spend ({why}); it is \
                     rebuilt from the session log"
                ))
                .await;
            }
        }
        let mut b = crate::lock(&self.budgets);
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
                self.emit(Event::SessionChanged {
                    id: None,
                    session_id: id,
                })
                .await;
            }
            Err(why) => {
                self.emit(Event::Error {
                    id: None,
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
            // As a rewind counts it (`at = 0` copies the set-up before the first step,
            // cp5 review C M9). The parent offset names the last record copied: with none,
            // a branch would read as one cut after record 0.
            let step = cut.context.next_turn() - 1;
            let count = match at {
                Some(at) => crate::timetravel::copy_count(&parent, at, &cut),
                // The tip of a session with no step yet is nothing to branch from.
                None => cut.end_offset.map(|end| end + 1),
            };
            let count = count.ok_or_else(|| {
                "there is nothing to branch from yet: the session has no records before \
                 that point"
                    .to_string()
            })?;
            // The branch's records start from the parent's first model.
            let init = HeaderInit {
                parent: Some((parent_id, count - 1)),
                parent_step: Some(step),
                model: parent.header().model.clone(),
                ..init
            };
            let child = branch(
                &data_home,
                &root,
                init,
                &parent_path,
                count,
                cut.incomplete_turn,
                Vec::new(),
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
        Ok(self
            .continue_on(child, rebuilt, stop, Continue::Branch, "fork")
            .await)
    }

    /// Continue on the log `writer` (a branch, or a session switched to) with the context
    /// `rebuilt` from it: the log, the session id, the checkpoint refs and numbering
    /// ([`adopt_checkpoints`](Self::adopt_checkpoints)), the last-request file, the spend
    /// (none for a new branch; the session's own for one switched to), the rebuilt state
    /// and the model the log ends on. Warnings say `what` (`fork`, `rewind`, `switch`).
    /// Returns the session's id and log.
    pub(crate) async fn continue_on(
        &mut self,
        writer: SessionWriter<SessionRecord>,
        rebuilt: Rebuilt,
        stop: impl std::future::Future<Output = ()>,
        how: Continue,
        what: &str,
    ) -> (String, PathBuf) {
        let id = writer.session_id().to_string();
        let path = writer.path().to_path_buf();
        self.log.replace(writer);
        self.session_id = id.clone();
        self.adopt_checkpoints(&id, &rebuilt.checkpoints).await;
        self.last_request_path = crate::engine::last_request_path(&path);
        let fresh = crate::lock(&self.budgets).without_spend();
        *crate::lock(&self.budgets) = fresh;
        let mut resumed = self.adopt(rebuilt);
        for w in std::mem::take(&mut resumed.warnings) {
            self.warn(format!("{what}: {w}")).await;
        }
        if how == Continue::Resumed {
            self.restore_spend(std::mem::take(&mut resumed.usage)).await;
        }
        if let Some(model) = resumed.model {
            self.restore_model(model, stop, Stopped::Continue).await;
        }
        (id, path)
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
        let before = self.assemble().1.used;
        let after = self.compact_now(ctl).await;
        self.events = unbound;
        // Its end, whatever happened (sent even after a cancel: the client waits for it).
        self.events
            .emit_last(Event::CompactFinished {
                id: None,
                applied: after.is_some(),
                tokens_freed: after.map_or(0, |a| before.saturating_sub(a)),
            })
            .await;
    }

    /// The context's tokens after the compaction, or None when nothing was applied.
    async fn compact_now(&mut self, ctl: &TurnControl) -> Option<u32> {
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
            return None;
        }
        if let Some(text) = self.pending_memory_index.take() {
            self.context.memory_index = Some(text.clone()).filter(|t| !t.trim().is_empty());
            self.log(SessionRecord::MemoryIndexApplied { text }).await;
        }
        if self.context.fold_session_pins() {
            self.log(SessionRecord::PinsFolded).await;
        }
        // The cached prefix is gone: what is pending by the next step goes in its epoch.
        self.context.force_cold_epoch();
        self.checkpoint().await;
        let (_, status) = self.assemble();
        self.emit(Event::context_status(&status)).await;
        Some(status.used)
    }

    /// `Command::Uncompact` (between turns): undo the latest compaction if the context
    /// then fits the window (less the tools and the reply), then restore every collected
    /// result (from its logged blob) if that fits too. Restored results are not
    /// collected again. Logged as `Uncompacted`.
    pub(crate) async fn uncompact_command(&mut self) {
        self.checkpoint().await;
        // The compacted turns' entries are in the compaction record until it is undone.
        let compacted = self
            .latest_compaction
            .iter()
            .flat_map(|c| &c.removed_results);
        let elided: Vec<String> = self
            .context
            .results
            .iter()
            .chain(compacted)
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
        // The cached prefix is gone: what is pending by the next step goes in its epoch.
        self.context.force_cold_epoch();
        self.checkpoint().await;
        let (_, status) = self.assemble();
        self.emit(Event::context_status(&status)).await;
    }
}

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// How [`Session::continue_on`] continues.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Continue {
    /// On a new branch (a fork or rewind): nothing spent yet.
    Branch,
    /// On an existing session (a switch): its spend so far counts.
    Resumed,
}

/// A fork's log: the first `count` records of the log at `parent`, copied verbatim, then
/// `Forked` (what follows is the branch's own), a `TurnEnd{cancelled}` when the copy ends
/// inside a turn (`close_turn`), and `tail` (a rewind's record of the files it continues
/// from). On a failure the half-written log is removed, so no broken branch is listed.
pub(crate) fn branch(
    data_home: &std::path::Path,
    root: &std::path::Path,
    init: HeaderInit,
    parent: &std::path::Path,
    count: u64,
    close_turn: bool,
    tail: Vec<SessionRecord>,
) -> Result<SessionWriter<SessionRecord>, String> {
    let mut child =
        SessionWriter::<SessionRecord>::create_in(data_home, root, init).map_err(err)?;
    let written = (|| {
        child.copy_from(parent, count)?;
        // Both are written for the copy, at its last record's time.
        child.append_at_last(&SessionRecord::Forked)?;
        if close_turn {
            child.append_at_last(&SessionRecord::TurnEnd {
                reason: FinishReason::Cancelled,
                evidence: None,
                synthetic: true,
            })?;
        }
        for rec in &tail {
            child.append(rec)?;
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
pub(crate) fn discard(child: SessionWriter<SessionRecord>) {
    let path = child.path().to_path_buf();
    drop(child);
    let _ = std::fs::remove_file(path);
}

#[cfg(test)]
mod tests {
    use tautly_session::BlobRef;

    use super::*;
    use crate::protocol::Command;

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

    /// The attempt ledger's write generation of a log holding `recs`.
    fn generation_after(recs: &[SessionRecord]) -> u64 {
        generation_at(recs, None)
    }

    /// The same, of `rebuild(until)`.
    fn generation_at(recs: &[SessionRecord], until: Option<u32>) -> u64 {
        let home = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let mut w = SessionWriter::<SessionRecord>::create_in(
            home.path(),
            root.path(),
            HeaderInit::default(),
        )
        .unwrap();
        for r in recs {
            w.append(r).unwrap();
        }
        w.checkpoint().unwrap();
        let reader = SessionReader::<SessionRecord>::open(w.path()).unwrap();
        rebuild(&reader, until).unwrap().attempts.generation()
    }

    #[test]
    fn a_lost_write_moves_the_rebuilt_write_generation() {
        let root = std::path::Path::new("/r");
        let turn = |recs: Vec<SessionRecord>| {
            let mut all = vec![SessionRecord::TurnStart {
                turn: 1,
                text: "go".into(),
            }];
            all.extend(recs);
            all.push(SessionRecord::TurnEnd {
                reason: FinishReason::Cancelled,
                evidence: None,
                synthetic: false,
            });
            all
        };
        let lost = SessionRecord::WritesLost {
            paths: vec![tautly_session::RelPath::from_abs(root, &root.join("a.txt")).unwrap()],
            late: false,
        };
        assert_eq!(
            generation_after(&turn(vec![lost])),
            generation_after(&turn(vec![])) + 1
        );
    }

    /// A provider step `step` answered with text.
    fn text_step(step: u32) -> Vec<SessionRecord> {
        let blob = BlobRef::parse(&"a".repeat(64)).unwrap();
        vec![
            SessionRecord::Request {
                hash: blob,
                turn: 1,
                step,
                model: ModelId::from("mock/m"),
                blocks: crate::record::BlockHashes {
                    stable: String::new(),
                    pinned: String::new(),
                    history: String::new(),
                    tail: String::new(),
                },
            },
            SessionRecord::Provider(tautly_provider::ProviderEvent::TextDelta("ok".into())),
            SessionRecord::Provider(tautly_provider::ProviderEvent::Done {
                stop: tautly_provider::StopReason::EndTurn,
            }),
        ]
    }

    /// Re-review N3: a late write noticed at step 3's start is past a cut at step 2.
    #[test]
    fn a_lost_write_noticed_after_the_cut_is_left_out() {
        let root = std::path::Path::new("/r");
        let lost = SessionRecord::WritesLost {
            paths: vec![tautly_session::RelPath::from_abs(root, &root.join("a.txt")).unwrap()],
            late: true,
        };
        let log = |with_lost: bool| {
            let mut all = vec![SessionRecord::TurnStart {
                turn: 1,
                text: "go".into(),
            }];
            all.extend(text_step(2));
            if with_lost {
                all.push(lost.clone());
            }
            all.extend(text_step(3));
            all.push(SessionRecord::TurnEnd {
                reason: FinishReason::Completed,
                evidence: None,
                synthetic: false,
            });
            all
        };
        assert_eq!(
            generation_at(&log(true), Some(2)),
            generation_at(&log(false), Some(2))
        );
        assert_eq!(
            generation_at(&log(true), None),
            generation_at(&log(false), None) + 1
        );
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
            Vec::new(),
        );
        assert!(made.is_err());
        assert_eq!(logs(home.path()), Vec::<PathBuf>::new());
    }

    /// Fuzzing (timetravel): rebuild ends a step at every record that starts the next
    /// one's inputs (an epoch, a model switch, a fork, a late lost write), so a response
    /// event after one lands in no step; the time-travel numbering must end the step there
    /// too, or `state_at` knows a step `rebuild` does not.
    #[test]
    fn rebuild_and_time_travel_end_a_step_at_the_same_records() {
        let blob = BlobRef::parse(&"a".repeat(64)).unwrap();
        let closers = [
            SessionRecord::Compaction {
                removed: vec![blob.clone()],
                summary: "s".into(),
            },
            SessionRecord::Forked,
            SessionRecord::ModelSet {
                model: ModelId::from("mock/m"),
            },
            SessionRecord::WritesLost {
                paths: Vec::new(),
                late: true,
            },
            SessionRecord::PinsFolded,
        ];
        for closer in closers {
            // Step 2 gets a `Done` and no content (unanswered), then the closer, then text.
            let mut recs = vec![SessionRecord::TurnStart {
                turn: 1,
                text: "go".into(),
            }];
            recs.extend(text_step(2));
            recs.remove(2); // the text delta
            recs.push(closer.clone());
            recs.push(SessionRecord::Provider(
                tautly_provider::ProviderEvent::TextDelta("late".into()),
            ));
            assert_same_last_step(&recs, &format!("after {closer:?}"));
        }
        // A `ToolCall` record answers nothing on its own: rebuild takes the calls from the
        // response's events, and a step whose response put nothing into the history
        // shares its number with the next.
        let mut recs = vec![SessionRecord::TurnStart {
            turn: 1,
            text: "go".into(),
        }];
        recs.extend(text_step(2));
        recs.remove(2);
        recs.push(SessionRecord::ToolCall {
            id: "c".into(),
            name: "bash".into(),
            args: serde_json::json!({}),
            args_blob: None,
        });
        assert_same_last_step(&recs, "a lone ToolCall record");
        // Review M5: a pin sent between turns (no turn running) starts the next step's
        // inputs: rebuild ends the open step there.
        for cmd in [
            Command::Pin { text: "p".into() },
            Command::Unpin { text: "p".into() },
        ] {
            let mut recs = text_step(1);
            recs.remove(1);
            recs.push(SessionRecord::Command(cmd.clone()));
            recs.push(SessionRecord::Provider(
                tautly_provider::ProviderEvent::TextDelta("late".into()),
            ));
            assert_same_last_step(&recs, &format!("after {cmd:?} between turns"));
        }
    }

    /// `step_at_offset` at the end of a log of `recs` is the last step `rebuild` knows.
    fn assert_same_last_step(recs: &[SessionRecord], what: &str) {
        let home = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let mut w = SessionWriter::<SessionRecord>::create_in(
            home.path(),
            root.path(),
            HeaderInit::default(),
        )
        .unwrap();
        for r in recs {
            w.append(r).unwrap();
        }
        w.checkpoint().unwrap();
        let reader = SessionReader::<SessionRecord>::open(w.path()).unwrap();
        let last = crate::timetravel::step_at_offset(&reader, u64::MAX);
        match rebuild(&reader, Some(last + 1)) {
            Err(ResumeError::NoSuchStep { last: rebuilt, .. }) => {
                assert_eq!(rebuilt, last, "{what}")
            }
            other => panic!("{what}: {:?}", other.map(|_| ())),
        }
    }
}
