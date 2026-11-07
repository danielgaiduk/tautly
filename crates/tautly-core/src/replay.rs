//! Playback (spec 7.12.5): a session's logged stream, re-emitted from its log.
//!
//! [`events`] maps each record to the events the live session sent for it:
//!
//! | Record | Event |
//! |---|---|
//! | `TurnStart` | `UserMessage{text}` + `TurnStarted` |
//! | provider text and reasoning | `TextDelta` / `ReasoningDelta` (redacted reasoning skipped) |
//! | `ToolCall` | `ToolCallProposed` |
//! | `ApprovalRequested` (a tool's) | `ToolApprovalRequested` (no `diff`) |
//! | `ToolResult` | `ToolFinished` (output from its blob, no `diff`) |
//! | provider usage, `Charged` | `Usage` |
//! | `Compaction` | `Compacted` |
//! | `GcApplied` | `ContextCollected` |
//! | `MemoryProposal` | `MemoryProposed` |
//! | `ModelSet` | `ModelChanged` |
//! | `TurnEnd` | `TurnFinished` |
//!
//! The user message comes from `TurnStart`, not from the `SendMessage` command: a message
//! sent during another turn is logged when it arrives but starts its turn later, where
//! `TurnStart` is. A response's usage is the last one it reported (providers report it
//! cumulatively) and is sent once the response's records end, after the attempts the retry
//! wrapper discarded, as the live session sent them; each charge is priced by its own
//! model (a response by its step's `Request.model`, a `Charged` by its `model`) with the
//! catalog given to [`Events::priced`], else left unpriced.
//!
//! The log is untrusted input (it may come from another machine): a record path that is
//! absolute, has a `..` component or a NUL byte is refused ([`RelPath::resolve`]) and a
//! warning said instead; records of a type this version does not know are skipped with one
//! warning at the end. Nothing replayed can be answered: approval request and memory
//! proposal ids get the [`REPLAYED_ID_PREFIX`], which no live request has. Escaping for a
//! terminal is the renderer's (`printable`), as for live events.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;
use tautly_context::MutationReason;
use tautly_mcp::printable;
use tautly_provider::{Catalog, ModelId, ProviderEvent, Quota, RawUsage, cost_usd};
use tautly_session::{Entry, RelPath, SessionReader};
use tautly_tools::PersistedEffect;
use tokio::sync::mpsc;

use crate::protocol::{EffectView, ErrorKind, Event, ToolMetaView, UsageRole};
use crate::provider_setup::provider_label;
use crate::record::{ApprovalKind, SessionRecord};
use crate::session::Session;
use crate::timetravel::{StepCounter, TtError, step_at_offset};
use crate::turn::TurnControl;

/// One replayed event: the offset of the record it came from, when that was written (Unix
/// milliseconds), and the event.
pub type Replayed = (u64, u64, Event);

/// The longest pause between two replayed events.
pub const MAX_GAP: Duration = Duration::from_secs(2);

/// The `Error` a `Replay` gets while anything runs (it gets no `ReplayFinished`).
pub const REPLAY_BUSY: &str = "replay needs an idle session";

/// What replayed approval request and memory proposal ids start with: no live id does, so
/// an `Approve` or `ApproveMemory` naming one is never taken for consent.
pub const REPLAYED_ID_PREFIX: &str = "replayed:";

/// The events of `reader`'s log as `(offset, t_ms, event)`: the record each came from and
/// when it was written. With `from_step`, records before step `from_step` (spec 7.12.1
/// numbering: a user message's step starts at its `TurnStart`, a provider step at its
/// `Request`) are skipped; a step past the last one yields nothing. Reads the log and its
/// blobs, nothing else. A record that cannot be read ends the stream with a warning.
pub fn events(reader: &SessionReader<SessionRecord>, from_step: Option<u32>) -> Events<'_> {
    Events {
        reader,
        entries: Box::new(reader.entries()),
        root: reader.header().root.clone(),
        catalog: None,
        from_step: from_step.unwrap_or(0),
        steps: StepCounter::default(),
        calls: HashMap::new(),
        model: reader.header().model.clone().map(ModelId::from),
        response: None,
        unknown: 0,
        last: (0, 0),
        out: VecDeque::new(),
        done: false,
        truncated: false,
    }
}

/// The fastest replay speed accepted.
pub const MAX_SPEED: f32 = 1000.0;

