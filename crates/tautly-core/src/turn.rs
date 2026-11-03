//! One user message: the step loop (spec 3.1). A step is one provider request and the
//! tool calls of its response; it is also one context `Turn` (checkpoint 3), numbered by
//! `Context::next_turn()` read before the step's turn is pushed. `max_turns` counts steps;
//! `TurnStarted`/`TurnFinished` bracket the whole message.
//!
//! Every step starts with the cache epoch (spec 7.7): collection and threshold compaction
//! are decided together and applied together, with the session pins and a pending memory
//! index riding on the same epoch, so the cached prefix breaks at most once.
//!
//! Every provider response's usage (main and side, including a response that broke off
//! and was sent again) is written to the cost ledger, shown as `Usage` and counted against
//! the budgets (`budget.rs`). The budgets are checked at every step start, before every
//! request and at every tool boundary. The turn timeout (`turn_timeout_s`) cancels the
//! turn like `Cancel` does, and the turn ends with `timeout`.

use std::sync::Arc;
use std::sync::PoisonError;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use futures::StreamExt;
use tautly_context::compact::DEFAULT_KEEP_RECENT;
use tautly_context::schedule::{self, MutationReason};
use tautly_context::{CompactError, ContextStatus, GcRecord, TokenEstimator, Turn};
use tautly_provider::{
    CanonicalRequest, Content, Message, ModelId, Price, ProviderError, ProviderEvent, Quota,
    RawUsage, Role, StopReason, cost_usd,
};
use tautly_session::RelPath;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::approvals::Approvals;
use crate::ledger::Row;
use crate::model_switch::rejects_reasoning;
use crate::protocol::{BudgetScope, Command, ErrorKind, Event, FinishReason, UsageRole};
use crate::provider_setup::provider_label;
use crate::record::SessionRecord;
use crate::routing::{Side, model_failed};
use crate::session::Session;

/// A response that breaks off after content is sent again at most this many times.
const MAX_REISSUES: u32 = 4;

/// What a turn task gets from the actor.
pub(crate) struct TurnControl {
    pub cancel: CancellationToken,
    /// `Pin`/`Unpin` sent while the turn runs, applied at the next step start.
    pub inbox: mpsc::UnboundedReceiver<Command>,
    pub approvals: Approvals,
}

/// A tool call of a completed response.
#[derive(Debug, Clone)]
pub(crate) struct Call {
    pub id: String,
    pub name: String,
    /// The parsed arguments (a JSON object), or why they are unusable.
    pub args: Result<serde_json::Value, String>,
}

impl Call {
    /// The arguments as the history holds them: an empty object when unusable.
    pub fn input(&self) -> serde_json::Value {
        self.args
            .clone()
            .unwrap_or_else(|_| serde_json::Value::Object(Default::default()))
    }
}

/// A completed provider response.
#[derive(Debug, Default)]
struct Response {
    content: Vec<Content>,
    calls: Vec<Call>,
    /// The events as logged: consecutive deltas merged.
    logged: Vec<ProviderEvent>,
    usage: Option<RawUsage>,
    /// Usage of attempts the retry wrapper discarded.
    discarded: Vec<RawUsage>,
    quota: Option<Quota>,
    stop: Option<StopReason>,
}

