//! Running the tool calls of one response (spec 3.1 steps 3-4, checkpoint 2 ordering).
//!
//! The calls are cut into maximal runs: consecutive read-only calls (and calls that cannot
//! run at all) form one run whose calls execute concurrently; every write, command or
//! network call is a run of its own, executed alone, in model order. Before a run starts,
//! each of its calls passes, in model order, the loop breaker, the permission policy and,
//! for `ask`, the user, who is shown the call's preview diff (spec 7.11.4): a call whose
//! preview fails is answered with that failure without asking, and an approved call runs
//! only if the files it reads are the ones the preview read, with the same content. After
//! a run, its results are processed in model order: context operations (`pin`, `state`)
//! are applied before the result is final (a refused pin becomes an error result), the
//! attempt ledger and the working state are updated, the result is logged with its
//! effects and checkpointed, and only then is `ToolFinished` shown.
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
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures::FutureExt;
use futures::future::join_all;
use serde_json::Value;
use tautly_provider::Content;
use tautly_sandbox::{Decision, Effect, Mode, SPILL_DIR};
use tautly_session::{BlobRef, RelPath};
use tautly_tools::AppliedEffect;
use tautly_tools::{
    ContextOp, PersistedEffect, Preview, Tool, ToolCx, ToolResult, parse_context_op,
};
use tokio::sync::mpsc;

use crate::approvals::{ApprovalFor, Verdict};
use crate::protocol::{BudgetScope, EffectView, Event};
use crate::record::{ApprovalKind, SessionRecord};
use crate::session::{Log, LogFailure, Session};
use crate::turn::{Call, TurnControl};

/// Output lines buffered per running tool before they are dropped (bash never blocks).
const OUTPUT_BUFFER: usize = 64;
/// How long a running call may take to stop after a cancel before it is dropped; with the
/// log writes after it, the turn still ends within spec 8's 200 ms.
const ABANDON_AFTER: Duration = Duration::from_millis(150);
/// How long a write may still run after a cancel before it is dropped.
const WRITE_BOUND: Duration = Duration::from_secs(10);

/// The file hashes an approved preview started from (`ToolCx::with_approved_bases`).
type Bases = Vec<(PathBuf, Option<blake3::Hash>)>;

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
            BudgetScope::Unknown => "unknown",
        };
        Outcome::refused(ToolResult::error(format!(
            "not run: the {name} budget is exhausted"
        )))
    }
}

