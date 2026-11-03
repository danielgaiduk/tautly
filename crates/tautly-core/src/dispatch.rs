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

use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use futures::FutureExt;
use futures::future::join_all;
use serde_json::Value;
use tautly_provider::Content;
use tautly_sandbox::{Decision, Effect, SPILL_DIR};
use tautly_session::BlobRef;
use tautly_tools::{ContextOp, PersistedEffect, Tool, ToolCx, ToolResult, parse_context_op};
use tokio::sync::mpsc;

use crate::approvals::Verdict;
use crate::protocol::{EffectView, Event};
use crate::record::{SessionRecord, storable, utf8_effects};
use crate::session::Session;
use crate::turn::{Call, TurnControl};

/// Output lines buffered per running tool before they are dropped (bash never blocks).
const OUTPUT_BUFFER: usize = 64;

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
            let outcomes = self.execute(run, turn, ctl).await;
            for (p, o) in run.iter().zip(outcomes) {
                results.push(self.finish(p.call, o, turn).await);
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
        let mut ran = join_all(admitted.iter().map(|p| this.invoke(p, turn, ctl)))
            .await
            .into_iter();
        gated
            .into_iter()
            .map(|g| {
                g.unwrap_or_else(|| Outcome {
                    result: ran.next().expect("one result per admitted call"),
                    blocked: false,
                    ran: true,
                })
            })
            .collect()
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
        let forward = tokio::spawn(async move {
            while let Some(text) = rx.recv().await {
                events
                    .emit(Event::ToolOutputDelta {
                        call_id: call_id.clone(),
                        text,
                    })
                    .await;
            }
        });
        let mut cx = ToolCx::new(self.info.caps.edit_format, self.root().join(SPILL_DIR))
            .with_reads(Arc::clone(&self.reads))
            .with_cancel(ctl.cancel.child_token())
            .with_output_tx(tx)
            .with_turn(turn);
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
        let _ = forward.await;
        result
    }

    /// Settle one call's outcome in model order and return its result for the history.
    async fn finish(&mut self, call: &Call, outcome: Outcome, turn: u32) -> Content {
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
        self.persist_result(call, &result, turn).await;
        self.emit(Event::ToolFinished {
            call_id: call.id.clone(),
            tool: call.name.clone(),
            output: result.output.clone(),
            is_error: result.is_error,
            blocked,
            effects: result.effects.iter().map(EffectView::from).collect(),
            meta: (&result.meta).into(),
        })
        .await;
        Content::ToolResult {
            id: call.id.clone(),
            output: result.output,
            is_error: result.is_error,
        }
    }

    /// Log a result with its effects (file contents as blobs, paths that are not valid
    /// UTF-8 left out with a warning) and an `Edit` record per changed file, then fsync.
    async fn persist_result(&mut self, call: &Call, result: &ToolResult, turn: u32) {
        let Some(result_blob) = self.blob(result.output.as_bytes()).await else {
            return;
        };
        let log = self.log.clone();
        let mut persisted = Vec::with_capacity(result.effects.len());
        for e in &result.effects {
            let mut put = |bytes: &[u8]| log.blob(bytes).map(|b| b.as_str().to_string());
            match PersistedEffect::from_applied(e, &mut put) {
                Ok(p) => persisted.push(p),
                Err(f) => {
                    if let Some(ev) = f.event() {
                        self.emit(ev).await;
                    }
                    return;
                }
            }
        }
        let (effects, skipped) = utf8_effects(persisted);
        self.untracked(skipped).await;
        let edits: Vec<SessionRecord> = effects
            .iter()
            .filter_map(|e| {
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
                    _ => return None,
                };
                debug_assert!(storable(path));
                Some(SessionRecord::Edit {
                    path: path.clone(),
                    pre_image: pre.and_then(|b| BlobRef::parse(b).ok()),
                    turn,
                })
            })
            .collect();
        self.log(SessionRecord::ToolResult {
            id: call.id.clone(),
            result_blob,
            is_error: result.is_error,
            effects,
            meta: result.meta.clone(),
        })
        .await;
        for rec in edits {
            self.log(rec).await;
        }
        self.checkpoint().await;
    }
}