impl Response {
    fn push(&mut self, ev: ProviderEvent) {
        match &ev {
            ProviderEvent::TextDelta(t) => match self.content.last_mut() {
                Some(Content::Text(s)) => s.push_str(t),
                _ => self.content.push(Content::Text(t.clone())),
            },
            ProviderEvent::ReasoningDelta(t) => match self.content.last_mut() {
                Some(Content::Reasoning {
                    text,
                    signature: None,
                }) => text.push_str(t),
                _ => self.content.push(Content::Reasoning {
                    text: t.clone(),
                    signature: None,
                }),
            },
            // A signature belongs to the reasoning just streamed, if it is still unsigned.
            // Otherwise it stands alone (Gemini signs function calls and parts that carry
            // no thought text), as an empty reasoning item the adapter sends back.
            ProviderEvent::ReasoningSignature(sig) => match self.content.last_mut() {
                Some(Content::Reasoning {
                    signature: signature @ None,
                    ..
                }) => *signature = Some(sig.clone()),
                _ => self.content.push(Content::Reasoning {
                    text: String::new(),
                    signature: Some(sig.clone()),
                }),
            },
            ProviderEvent::RedactedReasoning(data) => self
                .content
                .push(Content::RedactedReasoning { data: data.clone() }),
            ProviderEvent::ToolCall {
                id,
                name,
                input_json,
            } => {
                let call = Call {
                    id: id.clone(),
                    name: name.clone(),
                    args: parse_args(input_json),
                };
                self.content.push(Content::ToolUse {
                    id: id.clone(),
                    name: name.clone(),
                    input: call.input(),
                });
                self.calls.push(call);
            }
            ProviderEvent::Usage(u) => self.usage = Some(*u),
            ProviderEvent::DiscardedUsage(u) => self.discarded.push(*u),
            ProviderEvent::Quota(q) => self.quota = Some(q.clone()),
            ProviderEvent::Done { stop } => self.stop = Some(stop.clone()),
        }
        match (self.logged.last_mut(), ev) {
            (Some(ProviderEvent::TextDelta(a)), ProviderEvent::TextDelta(b))
            | (Some(ProviderEvent::ReasoningDelta(a)), ProviderEvent::ReasoningDelta(b)) => {
                a.push_str(&b)
            }
            (_, ev) => self.logged.push(ev),
        }
    }

    /// What the response streamed so far is worth in output tokens (an estimate, for an
    /// attempt that ended before the provider reported its usage).
    fn streamed_tokens(&self, tokens: &TokenEstimator) -> u64 {
        let mut text = String::new();
        for c in &self.content {
            match c {
                Content::Text(t) | Content::Reasoning { text: t, .. } => text.push_str(t),
                Content::RedactedReasoning { data } => text.push_str(data),
                Content::ToolUse { name, input, .. } => {
                    text.push_str(name);
                    text.push_str(&input.to_string());
                }
                _ => {}
            }
        }
        u64::from(tokens.estimate(&text))
    }

    /// A response cut off (output limit) right after a reasoning item keeps no dangling
    /// reasoning: providers reject a reasoning item that leads nowhere.
    fn finish(mut self) -> Self {
        if self.stop == Some(StopReason::MaxTokens)
            && matches!(
                self.content.last(),
                Some(Content::Reasoning { .. } | Content::RedactedReasoning { .. })
            )
        {
            self.content.pop();
        }
        self
    }
}

fn parse_args(input_json: &str) -> Result<serde_json::Value, String> {
    if input_json.trim().is_empty() {
        return Ok(serde_json::Value::Object(Default::default()));
    }
    match serde_json::from_str::<serde_json::Value>(input_json) {
        Ok(v @ serde_json::Value::Object(_)) => Ok(v),
        Ok(_) => Err("the arguments must be a JSON object".into()),
        Err(e) => Err(format!("the arguments are not valid JSON ({e})")),
    }
}

/// The usage to count for an attempt that ended early (broke off, was cancelled): what the
/// provider reported, with the output raised to what was streamed, or else an estimate of
/// the request's input and the streamed output. True when any of it is estimated.
pub(crate) fn partial_usage(
    reported: Option<RawUsage>,
    est_input: u64,
    est_output: u64,
) -> (RawUsage, bool) {
    match reported {
        Some(u) => (
            RawUsage {
                output: u.output.max(est_output),
                ..u
            },
            est_output > u.output,
        ),
        None => (
            RawUsage {
                input: est_input,
                output: est_output,
                cache_read: 0,
                cache_write: 0,
            },
            true,
        ),
    }
}

/// How the stream of one attempt ended.
enum Ended {
    Done,
    Cancelled,
    Failed(ProviderError),
}

/// Why streaming a response stopped without one.
enum StreamEnd {
    Cancelled,
    Failed(ProviderError),
    /// A discarded attempt used up the budget; it is not sent again.
    Budget,
}

