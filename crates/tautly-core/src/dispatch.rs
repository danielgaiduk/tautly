//! Running the tool calls of one response (spec 3.1 steps 3-4, checkpoint 2 ordering).
//!
//! The calls are cut into maximal runs: consecutive read-only calls (and calls that cannot
//! run at all) form one run whose calls execute concurrently; every write, command or
//! network call is a run of its own, executed alone, in model order. Before a run starts,
//! each of its calls passes, in model order, the loop breaker, the permission policy and,
//! for `ask`, the user. After a run, its results are processed in model order: context
//! operations (`pin`, `state`) are applied before the result is final (a refused pin
//! becomes an error result), the attempt ledger and the working state are updated, the
//! result is logged with its effects and checkpointed, and only then is `ToolFinished`
//! shown.
//!
//! Every run boundary is a tool boundary for the budgets: once one is used up, the calls
//! not started are answered "not run" (a call already running finishes). After a cancel,
//! running read, command and network calls get [`ABANDON_AFTER`] to stop; one that has not
//! is dropped (`bash` kills its process group when dropped), answered as abandoned, and
//! every read baseline recorded in that step is forgotten, since what the dropped call
//! showed is unknown. A write is never abandoned that way: it is local and short, it
//! checks the cancel before its commit point and skips its slow bookkeeping after it, so
//! it is awaited and its change logged for undo. Only a write still running
//! [`WRITE_BOUND`] after the cancel is dropped; its paths are then treated as changed
//! (baselines forgotten, write generation bumped) and the user is told undo cannot
//! restore them. Dropping the future does not stop a blocking write already under way, so
//! those paths stay watched: one that changes later is treated as changed again at the
//! next step start (each check waits at most 100 ms on the file system).

use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures::FutureExt;
use futures::future::join_all;
use serde_json::Value;
use tautly_provider::Content;
use tautly_sandbox::{Decision, Effect, SPILL_DIR};
use tautly_session::BlobRef;
use tautly_tools::AppliedEffect;
use tautly_tools::{ContextOp, PersistedEffect, Tool, ToolCx, ToolResult, parse_context_op};
use tokio::sync::mpsc;

use crate::approvals::Verdict;
use crate::protocol::{BudgetScope, EffectView, Event};
use crate::record::{SessionRecord, storable, utf8_effects};
use crate::session::{Log, LogFailure, Session};
use crate::turn::{Call, TurnControl};

/// Output lines buffered per running tool before they are dropped (bash never blocks).
const OUTPUT_BUFFER: usize = 64;
/// How long a running call may take to stop after a cancel before it is dropped; with the
/// log writes after it, the turn still ends within spec 8's 200 ms.
const ABANDON_AFTER: Duration = Duration::from_millis(150);
/// How long a write may still run after a cancel before it is dropped.
const WRITE_BOUND: Duration = Duration::from_secs(10);

/// One call, checked for what can be known before running it.
struct Planned<'a> {
    call: &'a Call,
    /// The tool and the call's effect, when the call can run at all.
    runnable: Option<(Arc<dyn Tool>, Effect)>,
    /// The result of a call that cannot run (bad arguments, unknown tool).
    rejected: Option<ToolResult>,
}

impl Planned<'_> {
    /// A write, command or network call: run alone.
    fn is_barrier(&self) -> bool {
        matches!(&self.runnable, Some((_, e)) if !matches!(e, Effect::ReadOnly))
    }
}

/// How a call ended.
struct Outcome {
    result: ToolResult,
    /// The loop breaker stopped it.
    blocked: bool,
    /// It ran (as opposed to being refused, denied, blocked or cancelled).
    ran: bool,
}

impl Outcome {
    fn refused(result: ToolResult) -> Self {
        Outcome {
            result,
            blocked: false,
            ran: false,
        }
    }

    fn cancelled() -> Self {
        Outcome::refused(ToolResult::error(
            "cancelled: the turn was cancelled before this call ran",
        ))
    }

    fn abandoned() -> Self {
        Outcome::refused(ToolResult::error(
            "cancelled: the call did not stop in time and was abandoned; whatever it \
             changed before that is unknown",
        ))
    }