/// What identifies a failure for the ledger: `exit N #<hash of the output>` for commands,
/// the error kind otherwise. The part from ` #` on is not shown to the model.
pub(crate) fn signature(tool: &str, r: &ToolResult) -> String {
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
                let (content, events) = self.finish(p.call, o, turn).await;
                results.push(content);
                finished.extend(events);
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
            gated.push(self.gate(p, turn, ctl).await);
        }
        let admitted: Vec<(&Planned, Option<Bases>)> = run
            .iter()
            .zip(gated.iter_mut())
            .filter_map(|(p, g)| g.as_mut().ok().map(|bases| (p, bases.take())))
            .collect();
        let this = &*self;
        let bounded = admitted.iter().map(|(p, bases)| async move {
            let grace = match &p.runnable {
                Some((_, Effect::Write { .. })) => WRITE_BOUND,
                _ => ABANDON_AFTER,
            };
            tokio::select! {
                r = this.invoke(p, bases.as_ref(), turn, ctl) => Some(r),
                () = after_cancel(ctl, grace) => None,
            }
        });
        let ran: Vec<Option<ToolResult>> = join_all(bounded).await;
        // What a call asked the side model counts before its result is final.
        self.drain_side_usage().await;
        let mut lost_writes = Vec::new();
        for ((p, _), r) in admitted.iter().zip(&ran) {
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
        let mut ran = admitted.iter().map(|(p, _)| p).zip(ran);
        gated
            .into_iter()
            .map(Result::err)
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
        self.note_writes_lost(paths, false).await;
        self.memory.refresh_due = true;
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
            self.note_writes_lost(std::slice::from_ref(&p), true).await;
            self.warn(format!(
                "{} changed after its write was abandoned; undo cannot restore it",
                p.display()
            ))
            .await;
        }
    }

    /// Writes to `paths` whose outcome is unknown: the write generation moves, logged as
    /// `WritesLost` so a rebuilt attempt ledger moves too (paths a record cannot hold are
    /// said instead). `late`: noticed at a step start (the abandoned write landed).
    async fn note_writes_lost(&mut self, paths: &[PathBuf], late: bool) {
        self.ledger
            .note_effects(&[AppliedEffect::Exec { may_write: true }]);
        let mut logged = Vec::with_capacity(paths.len());
        let mut skipped = Vec::new();
        for p in paths {
            let full = self.sandbox.resolve(p).unwrap_or_else(|_| p.clone());
            match RelPath::from_abs(self.root(), &full) {
                Some(rel) => logged.push(rel),
                None => skipped.push(full),
            }
        }
        self.untracked(skipped).await;
        self.log(SessionRecord::WritesLost {
            paths: logged,
            late,
        })
        .await;
    }

    /// `(mtime, len)` of `p`, or None when it is missing or does not answer quickly (a file
    /// system that hangs is what got the write abandoned). A probe that did not answer
    /// keeps running and is waited on again next time: at most one per path is in flight.
    async fn quick_meta(&mut self, p: &Path) -> Option<(std::time::SystemTime, u64)> {
        let mut probe = match self.lost_write_probes.remove(p) {
            Some(still_running) if !still_running.is_finished() => still_running,
            // A probe that answered after its wait is stale: ask again.
            _ => {
                let (sandbox, path) = (Arc::clone(&self.sandbox), p.to_path_buf());
                tokio::spawn(async move {
                    let m = sandbox.metadata(&path).await.ok()?;
                    Some((m.mtime, m.len))
                })
            }
        };
        match tokio::time::timeout(Duration::from_millis(100), &mut probe).await {
            Ok(answer) => answer.ok().flatten(),
            Err(_) => {
                self.lost_write_probes.insert(p.to_path_buf(), probe);
                None
            }
        }
    }

    /// Whether the call may run: `Ok` if so, with the file bases its approved preview saw
    /// (when it was asked and has a preview), else how it ended without running.
    async fn gate(
        &mut self,
        p: &Planned<'_>,
        turn: u32,
        ctl: &TurnControl,
    ) -> Result<Option<Bases>, Outcome> {
        if let Some(r) = &p.rejected {
            return Err(Outcome::refused(r.clone()));
        }
        if ctl.cancel.is_cancelled() {
            return Err(Outcome::cancelled());
        }
        let Some((tool, effect)) = p.runnable.as_ref() else {
            return Ok(None);
        };
        let args = p.call.input();
        if let Some(by) = self.ledger.check(&p.call.name, &args) {
            return Err(Outcome {
                result: ToolResult::error(by.message()),
                blocked: true,
                ran: false,
            });
        }
        let raw_cmd = args.get("command").and_then(Value::as_str);
        let raw_cmd = raw_cmd.filter(|_| p.call.name == "bash");
        match self.policy.decide(&p.call.name, effect, raw_cmd) {
            Decision::Allow => Ok(None),
            Decision::Deny { rule } => Err(Outcome::refused(ToolResult::error(format!(
                "denied by the permission rule `{rule}`; the call did not run"
            )))),
            Decision::Refuse { reason } => Err(Outcome::refused(ToolResult::error(format!(
                "refused ({reason}); the call did not run"
            )))),
            Decision::Ask { reason } => {
                let (bases, rule) = self.ask(p.call, tool, &args, reason, turn, ctl).await?;
                if let Some(rule) = rule {
                    self.add_session_rule(&rule, &p.call.name, effect, raw_cmd)
                        .await;
                }
                Ok(bases)
            }
        }
    }

    /// "Allow for this session" (spec 7.11.8): `rule` becomes a session allow rule only if
    /// it matches the call it came with; otherwise that call is approved once and the user
    /// is told. Deny rules, protected paths and unparseable commands still come first.
    async fn add_session_rule(
        &mut self,
        rule: &str,
        tool: &str,
        effect: &Effect,
        raw_cmd: Option<&str>,
    ) {
        let shown = crate::printable(rule);
        let why = match self.policy.rule_matches(rule, tool, effect, raw_cmd) {
            Ok(true) => match self.policy.add_session_allow(rule) {
                Ok(()) => {
                    let note = if broad_rule(rule) {
                        " (broad: it matches far more than this call; deny rules, protected \
                         paths and unparseable commands still ask or refuse)"
                            .to_string()
                    } else {
                        String::new()
                    };
                    self.emit(Event::Warning {
                        message: format!("allowed for this session: {shown}{note}"),
                    })
                    .await;
                    return;
                }
                Err(e) => e.to_string(),
            },
            Ok(false) => format!("it does not match this `{}` call", crate::printable(tool)),
            Err(e) => e.to_string(),
        };
        self.emit(Event::Warning {
            message: format!(
                "session rule `{shown}` not added: {}; the call was approved once",
                crate::printable(&why)
            ),
        })
        .await;
    }

    /// Preview the call, then ask the user (interactive) or deny (headless, spec 3.4). A
    /// call whose preview fails is answered with that failure without asking, since it
    /// would change nothing (spec 7.11.4).
    async fn ask(
        &self,
        call: &Call,
        tool: &Arc<dyn Tool>,
        args: &Value,
        reason: String,
        turn: u32,
        ctl: &TurnControl,
    ) -> Result<(Option<Bases>, Option<String>), Outcome> {
        let (diff, bases) = match self.preview(tool, args, turn, ctl).await? {
            Some(Ok(preview)) => (Some(preview.diff), Some(preview.bases)),
            Some(Err(result)) => {
                return Err(Outcome {
                    result,
                    blocked: false,
                    ran: true,
                });
            }
            None => (None, None),
        };
        let Some((request_id, rx)) = ctl.approvals.open(ApprovalFor::Tool) else {
            let hint = headless_hint(self.policy.mode(), &reason);
            return Err(Outcome::refused(ToolResult::error(format!(
                "denied: this call needs approval ({reason}), and nobody can approve it in \
                 this session (headless); {hint}"
            ))));
        };
        self.log_approval(SessionRecord::ApprovalRequested {
            request_id: request_id.clone(),
            kind: ApprovalKind::Tool,
            call_id: Some(call.id.clone()),
            reason: reason.clone(),
        })
        .await;
        self.emit(Event::ToolApprovalRequested {
            request_id: request_id.clone(),
            call_id: call.id.clone(),
            tool: call.name.clone(),
            args: args.clone(),
            reason,
            diff,
        })
        .await;
        let verdict = tokio::select! {
            v = rx => v.unwrap_or(Verdict::Denied { reason: None }),
            () = ctl.cancel.cancelled() => {
                ctl.approvals.withdraw(&request_id);
                self.emit(Event::ApprovalWithdrawn { request_id }).await;
                return Err(Outcome::cancelled());
            }
        };
        match verdict {
            Verdict::Approved { session_rule } => Ok((bases, session_rule)),
            Verdict::Denied { reason } => Err(Outcome::refused(ToolResult::error(match reason {
                Some(r) => format!("denied by user: {r}"),
                None => "denied by user".to_string(),
            }))),
        }
    }

    /// The call's [`Tool::preview`]. A panicking preview becomes an error result; a cancel
    /// ends the call.
    async fn preview(
        &self,
        tool: &Arc<dyn Tool>,
        args: &Value,
        turn: u32,
        ctl: &TurnControl,
    ) -> Result<Option<Result<Preview, ToolResult>>, Outcome> {
        let mut cx = self.tool_cx(turn, ctl);
        // Headless, nobody sees the diff: the preview only says whether the call fails.
        if !ctl.approvals.is_interactive() {
            cx = cx.without_preview_diff();
        }
        let preview =
            AssertUnwindSafe(tool.preview(args, self.sandbox.as_ref(), &cx)).catch_unwind();
        tokio::select! {
            p = preview => Ok(p.unwrap_or_else(|_| {
                Some(Err(ToolResult::error(
                    "the tool's preview crashed (an engine bug); nothing was changed",
                )))
            })),
            () = ctl.cancel.cancelled() => Err(Outcome::cancelled()),
        }
    }

    /// The context a call of step `turn` runs (or is previewed) with.
    fn tool_cx(&self, turn: u32, ctl: &TurnControl) -> ToolCx {
        let mut cx = ToolCx::new(self.info.caps.edit_format, self.root().join(SPILL_DIR))
            .with_reads(Arc::clone(&self.reads))
            .with_cancel(ctl.cancel.child_token())
            .with_turn(turn)
            .with_process_groups(self.groups.clone())
            .with_env_remove(crate::mcp::provider_key_vars(&self.setup.config))
            .with_side(Arc::clone(&self.side.runner));
        if let Some(index) = self.workspace.index() {
            cx = cx.with_index(index);
        }
        if let Some(memory) = self.workspace.memory() {
            cx = cx.with_memory(memory);
        }
        cx
    }

    /// Run one admitted call. Output lines are forwarded as `ToolOutputDelta` and all of
    /// them are out before this returns. A panicking tool becomes an error result.
    async fn invoke(
        &self,
        p: &Planned<'_>,
        bases: Option<&Bases>,
        turn: u32,
        ctl: &TurnControl,
    ) -> ToolResult {
        let (tool, effect) = p.runnable.as_ref().expect("admitted calls are runnable");
        let call = p.call;
        self.before_exec(&call.name, effect, turn, &ctl.cancel)
            .await;
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
        let mut cx = self.tool_cx(turn, ctl).with_output_tx(tx);
        if let Some(bases) = bases {
            cx = cx.with_approved_bases(bases.clone());
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
    /// `ToolFinished` (then a `remember` call's `MemoryProposed`), to be shown once the run
    /// is checkpointed.
    async fn finish(&mut self, call: &Call, outcome: Outcome, turn: u32) -> (Content, Vec<Event>) {
        let Outcome {
            mut result,
            blocked,
            ran,
        } = outcome;
        let args = call.input();
        let mut memory_events = Vec::new();
        if ran && !result.is_error {
            match parse_context_op(&call.name, &args) {
                // Only a `remember` whose result is not an error is proposed.
                Some(ContextOp::Remember(draft)) => {
                    memory_events = self.remember_call(draft, &mut result).await;
                }
                None => {}
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
            self.note_memory_effects(&result.effects);
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
        if ran && let Some(r) = &mut self.receipt {
            r.tool(&result.meta);
        }
        let event = Event::ToolFinished {
            call_id: call.id.clone(),
            tool: call.name.clone(),
            output: result.output.clone(),
            is_error: result.is_error,
            blocked,
            effects: result.effects.iter().map(EffectView::from).collect(),
            meta: (&result.meta).into(),
            diff: result.applied_diff.as_deref().cloned(),
        };
        let content = Content::ToolResult {
            id: call.id.clone(),
            output: result.output.clone(),
            is_error: result.is_error,
        };
        let id = call.id.clone();
        let root = self.root().to_path_buf();
        let how = Ran { ran, blocked };
        if let Some(skipped) = self
            .with_log(move |log| persist_result(log, &root, id, result, how, turn))
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
        let mut events = vec![event];
        events.extend(memory_events);
        (content, events)
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
    /// Paths a record cannot hold (outside the root, or not valid UTF-8).
    untracked: Vec<PathBuf>,
    /// Changed files whose pre-image reference is unusable: no `Edit` record, rather than
    /// one that says the file was created.
    bad_pre_images: Vec<PathBuf>,
}

/// The path an effect names.
fn effect_path(effect: &AppliedEffect) -> Option<&Path> {
    match effect {
        AppliedEffect::Read { path, .. }
        | AppliedEffect::Edited { path, .. }
        | AppliedEffect::Deleted { path, .. } => Some(path),
        AppliedEffect::Exec { .. } => None,
    }
}

/// What an edit wrote, read back from disk: only when it still hashes to `post_hash`, so
/// the blob is exactly the tool's output and never a later change by someone else. Only a
/// regular file is read, never through a symlink, never blocking (a FIFO swapped in), and
/// never more than its size at open plus one byte.
fn post_image(path: &Path, post_hash: &blake3::Hash) -> Option<Vec<u8>> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() {
        return None;
    }
    let mut bytes = Vec::with_capacity(usize::try_from(meta.len()).ok()?);
    file.take(meta.len().saturating_add(1))
        .read_to_end(&mut bytes)
        .ok()?;
    (blake3::hash(&bytes) == *post_hash).then_some(bytes)
}

/// The `Edit` record of a changed file at step `step`: None for reads, commands and
/// effects this version does not know, `Some(None)` when its pre-image reference is
/// unusable.
fn edit_record(e: &PersistedEffect, step: u32) -> Option<Option<SessionRecord>> {
    let (path, pre, post) = match e {
        PersistedEffect::Edited {
            path,
            pre_image_blob,
            post_image_blob,
            ..
        } => (path, pre_image_blob.as_deref(), post_image_blob.clone()),
        PersistedEffect::Deleted {
            path,
            pre_image_blob,
            ..
        } => (path, Some(pre_image_blob.as_str()), None),
        PersistedEffect::Read { .. } | PersistedEffect::Exec { .. } | PersistedEffect::Unknown => {
            return None;
        }
    };
    let Ok(pre_image) = pre.map(BlobRef::parse).transpose() else {
        return Some(None);
    };
    Some(Some(SessionRecord::Edit {
        path: path.clone(),
        pre_image,
        post_image_blob: post,
        step,
        asserted: false,
    }))
}

/// Whether a call ran or the loop breaker refused it, as its `ToolResult` record says.
struct Ran {
    ran: bool,
    blocked: bool,
}

/// Log a result with its effects (paths relative to `root`, file contents as blobs, paths a
/// record cannot hold left out) and an `Edit` record per changed file, with the content
/// it had before and after. Buffered: the caller checkpoints. Blocking file I/O.
fn persist_result(
    log: &Log,
    root: &Path,
    id: String,
    result: ToolResult,
    how: Ran,
    turn: u32,
) -> Result<Skipped, LogFailure> {
    let result_blob = log.blob(result.output.as_bytes())?;
    let mut effects = Vec::with_capacity(result.effects.len());
    let mut untracked = Vec::new();
    let mut edits = Vec::new();
    let mut bad_pre_images = Vec::new();
    for e in &result.effects {
        let mut put = |bytes: &[u8]| log.blob(bytes).map(|b| b.as_str().to_string());
        let Some(mut persisted) = PersistedEffect::from_applied(e, root, &mut put)? else {
            untracked.extend(effect_path(e).map(Path::to_path_buf));
            continue;
        };
        if let (
            PersistedEffect::Edited {
                post_image_blob, ..
            },
            AppliedEffect::Edited {
                path, post_hash, ..
            },
        ) = (&mut persisted, e)
            && let Some(bytes) = post_image(path, post_hash)
        {
            *post_image_blob = Some(log.blob(&bytes)?);
        }
        match edit_record(&persisted, turn) {
            Some(Some(rec)) => edits.push(rec),
            Some(None) => bad_pre_images.extend(effect_path(e).map(Path::to_path_buf)),
            None => {}
        }
        effects.push(persisted);
    }
    log.append(&SessionRecord::ToolResult {
        id,
        result_blob,
        is_error: result.is_error,
        effects,
        meta: result.meta,
        ran: how.ran,
        blocked: how.blocked,
    })?;
    for rec in &edits {
        log.append(rec)?;
    }
    Ok(Skipped {
        untracked,
        bad_pre_images,
    })
}

/// Whether a session rule matches far more than one call, so its acceptance notice says
/// so: a bare tool (`bash`), a pattern of only wildcards and spaces (`bash(*)`,
/// `bash(* *)`, `bash(?*)`), or an MCP rule with a part of only wildcards (`mcp__x__*`,
/// `mcp__*`).
fn broad_rule(rule: &str) -> bool {
    let wild = |s: &str| s.chars().all(|c| matches!(c, '*' | '?' | ' '));
    if let Some(rest) = rule.strip_prefix("mcp__") {
        return rest.split("__").any(|part| !part.is_empty() && wild(part));
    }
    match rule.split_once('(') {
        None => true,
        Some((_, p)) => wild(p.strip_suffix(')').unwrap_or(p)),
    }
}

/// What a headless denial tells the user to do: only what would let this call run.
fn headless_hint(mode: Mode, reason: &str) -> &'static str {
    if reason.starts_with("protected path") || reason.starts_with("unparseable command") {
        return "protected paths and unparseable commands always ask, so only an interactive \
                session can run it";
    }
    match mode {
        Mode::Ask => {
            "the user can allow it with --approve auto-edit (edits) or yolo (commands too), \
             or an allow rule"
        }
        Mode::AutoEdit => "the user can allow it with --approve yolo or an allow rule",
        Mode::Yolo => "the user can allow it with an allow rule",
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn headless_hint_names_only_what_helps() {
        let h = super::headless_hint;
        assert!(h(Mode::AutoEdit, "protected path: Cargo.toml").contains("interactive"));
        assert!(h(Mode::Ask, "unparseable command: x").contains("interactive"));
        assert!(!h(Mode::AutoEdit, "command requires approval").contains("auto-edit"));
        assert!(h(Mode::AutoEdit, "command requires approval").contains("yolo"));
        assert!(h(Mode::Ask, "write requires approval").contains("auto-edit"));
    }

    /// Re-review T30 (M1 nit): rules that match far more than one call are flagged.
    #[test]
    fn broad_rules_are_recognized() {
        for broad in [
            "bash",
            "bash(*)",
            "bash(**)",
            "bash(* *)",
            "bash(?*)",
            "mcp__x__*",
            "mcp__*",
            "edit",
            "edit(*)",
        ] {
            assert!(super::broad_rule(broad), "{broad}");
        }
        for narrow in [
            "bash(cargo test*)",
            "bash(ls)",
            "mcp__x__create_issue",
            "mcp__x__create_*",
            "edit(src/*.rs)",
            "web_fetch(docs.rs)",
        ] {
            assert!(!super::broad_rule(narrow), "{narrow}");
        }
    }

    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::sync::mpsc;

    use super::*;

    /// `post_image` on another thread, given at most 2 s.
    fn read_back(path: &Path, hash: blake3::Hash) -> Result<Option<Vec<u8>>, &'static str> {
        let (tx, rx) = mpsc::channel();
        let path = path.to_path_buf();
        std::thread::spawn(move || {
            let _ = tx.send(post_image(&path, &hash));
        });
        rx.recv_timeout(Duration::from_secs(2))
            .map_err(|_| "the read-back blocked")
    }

    #[test]
    fn post_images_are_read_only_from_regular_files_that_match() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, b"written").unwrap();
        let hash = blake3::hash(b"written");
        assert_eq!(read_back(&file, hash), Ok(Some(b"written".to_vec())));
        // Changed since: not the tool's output.
        assert_eq!(read_back(&file, blake3::hash(b"other")), Ok(None));
        // A symlink swapped in, even to the right content, is not followed.
        let link = dir.path().join("link.txt");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        assert_eq!(read_back(&link, hash), Ok(None));
        // A FIFO swapped in must neither block nor be read.
        let fifo = dir.path().join("fifo");
        let c = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: plain syscall with a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        assert_eq!(read_back(&fifo, hash), Ok(None));
    }
}