fn error_event(e: &ProviderError) -> Event {
    let (kind, hint) = match e {
        ProviderError::Transient { .. } => (
            ErrorKind::ProviderTransient,
            Some("the provider is unavailable or rate-limited; try again shortly".to_string()),
        ),
        ProviderError::Auth(_) => (
            ErrorKind::ProviderFatal,
            Some("check the API key for this provider".to_string()),
        ),
        ProviderError::InvalidRequest(_) => (ErrorKind::ProviderFatal, None),
        ProviderError::ContextOverflow(_) => (
            ErrorKind::ContextOverflow,
            Some("the context is too large for the model; compact it".to_string()),
        ),
        ProviderError::Other(_) => (ErrorKind::ProviderFatal, None),
    };
    Event::Error {
        kind,
        message: e.to_string(),
        retryable: e.is_transient(),
        hint,
    }
}

impl Session {
    /// Run one user message to its end.
    pub async fn run_turn(&mut self, text: String, ctl: &mut TurnControl) {
        let unbound = self.events.clone();
        self.events = unbound.bound(ctl.cancel.clone());
        self.run_turn_inner(text, ctl).await;
        self.events = unbound;
    }

    async fn run_turn_inner(&mut self, text: String, ctl: &mut TurnControl) {
        let timed_out = Arc::new(AtomicBool::new(false));
        let timer = self.turn_timeout.map(|limit| {
            let (cancel, timed_out) = (ctl.cancel.clone(), Arc::clone(&timed_out));
            tokio::spawn(async move {
                tokio::select! {
                    () = tokio::time::sleep(limit) => {
                        timed_out.store(true, Ordering::Relaxed);
                        cancel.cancel();
                    }
                    () = cancel.cancelled() => {}
                }
            })
        });
        self.apply_startup().await;
        self.evidence = self.evidence.fresh();
        self.user_turn += 1;
        self.emit(Event::TurnStarted).await;
        // The tool set changes only here, so the turn's requests share one prefix.
        self.refresh_mcp_tools().await;
        self.budgets
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .start_turn();
        self.context.history.push(Turn {
            messages: vec![Message {
                role: Role::User,
                content: vec![Content::Text(text)],
            }],
        });
        let mut reason = self.steps(ctl).await;
        if let Some(t) = timer {
            t.abort();
        }
        if reason == FinishReason::Cancelled && timed_out.load(Ordering::Relaxed) {
            reason = FinishReason::Timeout;
        }
        let next = self.evidence.fresh();
        let evidence = std::mem::replace(&mut self.evidence, next).finish();
        self.log(SessionRecord::TurnEnd {
            reason,
            evidence: evidence.clone(),
        })
        .await;
        self.checkpoint().await;
        self.emit(Event::TurnFinished { reason, evidence }).await;
    }