    fn over_budget(scope: BudgetScope) -> Self {
        let name = match scope {
            BudgetScope::Turn => "turn",
            BudgetScope::Session => "session",
        };
        Outcome::refused(ToolResult::error(format!(
            "not run: the {name} budget is exhausted"
        )))
    }
}

/// What identifies a failure for the ledger: `exit N #<hash of the output>` for commands,
/// the error kind otherwise. The part from ` #` on is not shown to the model.
fn signature(tool: &str, r: &ToolResult) -> String {
    match (tool, r.meta.exit_code) {
        ("bash", Some(code)) => format!(
            "exit {code} #{}",
            &blake3::hash(r.output.as_bytes()).to_hex()[..16]
        ),
        ("bash", None) => "killed".to_string(),
        _ => r.meta.error_kind.unwrap_or("error").to_string(),
    }
}

impl Session {
    /// Run `calls` (one response's, in model order) as step `turn`; the results, in model
    /// order, one per call.
    pub(crate) async fn run_calls(
        &mut self,
        calls: &[Call],
        turn: u32,
        ctl: &TurnControl,
    ) -> Vec<Content> {
        let planned: Vec<Planned> = calls.iter().map(|c| self.plan(c)).collect();
        let mut results = Vec::with_capacity(calls.len());
        let mut start = 0;
        while start < planned.len() {
            let end = if planned[start].is_barrier() {
                start + 1
            } else {
                start
                    + planned[start..]
                        .iter()
                        .take_while(|p| !p.is_barrier())
                        .count()
            };
            let run = &planned[start..end];
            let outcomes = match self.budget_exhausted().await {
                Some(scope) => run
                    .iter()
                    .map(|p| match &p.rejected {
                        Some(r) => Outcome::refused(r.clone()),
                        None => Outcome::over_budget(scope),
                    })
                    .collect(),
                None => self.execute(run, turn, ctl).await,
            };
            let mut finished = Vec::with_capacity(run.len());
            for (p, o) in run.iter().zip(outcomes) {
                let (content, event) = self.finish(p.call, o, turn).await;
                results.push(content);
                finished.push(event);
            }
            // One durability point per run, before any of its results is shown.
            self.checkpoint().await;
            for ev in finished {
                self.emit(ev).await;
            }
            start = end;
        }
        self.context.state.failed_attempts = self.ledger.listed();
        results
    }