/// A replay speed a client may ask for: a finite number from 0 (instant) to
/// [`MAX_SPEED`]. Anything else is refused where commands come in (cp5 review C I2).
pub fn speed_ok(speed: f32) -> bool {
    (0.0..=MAX_SPEED).contains(&speed)
}

/// The pause before an event written at `t` after one written at `prev` (Unix
/// milliseconds): the gap divided by `speed`, at most [`MAX_GAP`]; none when `speed` is 0
/// (or not a positive number) or the clock went back.
pub fn delay(prev: u64, t: u64, speed: f32) -> Duration {
    if !(speed.is_finite() && speed > 0.0) {
        return Duration::ZERO;
    }
    let gap = t.saturating_sub(prev) as f64 / 1000.0 / f64::from(speed);
    Duration::try_from_secs_f64(gap).map_or(MAX_GAP, |d| d.min(MAX_GAP))
}

/// The usage of the response being read: what it reported last and what the retry
/// wrapper discarded before it, sent when its records end.
struct Response {
    at: (u64, u64),
    usage: Option<RawUsage>,
    discarded: Vec<RawUsage>,
    quota: Option<Quota>,
}

/// See [`events`].
pub struct Events<'a> {
    reader: &'a SessionReader<SessionRecord>,
    entries: Box<dyn Iterator<Item = tautly_session::Result<Entry<SessionRecord>>> + 'a>,
    root: PathBuf,
    catalog: Option<Catalog>,
    from_step: u32,
    steps: StepCounter,
    /// Each tool call's name and arguments, by call id.
    calls: HashMap<String, (String, Value)>,
    /// The model of the latest request (else the session's).
    model: Option<ModelId>,
    response: Option<Response>,
    unknown: u32,
    /// Where the last record read was: the end-of-log warnings go there.
    last: (u64, u64),
    out: VecDeque<Replayed>,
    done: bool,
    /// The stream ended on a record that could not be read.
    truncated: bool,
}