    async fn steps(&mut self, ctl: &mut TurnControl) -> FinishReason {
        let mut steps = 0u32;
        loop {
            if ctl.cancel.is_cancelled() {
                return FinishReason::Cancelled;
            }
            if steps >= self.max_turns {
                return FinishReason::MaxTurns;
            }
            if self.budget_exhausted().await.is_some() {
                return FinishReason::Budget;
            }
            while let Ok(cmd) = ctl.inbox.try_recv() {
                self.context_command(cmd).await;
            }
            self.check_lost_writes().await;
            self.check_index().await;
            // Reasoning is provider-specific (a switch strips it already; this holds the
            // invariant for any other way the model changes).
            self.context
                .strip_reasoning(Some(&provider_label(&self.model)));
            let (req, status) = self.epoch(ctl).await;
            if ctl.cancel.is_cancelled() {
                return FinishReason::Cancelled;
            }
            // The epoch's compaction summary may have spent the rest.
            if self.budget_exhausted().await.is_some() {
                return FinishReason::Budget;
            }
            let turn = self.context.next_turn();
            self.log_request(&req, turn).await;
            let sent_at = Instant::now();
            let retry_without_reasoning = std::mem::take(&mut self.reasoning_retry);
            let resp = match self.stream(req, u64::from(status.used), ctl).await {
                Ok(r) => r,
                Err(StreamEnd::Cancelled) => return FinishReason::Cancelled,
                Err(StreamEnd::Budget) => return FinishReason::Budget,
                // The first request after a switch within one provider: the new model may
                // refuse the old one's reasoning. Strip it and send the step once more.
                Err(StreamEnd::Failed(e)) if retry_without_reasoning && rejects_reasoning(&e) => {
                    self.context.strip_reasoning(None);
                    // Logged, so a rebuild strips at the same point (`record.rs`).
                    self.log(SessionRecord::ReasoningStripped).await;
                    self.warn(format!(
                        "the new model refused the reasoning carried over from the previous \
                         one ({e}); sending the request again without it"
                    ))
                    .await;
                    continue;
                }
                Err(StreamEnd::Failed(e)) => {
                    self.emit(error_event(&e)).await;
                    return FinishReason::Error;
                }
            };
            self.log_all(
                resp.logged
                    .iter()
                    .cloned()
                    .map(SessionRecord::Provider)
                    .collect(),
            )
            .await;
            self.context.note_request(sent_at, status.cached_prefix);
            if let Some(u) = resp.usage {
                self.context.calibrate(&status, &u);
                self.spend(UsageRole::Main, u, resp.quota.clone(), false)
                    .await;
            }
            self.emit(Event::context_status(&status)).await;
            steps += 1;
            let assistant = Message {
                role: Role::Assistant,
                content: resp.content,
            };
            if resp.calls.is_empty() {
                if !assistant.content.is_empty() {
                    self.context.history.push(Turn {
                        messages: vec![assistant],
                    });
                }
                self.checkpoint().await;
                return FinishReason::Completed;
            }
            self.log_all(
                resp.calls
                    .iter()
                    .map(|c| SessionRecord::ToolCall {
                        id: c.id.clone(),
                        name: c.name.clone(),
                        args: c.input(),
                    })
                    .collect(),
            )
            .await;
            self.checkpoint().await;
            for c in &resp.calls {
                self.emit(Event::ToolCallProposed {
                    call_id: c.id.clone(),
                    tool: c.name.clone(),
                    args: c.input(),
                })
                .await;
            }
            let results = self.run_calls(&resp.calls, turn, ctl).await;
            self.context.history.push(Turn {
                messages: vec![
                    assistant,
                    Message {
                        role: Role::User,
                        content: results,
                    },
                ],
            });
            self.checkpoint().await;
        }
    }

    /// Report a failed index refresh once.
    async fn check_index(&mut self) {
        if self.index_error_reported {
            return;
        }
        if let Some(e) = self.workspace.index().and_then(|i| i.last_error()) {
            self.index_error_reported = true;
            self.warn(format!("code index refresh failed: {e}")).await;
        }
    }

    fn assemble(&self) -> (CanonicalRequest, ContextStatus) {
        self.context
            .assemble(&self.info.caps, &self.specs, &self.model)
    }