    fn plan<'a>(&self, call: &'a Call) -> Planned<'a> {
        let tool = self.tools.iter().find(|t| t.name() == call.name).cloned();
        let mut planned = Planned {
            call,
            runnable: None,
            rejected: None,
        };
        match (&call.args, tool) {
            (Err(why), _) => {
                planned.rejected = Some(ToolResult::error_kind(
                    format!("invalid arguments: {why}; the call did not run"),
                    "bad_args",
                ));
            }
            (Ok(_), None) => {
                let names: Vec<&str> = self.tools.iter().map(|t| t.name()).collect();
                planned.rejected = Some(ToolResult::error(format!(
                    "unknown tool `{}`; the tools are: {}",
                    call.name,
                    names.join(", ")
                )));
            }
            (Ok(args), Some(tool)) => match tool.effect(args) {
                Ok(effect) => planned.runnable = Some((tool, effect)),
                Err(e) => {
                    planned.rejected = Some(ToolResult::error_kind(e.to_string(), "bad_args"))
                }
            },
        }
        planned
    }

    /// Gate every call of `run` in model order, then run the admitted ones concurrently.
    async fn execute(&mut self, run: &[Planned<'_>], turn: u32, ctl: &TurnControl) -> Vec<Outcome> {
        let mut gated = Vec::with_capacity(run.len());
        for p in run {
            gated.push(self.gate(p, ctl).await);
        }
        let admitted: Vec<&Planned> = run
            .iter()
            .zip(&gated)
            .filter(|(_, g)| g.is_none())
            .map(|(p, _)| p)
            .collect();
        let this = &*self;
        let bounded = admitted.iter().map(|p| async move {
            let grace = match &p.runnable {
                Some((_, Effect::Write { .. })) => WRITE_BOUND,
                _ => ABANDON_AFTER,
            };
            tokio::select! {
                r = this.invoke(p, turn, ctl) => Some(r),
                () = after_cancel(ctl, grace) => None,
            }
        });
        let ran: Vec<Option<ToolResult>> = join_all(bounded).await;
        let mut lost_writes = Vec::new();
        for (p, r) in admitted.iter().zip(&ran) {
            if let (None, Some((_, Effect::Write { paths }))) = (r, &p.runnable) {
                lost_writes.extend(paths.iter().cloned());
            }
        }
        if ran.iter().any(Option::is_none) {
            self.reads.forget_recorded_at(turn);
        }
        if !lost_writes.is_empty() {
            self.lost_writes(&lost_writes).await;
        }
        let mut ran = admitted.iter().zip(ran);
        gated
            .into_iter()
            .map(|g| {
                g.unwrap_or_else(|| match ran.next().expect("one result per admitted call") {
                    (_, Some(result)) => Outcome {
                        result,
                        blocked: false,
                        ran: true,
                    },
                    (p, None) if matches!(&p.runnable, Some((_, Effect::Write { .. }))) => {
                        Outcome::refused(ToolResult::error(
                            "cancelled: the write did not finish in time and was abandoned; \
                             its files may have changed, and undo cannot restore them",
                        ))
                    }
                    (_, None) => Outcome::abandoned(),
                })
            })
            .collect()
    }

    /// A write was dropped half way: treat its paths as changed in ways nobody saw.
    async fn lost_writes(&mut self, paths: &[PathBuf]) {
        for p in paths {
            if let Ok(full) = self.sandbox.resolve(p) {
                self.reads.forget_path(&full);
            }
            self.reads.forget_path(p);
            // Its blocking write may still land later: watched at every step start.
            let seen = self.quick_meta(p).await;
            self.lost_write_paths.insert(p.clone(), seen);
        }
        self.ledger
            .note_effects(&[AppliedEffect::Exec { may_write: true }]);
        self.evidence.on_lost_write();
        let list: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
        self.warn(format!(
            "a write was abandoned after the cancel; {} may have changed, and undo cannot \
             restore {}",
            list.join(", "),
            if list.len() == 1 { "it" } else { "them" }
        ))
        .await;
    }

    /// At a step start: a path whose write was abandoned and that has changed since is
    /// treated as changed again (baselines forgotten, write generation bumped), and said.
    pub(crate) async fn check_lost_writes(&mut self) {
        if self.lost_write_paths.is_empty() {
            return;
        }
        let paths: Vec<PathBuf> = self.lost_write_paths.keys().cloned().collect();
        for p in paths {
            let now = self.quick_meta(&p).await;
            if self.lost_write_paths.get(&p) == Some(&now) {
                continue;
            }
            self.lost_write_paths.insert(p.clone(), now);
            if let Ok(full) = self.sandbox.resolve(&p) {
                self.reads.forget_path(&full);
            }
            self.reads.forget_path(&p);
            self.ledger
                .note_effects(&[AppliedEffect::Exec { may_write: true }]);
            self.warn(format!(
                "{} changed after its write was abandoned; undo cannot restore it",
                p.display()
            ))
            .await;
        }
    }

    /// `(mtime, len)` of `p`, or None when it is missing or does not answer quickly (a file
    /// system that hangs is what got the write abandoned).
    async fn quick_meta(&self, p: &std::path::Path) -> Option<(std::time::SystemTime, u64)> {
        let meta = tokio::time::timeout(Duration::from_millis(100), self.sandbox.metadata(p));
        match meta.await {
            Ok(Ok(m)) => Some((m.mtime, m.len)),
            _ => None,
        }
    }

    /// Whether the call may run: None if so, else how it ended without running.
    async fn gate(&mut self, p: &Planned<'_>, ctl: &TurnControl) -> Option<Outcome> {
        if let Some(r) = &p.rejected {
            return Some(Outcome::refused(r.clone()));
        }
        if ctl.cancel.is_cancelled() {
            return Some(Outcome::cancelled());
        }
        let (_, effect) = p.runnable.as_ref()?;
        let args = p.call.input();
        if let Some(by) = self.ledger.check(&p.call.name, &args) {
            return Some(Outcome {
                result: ToolResult::error(by.message()),
                blocked: true,
                ran: false,
            });
        }
        let raw_cmd = args.get("command").and_then(Value::as_str);
        let raw_cmd = raw_cmd.filter(|_| p.call.name == "bash");
        match self.policy.decide(&p.call.name, effect, raw_cmd) {
            Decision::Allow => None,
            Decision::Deny { rule } => Some(Outcome::refused(ToolResult::error(format!(
                "denied by the permission rule `{rule}`; the call did not run"
            )))),
            Decision::Ask { reason } => self.ask(p.call, &args, reason, ctl).await,
        }
    }

    /// Ask the user (interactive) or deny (headless, spec 3.4).
    async fn ask(
        &self,
        call: &Call,
        args: &Value,
        reason: String,
        ctl: &TurnControl,
    ) -> Option<Outcome> {
        let Some((request_id, rx)) = ctl.approvals.open() else {
            return Some(Outcome::refused(ToolResult::error(format!(
                "denied: this call needs approval ({reason}), and nobody can approve it in \
                 this session (headless); the user can allow it with --approve auto-edit or yolo"
            ))));
        };
        self.emit(Event::ToolApprovalRequested {
            request_id: request_id.clone(),
            call_id: call.id.clone(),
            tool: call.name.clone(),
            args: args.clone(),
            reason,
        })
        .await;
        let verdict = tokio::select! {
            v = rx => v.unwrap_or(Verdict::Denied { reason: None }),
            () = ctl.cancel.cancelled() => {
                ctl.approvals.withdraw(&request_id);
                return Some(Outcome::cancelled());
            }
        };
        match verdict {
            Verdict::Approved => None,
            Verdict::Denied { reason } => Some(Outcome::refused(ToolResult::error(match reason {
                Some(r) => format!("denied by user: {r}"),
                None => "denied by user".to_string(),
            }))),
        }
    }

    /// Run one admitted call. Output lines are forwarded as `ToolOutputDelta` and all of
    /// them are out before this returns. A panicking tool becomes an error result.
    async fn invoke(&self, p: &Planned<'_>, turn: u32, ctl: &TurnControl) -> ToolResult {
        let (tool, _) = p.runnable.as_ref().expect("admitted calls are runnable");
        let call = p.call;
        self.emit(Event::ToolStarted {
            call_id: call.id.clone(),
            tool: call.name.clone(),
        })
        .await;
        let (tx, mut rx) = mpsc::channel::<String>(OUTPUT_BUFFER);
        let events = self.events.clone();
        let call_id = call.id.clone();
        // Aborted if this call is dropped (abandoned), so no output of it follows its
        // `ToolFinished`.
        let mut forward = AbortOnDrop(Some(tokio::spawn(async move {
            while let Some(text) = rx.recv().await {
                events
                    .emit(Event::ToolOutputDelta {
                        call_id: call_id.clone(),
                        text,
                    })
                    .await;
            }
        })));
        let mut cx = ToolCx::new(self.info.caps.edit_format, self.root().join(SPILL_DIR))
            .with_reads(Arc::clone(&self.reads))
            .with_cancel(ctl.cancel.child_token())
            .with_output_tx(tx)
            .with_turn(turn)
            .with_process_groups(self.groups.clone());
        if let Some(index) = self.workspace.index() {
            cx = cx.with_index(index);
        }
        if let Some(memory) = self.workspace.memory() {
            cx = cx.with_memory(memory);
        }
        let run = tool.run(call.input(), self.sandbox.as_ref(), &cx);
        let result = AssertUnwindSafe(run)
            .catch_unwind()
            .await
            .unwrap_or_else(|_| {
                ToolResult::error(
                    "the tool crashed (an engine bug); whatever it changed before that is unknown",
                )
            });
        drop(cx);
        if let Some(h) = forward.0.take() {
            let _ = h.await;
        }
        result
    }

    /// Settle one call's outcome in model order: its result for the history and its
    /// `ToolFinished`, to be shown once the run is checkpointed.
    async fn finish(&mut self, call: &Call, outcome: Outcome, turn: u32) -> (Content, Event) {
        let Outcome {
            mut result,
            blocked,
            ran,
        } = outcome;
        let args = call.input();
        if ran && !result.is_error {
            match parse_context_op(&call.name, &args) {
                // Task 28: a `remember` proposal goes to the user (`MemoryProposed`) here.
                Some(ContextOp::Remember(_)) | None => {}
                Some(op) => {
                    if let Err(e) = self.context.apply_op(op) {
                        result.output = format!("not applied: {e}");
                        result.is_error = true;
                    }
                }
            }
        }
        if ran {
            self.ledger.note_effects(&result.effects);
            let raw_cmd = args.get("command").and_then(Value::as_str);
            self.evidence.on_tool(
                turn,
                raw_cmd.filter(|_| call.name == "bash"),
                &result,
                self.ledger.generation(),
            );
            let sig = signature(&call.name, &result);
            self.ledger.record(
                &call.name,
                &args,
                turn,
                !result.is_error,
                &sig,
                &result.output,
            );
        }
        self.context.apply_effects(&result.effects, turn);
        self.context.push_tool_result(
            &call.id,
            &call.name,
            &args,
            &result.effects,
            result.is_error,
            turn,
        );
        let event = Event::ToolFinished {
            call_id: call.id.clone(),
            tool: call.name.clone(),
            output: result.output.clone(),
            is_error: result.is_error,
            blocked,
            effects: result.effects.iter().map(EffectView::from).collect(),
            meta: (&result.meta).into(),
        };
        let content = Content::ToolResult {
            id: call.id.clone(),
            output: result.output.clone(),
            is_error: result.is_error,
        };
        let id = call.id.clone();
        if let Some(skipped) = self
            .with_log(move |log| persist_result(log, id, result, turn))
            .await
        {
            self.untracked(skipped.untracked).await;
            for p in skipped.bad_pre_images {
                self.warn(format!(
                    "{}: the change is not in the session log (its prior content could not \
                     be referenced), so undo will not restore it",
                    p.to_string_lossy()
                ))
                .await;
            }
        }
        (content, event)
    }
}