impl Events<'_> {
    /// The stream ended early, on a record that could not be read (a warning said so):
    /// the rest of the log was not replayed.
    pub fn truncated(&self) -> bool {
        self.truncated
    }

    /// Price each charge at its model's price in `catalog` (the configuration's
    /// `effective_catalog`). Without it, `Usage.cost_usd` is none.
    pub fn priced(mut self, catalog: Catalog) -> Self {
        self.catalog = Some(catalog);
        self
    }

    fn cost(&self, model: Option<&ModelId>, u: &RawUsage) -> Option<f64> {
        let price = self.catalog.as_ref()?.get(model?).price;
        cost_usd(u, price.as_ref())
    }

    fn usage(
        &self,
        role: UsageRole,
        model: Option<&ModelId>,
        u: RawUsage,
        quota: Option<Quota>,
        estimated: bool,
    ) -> Event {
        Event::Usage {
            role,
            tokens_in: u.input,
            tokens_out: u.output,
            cache_read: u.cache_read,
            cache_write: u.cache_write,
            cost_usd: self.cost(model, &u),
            quota,
            estimated,
        }
    }

    /// Send the usage of the response whose records just ended.
    fn end_response(&mut self) {
        let Some(r) = self.response.take() else {
            return;
        };
        let (offset, t) = r.at;
        let model = self.model.clone();
        for u in r.discarded {
            let ev = self.usage(UsageRole::Main, model.as_ref(), u, None, false);
            self.out.push_back((offset, t, ev));
        }
        if let Some(u) = r.usage {
            let ev = self.usage(UsageRole::Main, model.as_ref(), u, r.quota, false);
            self.out.push_back((offset, t, ev));
        }
    }

    fn provider(&mut self, at: (u64, u64), ev: ProviderEvent) {
        let r = self.response.get_or_insert_with(|| Response {
            at,
            usage: None,
            discarded: Vec::new(),
            quota: None,
        });
        match ev {
            ProviderEvent::TextDelta(text) => {
                self.out.push_back((at.0, at.1, Event::TextDelta { text }))
            }
            ProviderEvent::ReasoningDelta(text) => {
                self.out
                    .push_back((at.0, at.1, Event::ReasoningDelta { text }))
            }
            ProviderEvent::Usage(u) => {
                r.at = at;
                r.usage = Some(u);
            }
            ProviderEvent::DiscardedUsage(u) => r.discarded.push(u),
            ProviderEvent::Quota(q) => r.quota = Some(q),
            // Signatures and redacted reasoning show nothing; the tool calls are their own
            // records; `Done` and what this version does not know say nothing either.
            ProviderEvent::ReasoningSignature(_)
            | ProviderEvent::RedactedReasoning(_)
            | ProviderEvent::ToolCall { .. }
            | ProviderEvent::Done { .. }
            | ProviderEvent::Unknown => {}
        }
    }

    /// The events of one record (other than a provider event). A record before
    /// `from_step` (`shown` false) only updates what later ones need (call names, the
    /// model): nothing of it is read from the blob store or resolved.
    fn record(&mut self, rec: SessionRecord, shown: bool) -> Vec<Event> {
        match rec {
            SessionRecord::ToolResult { .. } if !shown => Vec::new(),
            SessionRecord::TurnStart { text, .. } => {
                vec![Event::UserMessage { text }, Event::TurnStarted]
            }
            SessionRecord::Request { model, .. } => {
                self.model = Some(model);
                Vec::new()
            }
            SessionRecord::ModelSet { model } => {
                self.model = Some(model.clone());
                vec![Event::ModelChanged {
                    provider: provider_label(&model),
                    model,
                }]
            }
            SessionRecord::Charged {
                role,
                model,
                usage,
                estimated,
            } => vec![self.usage(role, Some(&model), usage, None, estimated)],
            SessionRecord::ToolCall { id, name, args } => {
                self.calls.insert(id.clone(), (name.clone(), args.clone()));
                vec![Event::ToolCallProposed {
                    call_id: id,
                    tool: name,
                    args,
                }]
            }
            SessionRecord::ApprovalRequested {
                request_id,
                kind: ApprovalKind::Tool,
                call_id: Some(call_id),
                reason,
            } => {
                let (tool, args) = self.calls.get(&call_id).cloned().unwrap_or_default();
                vec![Event::ToolApprovalRequested {
                    request_id: format!("{REPLAYED_ID_PREFIX}{request_id}"),
                    call_id,
                    tool,
                    args,
                    reason,
                    diff: None,
                }]
            }
            SessionRecord::ToolResult {
                id,
                result_blob,
                is_error,
                effects,
                meta,
                blocked,
                ..
            } => {
                let mut out = Vec::new();
                let mut shown = Vec::with_capacity(effects.len());
                for e in &effects {
                    match self.effect(e) {
                        Ok(v) => shown.push(v),
                        Err(why) => out.push(Event::Warning {
                            message: format!("replay: an effect of call {id} was left out: {why}"),
                        }),
                    }
                }
                let output = match self.reader.read_blob(&result_blob) {
                    Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
                    Err(e) => format!("[the output is not in the session's blob store: {e}]"),
                };
                let tool = self
                    .calls
                    .get(&id)
                    .map(|(name, _)| name.clone())
                    .unwrap_or_default();
                out.push(Event::ToolFinished {
                    call_id: id,
                    tool,
                    output,
                    is_error,
                    blocked,
                    effects: shown,
                    meta: ToolMetaView::from(&meta),
                    diff: None,
                });
                out
            }
            SessionRecord::Compaction { removed, summary } => vec![Event::Compacted {
                removed_turns: u32::try_from(removed.len()).unwrap_or(u32::MAX),
                summary,
                // Not logged.
                reason: MutationReason::Unknown,
            }],
            SessionRecord::GcApplied {
                elided,
                reason,
                tokens_freed,
                ..
            } => vec![Event::ContextCollected {
                elided_results: u32::try_from(elided.len()).unwrap_or(u32::MAX),
                tokens_freed,
                reason,
            }],
            SessionRecord::MemoryProposal { id, change } => vec![Event::MemoryProposed {
                proposal_id: format!("{REPLAYED_ID_PREFIX}{id}"),
                change,
                replaces: Vec::new(),
            }],
            SessionRecord::TurnEnd { reason, evidence } => {
                vec![Event::TurnFinished { reason, evidence }]
            }
            SessionRecord::Unknown => {
                self.unknown += 1;
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// A logged effect as shown, its path resolved under the session's root; an error for
    /// a path that could leave it.
    fn effect(&self, e: &PersistedEffect) -> Result<EffectView, String> {
        let path = |p: &RelPath| p.resolve(&self.root).map_err(|e| e.to_string());
        Ok(match e {
            PersistedEffect::Read {
                path: p,
                range,
                shown,
                ..
            } => EffectView::Read {
                path: path(p)?,
                range: *range,
                shown: *shown,
            },
            PersistedEffect::Edited {
                path: p,
                pre_image_blob,
                post_hash,
                ..
            } => EffectView::Edited {
                path: path(p)?,
                created: pre_image_blob.is_none(),
                post_hash: post_hash.to_hex().to_string(),
            },
            PersistedEffect::Deleted { path: p, .. } => EffectView::Deleted { path: path(p)? },
            PersistedEffect::Exec { may_write } => EffectView::Exec {
                may_write: *may_write,
            },
            PersistedEffect::Unknown => EffectView::Unknown,
        })
    }

    /// Read the next record into `out`; false at the end of the log.
    fn advance(&mut self) -> bool {
        let entry = match self.entries.next() {
            Some(Ok(e)) => e,
            Some(Err(e)) => {
                self.truncated = true;
                self.end_response();
                let (offset, t) = self.last;
                self.out.push_back((
                    offset,
                    t,
                    Event::Warning {
                        message: format!(
                            "replay: the session log cannot be read past record {offset}: {e}"
                        ),
                    },
                ));
                return false;
            }
            None => return false,
        };
        let at = (entry.offset, entry.t_ms);
        self.last = at;
        let owner = self.steps.see(&entry.rec);
        let shown = owner.map_or(self.steps.last(), |(s, _)| s) >= self.from_step;
        if let SessionRecord::Provider(ev) = entry.rec {
            if shown {
                self.provider(at, ev);
            }
            return true;
        }
        self.end_response();
        let evs = self.record(entry.rec, shown);
        if shown {
            self.out.extend(evs.into_iter().map(|ev| (at.0, at.1, ev)));
        }
        true
    }
}

/// The log of session `id` of the project at `root`: its full id or a prefix naming one.
fn find(data_home: &Path, root: &Path, id: &str) -> Result<(String, PathBuf), String> {
    let all = tautly_session::list_in(data_home, root).map_err(|e| e.to_string())?;
    if let Some(s) = all.iter().find(|s| s.id == id) {
        return Ok((s.id.clone(), s.path.clone()));
    }
    let mut named = all
        .into_iter()
        .filter(|s| !id.is_empty() && s.id.starts_with(id));
    match (named.next(), named.next()) {
        (Some(s), None) => Ok((s.id, s.path)),
        (Some(_), Some(_)) => Err(format!(
            "`{}` names more than one session; use more of its id",
            printable(id)
        )),
        _ => Err(format!("no session `{}` in this project", printable(id))),
    }
}

/// Replayed events the log reader may be ahead of the client by: what a replay holds in
/// memory is this many events, whatever the log's size.
pub const STREAM_BOUND: usize = 64;

/// What the log reader sends the replay job.
enum Read {
    /// The replay starts: the session's full id.
    Start(String),
    /// It cannot start.
    Refused(String),
    Item(Box<Replayed>),
}

/// Read session `id`'s replayed events (default: `current`'s) into `tx`, one at a time:
/// first `Start` (or `Refused`), then each event. Stops when the receiver is gone (the
/// replay was cancelled). Blocking I/O.
fn read(
    data_home: &Path,
    root: &Path,
    id: Option<&str>,
    current: (String, PathBuf),
    from_step: Option<u32>,
    catalog: Catalog,
    tx: &mpsc::Sender<Read>,
) {
    let opened = (|| {
        let (id, path) = match id {
            None => current,
            Some(id) => find(data_home, root, id)?,
        };
        let reader = SessionReader::<SessionRecord>::open(&path)
            .map_err(|e| format!("cannot read session {id}: {e}"))?;
        if let Some(at) = from_step {
            let last = step_at_offset(&reader, u64::MAX);
            if at > last {
                return Err(TtError::NoSuchStep { at, last }.to_string());
            }
        }
        Ok((id, reader))
    })();
    let reader = match opened {
        Ok((id, reader)) => {
            if tx.blocking_send(Read::Start(id)).is_err() {
                return;
            }
            reader
        }
        Err(why) => {
            let _ = tx.blocking_send(Read::Refused(why));
            return;
        }
    };
    for item in events(&reader, from_step).priced(catalog) {
        if tx.blocking_send(Read::Item(Box::new(item))).is_err() {
            return;
        }
    }
}

impl Session {
    /// `Command::Replay` (spec 7.12.5), as a job: the log read off the async workers, then
    /// each event sent as `Replayed`, paced, until the end or a cancel, then
    /// `ReplayFinished`. A replay that cannot start (a bad speed, no such session or step)
    /// is answered with an `Error` and a `ReplayFinished` of no events.
    pub(crate) async fn replay_job(
        &mut self,
        session_id: Option<String>,
        speed: f32,
        from_step: Option<u32>,
        ctl: &TurnControl,
    ) {
        let sink = self.events.bound(ctl.cancel.clone());
        let mut shown = session_id
            .clone()
            .unwrap_or_else(|| self.session_id.clone());
        let finish = |session_id: String, events: u32, cancelled: bool| Event::ReplayFinished {
            session_id,
            events,
            cancelled,
        };
        if !speed_ok(speed) {
            sink.emit(replay_error(&format!(
                "the replay speed must be a number from 0 to {MAX_SPEED}"
            )))
            .await;
            return sink.emit_last(finish(shown, 0, false)).await;
        }
        // What this session logged so far is on disk before it is read.
        self.checkpoint().await;
        let current = (self.session_id.clone(), self.log.path());
        let (home, root) = (self.data_home.clone(), self.root().to_path_buf());
        let catalog = (*self.setup.catalog).clone();
        // Streamed through a bounded channel: memory stays bounded however large the log
        // (or however often its records name one big blob), and the first event goes out
        // before the rest is read. Dropping `rx` (a cancel) stops the reader.
        let (tx, mut rx) = mpsc::channel(STREAM_BOUND);
        tokio::task::spawn_blocking(move || {
            read(
                &home,
                &root,
                session_id.as_deref(),
                current,
                from_step,
                catalog,
                &tx,
            )
        });
        let first = tokio::select! {
            biased;
            () = ctl.cancel.cancelled() => return sink.emit_last(finish(shown, 0, true)).await,
            r = rx.recv() => r,
        };
        match first {
            Some(Read::Start(id)) => shown = id,
            other => {
                let why = match other {
                    Some(Read::Refused(why)) => why,
                    _ => "the log reader stopped".to_string(),
                };
                sink.emit(replay_error(&format!("cannot replay: {why}")))
                    .await;
                return sink.emit_last(finish(shown, 0, false)).await;
            }
        }
        let (mut sent, mut cancelled, mut prev) = (0u32, false, None);
        loop {
            let next = tokio::select! {
                biased;
                () = ctl.cancel.cancelled() => {
                    cancelled = true;
                    break;
                }
                r = rx.recv() => r,
            };
            let Some(Read::Item(item)) = next else {
                break;
            };
            let (offset, t_ms, event) = *item;
            if let Some(p) = prev {
                let gap = delay(p, t_ms, speed);
                if !gap.is_zero() {
                    tokio::select! {
                        biased;
                        () = ctl.cancel.cancelled() => {}
                        () = tokio::time::sleep(gap) => {}
                    }
                }
            }
            if ctl.cancel.is_cancelled() {
                cancelled = true;
                break;
            }
            prev = Some(t_ms);
            sink.emit(Event::Replayed {
                offset,
                t_ms,
                event: Box::new(event),
            })
            .await;
            sent = sent.saturating_add(1);
        }
        drop(rx);
        sink.emit_last(finish(shown, sent, cancelled)).await;
    }
}

fn replay_error(message: &str) -> Event {
    Event::Error {
        kind: ErrorKind::Internal,
        message: message.to_string(),
        retryable: false,
        hint: None,
    }
}

impl Iterator for Events<'_> {
    type Item = Replayed;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(item) = self.out.pop_front() {
                return Some(item);
            }
            if self.done {
                return None;
            }
            if !self.advance() {
                self.done = true;
                self.end_response();
                if self.unknown > 0 {
                    let (offset, t) = self.last;
                    let n = self.unknown;
                    self.out.push_back((
                        offset,
                        t,
                        Event::Warning {
                            message: format!(
                                "{n} record{} from a newer version skipped",
                                if n == 1 { "" } else { "s" }
                            ),
                        },
                    ));
                }
            }
        }
    }
}