    /// Stream one response. Text and reasoning are shown as they arrive; tool calls are
    /// used only once the response is complete. A response that breaks off with a
    /// transient error after content (the retry wrapper cannot hide that) is discarded and
    /// the request sent again, so a partial tool call never runs.
    ///
    /// Every attempt's usage counts (`est_input` is the request's estimated size): what the
    /// retry wrapper discarded, and for an attempt that broke off or was cancelled, what it
    /// reported or else an estimate.
    async fn stream(
        &mut self,
        req: CanonicalRequest,
        est_input: u64,
        ctl: &TurnControl,
    ) -> Result<Response, StreamEnd> {
        let mut attempt = 0;
        loop {
            let mut s = self.provider.stream(req.clone(), ctl.cancel.child_token());
            let mut resp = Response::default();
            // Mirrors `Retrying`: usage and quota alone are not content.
            let mut content = false;
            let ended = loop {
                let item = tokio::select! {
                    biased;
                    () = ctl.cancel.cancelled() => break Ended::Cancelled,
                    item = s.next() => item,
                };
                match item {
                    None => break Ended::Done,
                    Some(Ok(ev)) => {
                        match &ev {
                            ProviderEvent::TextDelta(t) => {
                                self.emit(Event::TextDelta { text: t.clone() }).await
                            }
                            ProviderEvent::ReasoningDelta(t) => {
                                self.emit(Event::ReasoningDelta { text: t.clone() }).await
                            }
                            _ => {}
                        }
                        content |= !matches!(
                            ev,
                            ProviderEvent::Usage(_)
                                | ProviderEvent::Quota(_)
                                | ProviderEvent::DiscardedUsage(_)
                        );
                        resp.push(ev);
                    }
                    Some(Err(e)) => break Ended::Failed(e),
                }
            };
            for u in std::mem::take(&mut resp.discarded) {
                self.spend(UsageRole::Main, u, None, false).await;
            }
            let e = match ended {
                Ended::Done => return Ok(resp.finish()),
                Ended::Cancelled => {
                    self.spend_partial(UsageRole::Main, &resp, est_input).await;
                    return Err(StreamEnd::Cancelled);
                }
                Ended::Failed(e) => e,
            };
            // What the provider billed for the broken attempt counts, so cost is never
            // under-reported.
            self.spend_partial(UsageRole::Main, &resp, est_input).await;
            // Before any content, `Retrying` has already retried (spec 3.5): its error is
            // final. Only a response that broke off after content is sent again, and not
            // once the budget is used up.
            if !(content && e.is_transient() && attempt < MAX_REISSUES) {
                return Err(StreamEnd::Failed(e));
            }
            if self.budget_exhausted().await.is_some() {
                return Err(StreamEnd::Budget);
            }
            attempt += 1;
            self.warn(format!(
                "the response broke off ({e}); discarding it and sending the request again"
            ))
            .await;
        }
    }

    /// The cache epoch at a step start; returns the request to send.
    async fn epoch(&mut self, ctl: &TurnControl) -> (CanonicalRequest, ContextStatus) {
        let (req, status) = self.assemble();
        let now = Instant::now();
        let ttl = self
            .info
            .cache_ttl_s
            .map(|s| Duration::from_secs(u64::from(s)));
        let price = self.info.price;
        let window = self.info.caps.context_window;
        let gc = self
            .context
            .gc_decision(now, ttl, price.as_ref(), status.used, window);
        let compaction = self.context.compaction_decision(
            &status,
            self.compaction_threshold,
            now,
            ttl,
            price.as_ref(),
        );
        let cold = schedule::decide(
            &self.context.clock,
            now,
            ttl,
            0,
            price.as_ref(),
            status.used,
            window,
        ) == Some(MutationReason::ColdCache);

        let mut applied = false;
        if let Some(rec) = self.context.collect(gc.or(compaction)) {
            self.collected(rec).await;
            applied = true;
        }
        if let Some(reason) = compaction {
            applied |= self.compact(reason, ctl).await;
        }
        if !applied && !cold {
            return (req, status);
        }
        let mut changed = applied;
        if let Some(text) = self.pending_memory_index.take() {
            self.context.memory_index = Some(text.clone()).filter(|t| !t.trim().is_empty());
            self.log(SessionRecord::MemoryIndexApplied { text }).await;
            changed = true;
        }
        if self.context.fold_session_pins() {
            self.log(SessionRecord::PinsFolded).await;
            changed = true;
        }
        if changed {
            self.assemble()
        } else {
            (req, status)
        }
    }

    /// Log and announce a collection, and forget the read baselines it took away.
    async fn collected(&mut self, rec: GcRecord) {
        for p in &rec.forgotten_paths {
            self.reads.forget_path(p);
        }
        let mut paths = Vec::with_capacity(rec.forgotten_paths.len());
        let mut skipped = Vec::new();
        for p in &rec.forgotten_paths {
            match RelPath::from_abs(self.root(), p) {
                Some(rel) => paths.push(rel),
                None => skipped.push(p.clone()),
            }
        }
        self.untracked(skipped).await;
        self.log(SessionRecord::GcApplied {
            elided: rec
                .elided
                .iter()
                .map(|e| (e.call_id.clone(), e.by_turn))
                .collect(),
            reason: rec.reason,
            tokens_freed: rec.tokens_freed,
            forgotten_paths: paths,
        })
        .await;
        self.emit(Event::ContextCollected {
            elided_results: rec.elided.len() as u32,
            tokens_freed: rec.tokens_freed,
            reason: rec.reason,
        })
        .await;
    }