/// A task that is aborted when this is dropped before it was awaited.
struct AbortOnDrop(Option<tokio::task::JoinHandle<()>>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        if let Some(h) = &self.0 {
            h.abort();
        }
    }
}

/// Completes `grace` after the turn is cancelled.
async fn after_cancel(ctl: &TurnControl, grace: Duration) {
    ctl.cancel.cancelled().await;
    tokio::time::sleep(grace).await;
}

/// What [`persist_result`] left out of the log.
struct Skipped {
    /// Paths that are not valid UTF-8.
    untracked: Vec<PathBuf>,
    /// Changed files whose pre-image reference is unusable: no `Edit` record, rather than
    /// one that says the file was created.
    bad_pre_images: Vec<PathBuf>,
}

/// Log a result with its effects (file contents as blobs, paths that are not valid UTF-8
/// left out) and an `Edit` record per changed file. Buffered: the caller checkpoints.
/// Blocking file I/O.
fn persist_result(
    log: &Log,
    id: String,
    result: ToolResult,
    turn: u32,
) -> Result<Skipped, LogFailure> {
    let result_blob = log.blob(result.output.as_bytes())?;
    let mut persisted = Vec::with_capacity(result.effects.len());
    for e in &result.effects {
        let mut put = |bytes: &[u8]| log.blob(bytes).map(|b| b.as_str().to_string());
        persisted.push(PersistedEffect::from_applied(e, &mut put)?);
    }
    let (effects, untracked) = utf8_effects(persisted);
    let mut edits = Vec::new();
    let mut bad_pre_images = Vec::new();
    for e in &effects {
        let (path, pre) = match e {
            PersistedEffect::Edited {
                path,
                pre_image_blob,
                ..
            } => (path, pre_image_blob.as_deref()),
            PersistedEffect::Deleted {
                path,
                pre_image_blob,
                ..
            } => (path, Some(pre_image_blob.as_str())),
            _ => continue,
        };
        debug_assert!(storable(path));
        let pre_image = match pre.map(BlobRef::parse) {
            None => None,
            Some(Ok(b)) => Some(b),
            Some(Err(_)) => {
                bad_pre_images.push(path.clone());
                continue;
            }
        };
        edits.push(SessionRecord::Edit {
            path: path.clone(),
            pre_image,
            turn,
        });
    }
    log.append(&SessionRecord::ToolResult {
        id,
        result_blob,
        is_error: result.is_error,
        effects,
        meta: result.meta,
    })?;
    for rec in &edits {
        log.append(rec)?;
    }
    Ok(Skipped {
        untracked,
        bad_pre_images,
    })
}