    /// Threshold compaction (spec 6.4): announced first, summarized by the side model
    /// (7.5), and if that fails (or the summary is not smaller) the oldest turns are dropped
    /// without a summary; never retried in a loop. True when the history changed.
    async fn compact(&mut self, reason: MutationReason, ctl: &TurnControl) -> bool {
        // A side model that is no use (bad key, not on the account) is given up for the
        // session and the summary is asked of the main model once.
        let mut fell_back = self.side.model == self.model;
        let mut announced = false;
        let (summary, removed) = loop {
            // The request is sized for the side model's window.
            let (req, removed) = self.context.compaction_request(
                &self.side.model,
                &self.side.info.caps,
                DEFAULT_KEEP_RECENT,
            );
            if removed == 0 {
                return false;
            }
            if !std::mem::replace(&mut announced, true) {
                let (_, mut status) = self.assemble();
                status.warnings.push(format!(
                    "compacting: summarizing the {removed} oldest turns"
                ));
                self.emit(Event::context_status(&status)).await;
            }
            match self.summarize(req, ctl).await {
                Ok(s) => break (Some(s), removed),
                Err(StreamEnd::Cancelled | StreamEnd::Budget) => return false,
                Err(StreamEnd::Failed(e)) if !fell_back && model_failed(&e) => {
                    fell_back = true;
                    self.side_to_main(&e.to_string()).await;
                }
                Err(StreamEnd::Failed(e)) => {
                    self.warn(format!(
                        "the compaction summary failed ({e}); dropping the oldest turns without one"
                    ))
                    .await;
                    break (None, removed);
                }
            }
        };
        let rec = match summary.map(|s| self.context.apply_compaction(removed, s)) {
            Some(Ok(rec)) => Ok(rec),
            Some(Err(CompactError::SummaryTooLong { .. })) | None => {
                self.context.apply_compaction(removed, String::new())
            }
            Some(Err(e)) => Err(e),
        };
        let Ok(rec) = rec else {
            return false;
        };
        self.reads.forget_shown_at(rec.last_removed_turn);
        for p in &rec.forgotten_paths {
            self.reads.forget_path(p);
        }
        let mut blobs = Vec::with_capacity(rec.removed_turns.len());
        for t in &rec.removed_turns {
            match self.blob(&serde_json::to_vec(t).unwrap_or_default()).await {
                Some(b) => blobs.push(b),
                None => break,
            }
        }
        if blobs.len() == rec.removed_turns.len() {
            self.log(SessionRecord::Compaction {
                removed: blobs,
                summary: rec.summary.clone(),
            })
            .await;
        }
        self.emit(Event::Compacted {
            removed_turns: rec.removed_turns.len() as u32,
            summary: rec.summary.clone(),
            reason,
        })
        .await;
        self.latest_compaction = Some(rec);
        true
    }

    /// Run a summary request on the side model and collect its text.
    async fn summarize(
        &mut self,
        req: CanonicalRequest,
        ctl: &TurnControl,
    ) -> Result<String, StreamEnd> {
        let est_input = u64::from(
            self.context
                .tokens
                .estimate(&serde_json::to_string(&req).unwrap_or_default()),
        );
        let mut s = self.side.provider.stream(req, ctl.cancel.child_token());
        let mut text = String::new();
        let mut usage = None;
        let ended = loop {
            let item = tokio::select! {
                biased;
                () = ctl.cancel.cancelled() => break Ended::Cancelled,
                item = s.next() => item,
            };
            match item {
                None => break Ended::Done,
                Some(Ok(ProviderEvent::TextDelta(t))) => text.push_str(&t),
                Some(Ok(ProviderEvent::Usage(u))) => usage = Some(u),
                Some(Ok(ProviderEvent::DiscardedUsage(u))) => {
                    self.spend_side(u, false).await;
                }
                Some(Ok(_)) => {}
                Some(Err(e)) => break Ended::Failed(e),
            }
        };
        if let (Ended::Done, Some(u)) = (&ended, usage) {
            self.spend_side(u, false).await;
            return Ok(text.trim().to_string());
        }
        // Failed or cancelled: what it reported, or an estimate, still counts. A request the
        // provider refused outright (bad key, not available) was never billed.
        let refused = matches!(&ended, Ended::Failed(e) if model_failed(e))
            && usage.is_none()
            && text.is_empty();
        if !refused {
            let est_output = u64::from(self.context.tokens.estimate(&text));
            let (u, estimated) = partial_usage(usage, est_input, est_output);
            self.spend_side(u, estimated).await;
        }
        match ended {
            Ended::Done => Ok(text.trim().to_string()),
            Ended::Cancelled => Err(StreamEnd::Cancelled),
            Ended::Failed(e) => Err(StreamEnd::Failed(e)),
        }
    }

    /// Count an attempt that ended early (see [`partial_usage`]).
    async fn spend_partial(&mut self, role: UsageRole, resp: &Response, est_input: u64) {
        let est_output = resp.streamed_tokens(&self.context.tokens);
        let (u, estimated) = partial_usage(resp.usage, est_input, est_output);
        self.spend(role, u, resp.quota.clone(), estimated).await;
    }

    /// Account for one response of the main model (see [`Session::charge`]).
    async fn spend(&mut self, role: UsageRole, u: RawUsage, quota: Option<Quota>, estimated: bool) {
        let (model, price) = (self.model.clone(), self.info.price);
        self.charge(role, &model, price.as_ref(), u, quota, estimated)
            .await;
    }

    /// Account for one response of the side model: its own id and its own price, never the
    /// main model's (spec 7.5). An unpriced side model costs `None`.
    async fn spend_side(&mut self, u: RawUsage, estimated: bool) {
        let (model, price) = (self.side.model.clone(), self.side.info.price);
        self.charge(UsageRole::Side, &model, price.as_ref(), u, None, estimated)
            .await;
    }

    /// Account for the side-model requests tools made (`web_fetch` answers) since the last
    /// call. Run at tool boundaries, before the call's result is shown, so the budgets know
    /// what a call spent before the next one is admitted.
    pub(crate) async fn drain_side_usage(&mut self) {
        self.drain_usage().await;
        if self.side.model != self.model
            && let Some(why) = self.side.take_failure()
        {
            self.side_to_main(&why).await;
        }
    }

    /// Account what the side model's runner reported, without acting on its failures.
    pub(crate) async fn drain_usage(&mut self) {
        while let Ok(s) = self.side.usage.try_recv() {
            self.spend_side(s.usage, s.estimated).await;
        }
    }

    /// The side model failed for good: the main model does side work from now on, for the
    /// rest of the session. Said once, since afterwards the side model is the main model.
    async fn side_to_main(&mut self, why: &str) {
        self.drain_usage().await;
        self.warn(format!(
            "the side model {} failed ({why}); the main model {} does side work for the rest \
             of the session",
            self.side.model, self.model
        ))
        .await;
        self.side = Side::new(
            Arc::clone(&self.provider),
            self.model.clone(),
            self.info.clone(),
        );
    }

    /// Account for one provider response of `model`: queue its ledger row, emit `Usage`, then
    /// count it against the budgets (whose 80% and exhausted warnings follow the `Usage`).
    async fn charge(
        &mut self,
        role: UsageRole,
        model: &ModelId,
        price: Option<&Price>,
        u: RawUsage,
        quota: Option<Quota>,
        estimated: bool,
    ) {
        let cost = cost_usd(&u, price);
        // Queued for the background writer: never waits (not even on the cancel path).
        let row = Row {
            session: self.session_id.clone(),
            model: model.clone(),
            usage: u,
            cost,
            estimated,
        };
        if let Err(why) = self.cost_ledger.record(row) {
            self.ledger_row_lost(why).await;
        }
        self.emit(Event::Usage {
            role,
            tokens_in: u.input,
            tokens_out: u.output,
            cache_read: u.cache_read,
            cache_write: u.cache_write,
            cost_usd: cost,
            quota,
            estimated,
        })
        .await;
        {
            let mut budgets = self.budgets.lock().unwrap_or_else(PoisonError::into_inner);
            budgets.spend(&u, cost);
            if cost.is_none() {
                budgets.note_unpriced(model.as_str());
            }
        }
        self.budget_exhausted().await;
    }

    /// Show what the budgets have to say, and which one (if any) is used up.
    pub(crate) async fn budget_exhausted(&self) -> Option<BudgetScope> {
        let (news, exceeded) = {
            let mut b = self.budgets.lock().unwrap_or_else(PoisonError::into_inner);
            (b.news(), b.exceeded())
        };
        for w in news {
            self.warn(w).await;
        }
        exceeded
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deltas_merge_and_a_trailing_reasoning_item_is_dropped_when_cut_off() {
        let mut r = Response::default();
        for ev in [
            ProviderEvent::TextDelta("a".into()),
            ProviderEvent::TextDelta("b".into()),
            ProviderEvent::ReasoningDelta("think".into()),
            ProviderEvent::Done {
                stop: StopReason::MaxTokens,
            },
        ] {
            r.push(ev);
        }
        assert_eq!(r.logged.len(), 3);
        let r = r.finish();
        assert_eq!(r.content, vec![Content::Text("ab".into())]);
    }

    fn reasoning(r: &Response) -> Vec<(String, Option<String>)> {
        r.content
            .iter()
            .filter_map(|c| match c {
                Content::Reasoning { text, signature } => Some((text.clone(), signature.clone())),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_signature_without_thought_text_is_kept_as_its_own_item() {
        // Gemini: a signed function call with no thought before it.
        let mut r = Response::default();
        r.push(ProviderEvent::ReasoningSignature("SIG-CALL".into()));
        r.push(ProviderEvent::ToolCall {
            id: "c1".into(),
            name: "read".into(),
            input_json: "{}".into(),
        });
        assert_eq!(reasoning(&r), [(String::new(), Some("SIG-CALL".into()))]);
        assert!(matches!(r.content[1], Content::ToolUse { .. }));
    }

    #[test]
    fn a_call_signature_does_not_overwrite_the_thought_signature() {
        // Gemini: a signed thought, then a signed function call.
        let mut r = Response::default();
        for ev in [
            ProviderEvent::ReasoningDelta("think".into()),
            ProviderEvent::ReasoningSignature("SIG-THOUGHT".into()),
            ProviderEvent::ReasoningSignature("SIG-CALL".into()),
            ProviderEvent::ToolCall {
                id: "c1".into(),
                name: "read".into(),
                input_json: "{}".into(),
            },
        ] {
            r.push(ev);
        }
        assert_eq!(
            reasoning(&r),
            [
                ("think".to_string(), Some("SIG-THOUGHT".to_string())),
                (String::new(), Some("SIG-CALL".to_string())),
            ]
        );
    }

    #[test]
    fn an_anthropic_signature_signs_the_thinking_before_it() {
        let mut r = Response::default();
        for ev in [
            ProviderEvent::ReasoningDelta("a".into()),
            ProviderEvent::ReasoningDelta("b".into()),
            ProviderEvent::ReasoningSignature("SIG".into()),
            ProviderEvent::TextDelta("answer".into()),
        ] {
            r.push(ev);
        }
        assert_eq!(reasoning(&r), [("ab".to_string(), Some("SIG".to_string()))]);
    }

    #[test]
    fn an_early_end_counts_what_was_reported_or_an_estimate() {
        let start = RawUsage {
            input: 1000,
            output: 1,
            cache_read: 50,
            cache_write: 0,
        };
        // Reported input kept; output raised to what was streamed.
        assert_eq!(
            partial_usage(Some(start), 9999, 40),
            (
                RawUsage {
                    output: 40,
                    ..start
                },
                true
            )
        );
        assert_eq!(partial_usage(Some(start), 9999, 0), (start, false));
        // Nothing reported: the request's estimated input.
        let (u, estimated) = partial_usage(None, 3000, 7);
        assert_eq!((u.input, u.output, estimated), (3000, 7, true));
    }

    #[test]
    fn arguments_must_be_a_json_object() {
        assert!(parse_args("").unwrap().is_object());
        assert!(parse_args("[1]").is_err());
        assert!(parse_args("{x").is_err());
        assert!(parse_args(r#"{"a":1}"#).is_ok());
    }
}
