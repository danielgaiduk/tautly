//! A session's state, and its events turned into `session/update` notifications and
//! client requests.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tautly_core::{
    Command, DiffPreview, EffectView, Engine, Event, FinishReason, McpState, MemoryChange,
    ReasoningLevel, TestEvidence,
};
use tautly_rpc::jsonrpc::Error;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use super::conn::Conn;
use super::replay::finished_diffs;
use super::{INTERNAL_ERROR, lock, log};
use crate::ModelChoices;
use crate::map::{self};
use crate::permission::{self, Answer};
use crate::schema::{
    ContentBlock, StopReason, ToolCall, ToolCallContent, ToolCallStatus, ToolCallUpdate,
};

/// What a prompt waits for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Until {
    Turn,
    /// Ends with `CompactFinished`.
    Compact,
    Diff,
    /// `/undo`'s dry run: its `Undone` is shown to the editor for approval.
    UndoPreview,
    /// The editor is being asked whether to undo.
    UndoAsk,
    Undone,
}

/// A `SetModel` the bridge sent: the model, and who waits for the answer (none when it
/// was sent during a turn and answered at once).
pub(super) struct ModelSwitch {
    pub(super) model: String,
    /// The `SetModel`'s correlation id: its refusal echoes it.
    pub(super) id: String,
    pub(super) reply: Option<oneshot::Sender<Result<(), String>>>,
}

pub(super) struct Waiter {
    pub(super) until: Until,
    /// The correlation id of the command whose answer this waits for.
    pub(super) cmd_id: Option<String>,
    /// `session/cancel` came: the prompt answers `cancelled` whatever the turn says.
    pub(super) cancelled: bool,
    pub(super) reply: oneshot::Sender<Result<StopReason, Error>>,
}

/// What an open approval request is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pending {
    Tool,
    Mcp,
    Memory,
}

pub(super) struct Session {
    /// The ACP session id: the engine's session id when it opened.
    pub(super) id: String,
    pub(super) root: PathBuf,
    pub(super) engine: Engine,
    pub(super) st: Mutex<SessionState>,
    /// The last correlation id given to a command (`acp-<n>`).
    ids: AtomicU64,
}

#[derive(Default)]
pub(super) struct SessionState {
    pub(super) waiter: Option<Waiter>,
    pub(super) switch: Option<ModelSwitch>,
    /// Approval requests the client has not answered, by request (or proposal) id.
    approvals: HashMap<String, Pending>,
    pub(super) models: Option<ModelChoices>,
    /// The `reasoning` option's value: the session's level at open, then what the client
    /// set (spec 4.7).
    pub(super) reasoning: ReasoningLevel,
    /// The last `ContextStatus`: tokens used and the window.
    context: Option<(u32, u32)>,
    /// The session's priced spend so far.
    pub(super) cost_usd: f64,
    /// The last turn's receipt, for its notice and `/cost`.
    receipt: Option<Event>,
    /// The last error of the running turn.
    pub(super) last_error: Option<String>,
    /// Finished-edit updates being built off the forwarder (they may read the log); the
    /// turn's end waits for them, so they come before the prompt's answer.
    building: Vec<JoinHandle<()>>,
}

pub(super) fn chunk(text: &str) -> Value {
    json!({"sessionUpdate": "agent_message_chunk", "content": ContentBlock::text(text)})
}

/// The `model` option (when the session offers a model choice), then `reasoning` with the
/// levels the RPC `set_reasoning` takes.
pub(super) fn config_options(models: Option<&ModelChoices>, reasoning: ReasoningLevel) -> Value {
    let mut options = Vec::new();
    if let Some(m) = models {
        let mut ids = vec![m.current.clone()];
        ids.extend(m.available.iter().filter(|a| **a != m.current).cloned());
        options.push(json!({
            "id": "model",
            "name": "Model",
            "category": "model",
            "type": "select",
            "currentValue": m.current,
            "options": ids.iter().map(|id| json!({"value": id, "name": id})).collect::<Vec<_>>(),
        }));
    }
    let levels = [
        ReasoningLevel::Auto,
        ReasoningLevel::Off,
        ReasoningLevel::Low,
        ReasoningLevel::Medium,
        ReasoningLevel::High,
    ];
    options.push(json!({
        "id": "reasoning",
        "name": "Reasoning",
        "category": "thought_level",
        "type": "select",
        "currentValue": reasoning.to_string(),
        "options": levels
            .iter()
            .map(|l| json!({"value": l.to_string(), "name": l.to_string()}))
            .collect::<Vec<_>>(),
    }));
    Value::Array(options)
}

impl SessionState {
    pub(super) fn options(&self) -> Value {
        config_options(self.models.as_ref(), self.reasoning)
    }

    pub(super) fn cost_text(&self) -> String {
        let session = format!("session: ${:.4}", self.cost_usd);
        match &self.receipt {
            Some(Event::TurnReceipt {
                turn,
                tokens_in,
                tokens_out,
                cost_usd,
                steps,
                ..
            }) => {
                let cost = cost_usd.map_or_else(|| "unpriced".to_string(), |c| format!("${c:.4}"));
                format!(
                    "last turn: turn {turn} · {steps} steps · {tokens_in} tok in / {tokens_out} out · {cost}\n{session}"
                )
            }
            _ => format!("no turn yet\n{session}"),
        }
    }

    /// The waiter for `until`, when `ev` answers it: the event echoes its command's
    /// correlation id (a waiter without one takes any).
    fn take_waiter(&mut self, until: Until, ev: Option<&str>) -> Option<Waiter> {
        match &self.waiter {
            Some(w) if w.until == until && (w.cmd_id.is_none() || w.cmd_id.as_deref() == ev) => {
                self.waiter.take()
            }
            _ => None,
        }
    }

    fn usage_update(&self) -> Option<Value> {
        let (used, size) = self.context?;
        Some(json!({
            "sessionUpdate": "usage_update",
            "used": used,
            "size": size,
            "cost": {"amount": self.cost_usd, "currency": "USD"},
        }))
    }
}

impl Session {
    pub(super) fn new(engine: Engine, root: PathBuf, models: Option<ModelChoices>) -> Session {
        let reasoning = engine.reasoning();
        Session {
            id: engine.session_id().to_string(),
            root,
            engine,
            st: Mutex::new(SessionState {
                models,
                reasoning,
                ..SessionState::default()
            }),
            ids: AtomicU64::new(0),
        }
    }

    /// A fresh correlation id for a command whose answer the bridge waits for.
    pub(super) fn next_id(&self) -> String {
        format!("acp-{}", self.ids.fetch_add(1, Ordering::Relaxed) + 1)
    }

    pub(super) fn opened_result(&self) -> Value {
        let st = lock(&self.st);
        json!({"sessionId": self.id, "configOptions": st.options()})
    }

    pub(super) async fn send(&self, cmd: Command) -> Result<(), Error> {
        self.engine
            .send(cmd)
            .await
            .map_err(|e| Error::new(INTERNAL_ERROR, e.to_string()))
    }

    /// `session/cancel`: every open tool-call approval and memory proposal is answered as
    /// denied (and any that still comes before the turn ends is denied without asking),
    /// the turn is cancelled, and the prompt answers `cancelled` (a command's prompt at
    /// once).
    pub(super) async fn cancel(&self) {
        let open: Vec<(String, Pending)> = {
            let mut st = lock(&self.st);
            if let Some(w) = &mut st.waiter {
                w.cancelled = true;
            }
            if let Some(w) = st.waiter.take_if(|w| w.until != Until::Turn) {
                let _ = w.reply.send(Ok(StopReason::Cancelled));
            }
            // A repository MCP server's approval is not the turn's: a denial would last
            // for the session (spec 7.11.10), so it stays open.
            let open: Vec<(String, Pending)> = st
                .approvals
                .iter()
                .filter(|(_, kind)| **kind != Pending::Mcp)
                .map(|(id, kind)| (id.clone(), *kind))
                .collect();
            for (id, _) in &open {
                st.approvals.remove(id);
            }
            open
        };
        for (id, kind) in open {
            let _ = self.engine.send(deny(&id, kind)).await;
        }
        let _ = self.engine.send(Command::Cancel).await;
    }

    /// Whether approval `id` is still open (and now answered by the caller).
    fn take_approval(&self, id: &str) -> bool {
        lock(&self.st).approvals.remove(id).is_some()
    }

    /// Record approval `id` as asked; false (nothing recorded) when the running turn was
    /// cancelled, so the caller denies it without asking.
    fn open_approval(&self, id: &str, kind: Pending) -> bool {
        let mut st = lock(&self.st);
        if st.waiter.as_ref().is_some_and(|w| w.cancelled) {
            return false;
        }
        st.approvals.insert(id.to_string(), kind);
        true
    }

    /// Whether the running prompt waits for `until`.
    fn waiting_for(&self, until: Until) -> bool {
        lock(&self.st)
            .waiter
            .as_ref()
            .is_some_and(|w| w.until == until)
    }
}

fn deny(id: &str, kind: Pending) -> Command {
    match kind {
        Pending::Memory => permission::memory_command(id, &Answer::Reject),
        Pending::Tool | Pending::Mcp => permission::approval_command(id, Answer::Reject),
    }
}

/// Turn a session's events into updates until its stream ends; then the session is gone.
pub(super) async fn forward(conn: Arc<Conn>, s: Arc<Session>, mut events: mpsc::Receiver<Event>) {
    while let Some(ev) = events.recv().await {
        on_event(&conn, &s, ev).await;
    }
    lock(&conn.state).sessions.remove(&s.id);
    let mut st = lock(&s.st);
    if let Some(w) = st.waiter.take() {
        let _ = w
            .reply
            .send(Err(Error::new(INTERNAL_ERROR, "the session has stopped")));
    }
    st.switch.take();
}

/// Something the user should know: a notice when the client shows them, else (when
/// `message`) an agent message, else stderr.
async fn tell(
    conn: &Conn,
    s: &Session,
    severity: &str,
    title: &str,
    detail: Option<String>,
    message: bool,
) {
    if conn.notices() {
        let mut n = json!({"sessionUpdate": "notice", "severity": severity, "title": title});
        if let Some(d) = detail {
            n["description"] = json!(d);
        }
        conn.update(&s.id, n).await;
    } else if message {
        conn.update(&s.id, chunk(&format!("\n[{title}]\n"))).await;
    } else {
        log(title);
    }
}

async fn on_event(conn: &Arc<Conn>, s: &Arc<Session>, ev: Event) {
    match ev {
        Event::TextDelta { text } => {
            conn.update(&s.id, chunk(&text)).await;
        }
        Event::ReasoningDelta { text } => {
            conn.update(
                &s.id,
                json!({"sessionUpdate": "agent_thought_chunk", "content": ContentBlock::text(text)}),
            )
            .await;
        }
        Event::ToolCallProposed {
            call_id,
            tool,
            args,
        } => {
            let call = ToolCall {
                tool_call_id: call_id.clone(),
                title: map::tool_title(&tool, &args),
                kind: map::tool_kind(&tool),
                status: ToolCallStatus::Pending,
                content: Vec::new(),
                locations: map::locations(&s.root, &args),
                raw_input: Some(args.clone()),
            };
            let mut update = serde_json::to_value(call).unwrap_or_default();
            update["sessionUpdate"] = json!("tool_call");
            conn.update(&s.id, update).await;
        }
        Event::ToolApprovalRequested {
            request_id,
            call_id,
            tool,
            args,
            diff,
            ..
        } => {
            if !s.open_approval(&request_id, Pending::Tool) {
                let _ = s.engine.send(deny(&request_id, Pending::Tool)).await;
                return;
            }
            let (conn, s) = (Arc::clone(conn), Arc::clone(s));
            tokio::spawn(async move {
                ask_tool(&conn, &s, request_id, call_id, tool, args, diff).await;
            });
        }
        Event::ToolStarted { call_id, .. } => {
            let u = ToolCallUpdate {
                tool_call_id: call_id,
                status: Some(ToolCallStatus::InProgress),
                ..ToolCallUpdate::default()
            };
            conn.update(&s.id, tool_update(u)).await;
        }
        Event::ToolFinished {
            call_id,
            output,
            is_error,
            blocked,
            effects,
            diff,
            ..
        } => {
            let mut content = vec![map::output_content(&output)];
            let edited = effects
                .iter()
                .any(|e| matches!(e, EffectView::Edited { .. } | EffectView::Deleted { .. }));
            let status = if is_error || blocked {
                ToolCallStatus::Failed
            } else {
                ToolCallStatus::Completed
            };
            let (conn2, s2) = (Arc::clone(conn), Arc::clone(s));
            let send = async move {
                if edited && !is_error {
                    content.extend(finished_diffs(&s2, &call_id, &effects, diff).await);
                }
                let u = ToolCallUpdate {
                    tool_call_id: call_id,
                    status: Some(status),
                    content: Some(content),
                    ..ToolCallUpdate::default()
                };
                conn2.update(&s2.id, tool_update(u)).await;
            };
            if edited && !is_error {
                // Its diff may need a log read: built off the forwarder, so the turn's
                // events keep flowing (this call's last update, so its order holds).
                let done = tokio::spawn(send);
                let mut st = lock(&s.st);
                st.building.retain(|h| !h.is_finished());
                st.building.push(done);
            } else {
                send.await;
            }
        }
        Event::Usage { cost_usd, .. } => {
            let update = {
                let mut st = lock(&s.st);
                st.cost_usd += cost_usd.unwrap_or(0.0);
                st.usage_update()
            };
            if let Some(u) = update {
                conn.update(&s.id, u).await;
            }
        }
        Event::ContextStatus { used, budget, .. } => {
            let update = {
                let mut st = lock(&s.st);
                st.context = Some((used, budget));
                st.usage_update()
            };
            if let Some(u) = update {
                conn.update(&s.id, u).await;
            }
        }
        Event::Compacted { removed_turns, .. } => {
            let text = format!("compacted {removed_turns} turns of the conversation");
            if s.waiting_for(Until::Compact) {
                conn.update(&s.id, chunk(&format!("{text}\n"))).await;
            } else {
                tell(conn, s, "info", &text, None, false).await;
            }
        }
        Event::CompactFinished {
            id,
            applied,
            tokens_freed,
        } => {
            let waiter = lock(&s.st).take_waiter(Until::Compact, id.as_deref());
            if let Some(w) = waiter {
                if applied {
                    let text = format!("context compacted: {tokens_freed} tokens freed");
                    conn.update(&s.id, chunk(&text)).await;
                }
                let _ = w.reply.send(Ok(StopReason::EndTurn));
            }
        }
        Event::MemoryProposed {
            proposal_id,
            change,
            ..
        } => {
            if !s.open_approval(&proposal_id, Pending::Memory) {
                let _ = s.engine.send(deny(&proposal_id, Pending::Memory)).await;
                return;
            }
            let (conn, s) = (Arc::clone(conn), Arc::clone(s));
            tokio::spawn(async move { ask_memory(&conn, &s, proposal_id, change).await });
        }
        Event::McpServerApprovalRequested {
            request_id,
            server,
            source,
            launch,
        } => {
            lock(&s.st)
                .approvals
                .insert(request_id.clone(), Pending::Mcp);
            let (conn, s) = (Arc::clone(conn), Arc::clone(s));
            tokio::spawn(async move {
                let params = permission::mcp_request(&s.id, &request_id, &server, &source, &launch);
                let result = conn.ask("session/request_permission", params).await;
                // `allow_always` is not offered: a session rule is for tool calls only.
                let answer = permission::answer(result.as_ref(), None);
                if s.take_approval(&request_id) {
                    let _ = s
                        .engine
                        .send(permission::approval_command(&request_id, answer))
                        .await;
                }
            });
        }
        Event::McpServerStatus {
            server,
            state: McpState::Failed,
            message,
            ..
        } => {
            let title = format!("MCP server `{server}` failed");
            tell(conn, s, "warning", &title, message, false).await;
        }
        Event::McpServerStatus {
            server,
            state: McpState::Ready,
            tools,
            ..
        } if conn.notices() => {
            let title = format!("MCP server `{server}` ready ({tools} tools)");
            tell(conn, s, "info", &title, None, false).await;
        }
        ev @ Event::TurnReceipt { .. } => {
            lock(&s.st).receipt = Some(ev);
        }
        Event::TurnFinished {
            id,
            reason,
            evidence,
        } => turn_finished(conn, s, id, reason, evidence).await,
        Event::Error {
            id, message, hint, ..
        } => {
            // A failed model switch is the switch's answer, never the turn's or a
            // command's: it echoes the `SetModel`'s correlation id.
            let switch = {
                let mut st = lock(&s.st);
                st.switch
                    .take_if(|sw| id.as_deref() == Some(sw.id.as_str()))
            };
            let text = match hint {
                Some(h) => format!("{message}\nhint: {h}"),
                None => message,
            };
            if let Some(sw) = switch {
                match sw.reply {
                    Some(reply) => {
                        let _ = reply.send(Err(text));
                    }
                    None => {
                        // Answered at once during a turn: put the real model back.
                        let options = {
                            let st = lock(&s.st);
                            st.models.is_some().then(|| st.options())
                        };
                        if let Some(o) = options {
                            conn.update(
                                &s.id,
                                json!({"sessionUpdate": "config_option_update", "configOptions": o}),
                            )
                            .await;
                        }
                        tell(conn, s, "error", &text, None, true).await;
                    }
                }
                return;
            }
            let (command, compacting) = {
                let mut st = lock(&s.st);
                st.last_error = Some(text.clone());
                // A `/diff` or undo error echoes its command's id.
                let command = st.waiter.take_if(|w| match w.until {
                    Until::Diff => w.cmd_id == id,
                    Until::UndoPreview | Until::Undone => w.cmd_id == id,
                    _ => false,
                });
                (
                    command,
                    st.waiter
                        .as_ref()
                        .is_some_and(|w| w.until == Until::Compact),
                )
            };
            if let Some(w) = command {
                conn.update(&s.id, chunk(&format!("error: {text}"))).await;
                let _ = w.reply.send(Ok(StopReason::EndTurn));
            } else if compacting {
                // Shown; the compaction still ends with `CompactFinished`.
                conn.update(&s.id, chunk(&format!("error: {text}\n"))).await;
            } else {
                tell(conn, s, "error", &text, None, false).await;
            }
        }
        Event::SessionDiff {
            id, files, unified, ..
        } => {
            let waiter = lock(&s.st).take_waiter(Until::Diff, id.as_deref());
            if let Some(w) = waiter {
                let text = if files.is_empty() {
                    "no changes by Tautly's tools in this scope".to_string()
                } else {
                    map::fenced(&unified)
                };
                conn.update(&s.id, chunk(&text)).await;
                let _ = w.reply.send(Ok(StopReason::EndTurn));
            }
        }
        Event::Undone {
            id,
            turn,
            restored,
            deleted,
            note,
            dry_run,
            ..
        } => {
            let list = undo_list(&restored, &deleted, note.as_deref());
            if dry_run {
                let nothing = restored.is_empty() && deleted.is_empty();
                let asking = {
                    let mut st = lock(&s.st);
                    match &mut st.waiter {
                        Some(w) if w.until == Until::UndoPreview && w.cmd_id == id && !nothing => {
                            w.until = Until::UndoAsk;
                            true
                        }
                        _ => false,
                    }
                };
                if asking {
                    let (conn, s) = (Arc::clone(conn), Arc::clone(s));
                    tokio::spawn(async move { ask_undo(&conn, &s, turn, list).await });
                    return;
                }
                let waiter = lock(&s.st).take_waiter(Until::UndoPreview, id.as_deref());
                if let Some(w) = waiter {
                    let text = format!("nothing to undo in turn {turn}{list}");
                    conn.update(&s.id, chunk(&text)).await;
                    let _ = w.reply.send(Ok(StopReason::EndTurn));
                }
                return;
            }
            let waiter = lock(&s.st).take_waiter(Until::Undone, id.as_deref());
            if let Some(w) = waiter {
                let text = format!(
                    "undid turn {turn}: {} restored, {} deleted{list}",
                    restored.len(),
                    deleted.len()
                );
                conn.update(&s.id, chunk(&text)).await;
                let _ = w.reply.send(Ok(StopReason::EndTurn));
            }
        }
        Event::ModelChanged { model, .. } => {
            let (options, waiter) = {
                let mut st = lock(&s.st);
                let options = match st.models.as_mut() {
                    Some(m) => {
                        m.current = model.as_str().to_string();
                        Some(st.options())
                    }
                    None => None,
                };
                let switch = st.switch.take_if(|sw| sw.model == model.as_str());
                (options, switch.and_then(|sw| sw.reply))
            };
            if let Some(o) = options {
                conn.update(
                    &s.id,
                    json!({"sessionUpdate": "config_option_update", "configOptions": o}),
                )
                .await;
            }
            if let Some(w) = waiter {
                let _ = w.send(Ok(()));
            }
        }
        // The engine no longer waits for it (a cancelled or stopped turn, a shutdown): an
        // answer the editor still gives is not sent.
        Event::ApprovalWithdrawn { request_id } => {
            lock(&s.st).approvals.remove(&request_id);
        }
        Event::Warning { message } => {
            // During `/compact`, the user asked: shown in the answer (which still ends
            // with `CompactFinished`).
            if s.waiting_for(Until::Compact) {
                conn.update(&s.id, chunk(&format!("{message}\n"))).await;
            } else {
                tell(conn, s, "warning", &message, None, false).await;
            }
        }
        _ => {}
    }
}

fn tool_update(u: ToolCallUpdate) -> Value {
    let mut v = serde_json::to_value(u).unwrap_or_default();
    v["sessionUpdate"] = json!("tool_call_update");
    v
}

async fn turn_finished(
    conn: &Conn,
    s: &Session,
    id: Option<String>,
    reason: FinishReason,
    evidence: Option<TestEvidence>,
) {
    // The turn's finished-edit updates go out before its end.
    let building = std::mem::take(&mut lock(&s.st).building);
    for b in building {
        let _ = b.await;
    }
    let (waiter, receipt, error) = {
        let mut st = lock(&s.st);
        (
            st.take_waiter(Until::Turn, id.as_deref()),
            st.receipt.clone(),
            st.last_error.take(),
        )
    };
    if let Some(Event::TurnReceipt {
        turn,
        steps,
        wall_ms,
        cost_usd,
        saved,
        ..
    }) = receipt
        && conn.notices()
    {
        let cost = cost_usd.map_or_else(|| "unpriced".to_string(), |c| format!("${c:.4}"));
        let saved = saved.index + saved.diff_reads + saved.context_gc + saved.filters;
        let title = format!(
            "turn {turn} · {steps} steps · {:.1}s · {cost} · saved ~{saved} tok (est.)",
            wall_ms as f64 / 1000.0
        );
        tell(
            conn,
            s,
            "info",
            &title,
            evidence.as_ref().map(evidence_line),
            false,
        )
        .await;
    }
    match reason {
        FinishReason::Budget => {
            tell(
                conn,
                s,
                "warning",
                "turn stopped: budget reached",
                None,
                true,
            )
            .await
        }
        FinishReason::Timeout => {
            tell(
                conn,
                s,
                "warning",
                "turn stopped: time limit reached",
                None,
                true,
            )
            .await;
        }
        _ => {}
    }
    let Some(w) = waiter else {
        return;
    };
    let answer = if w.cancelled {
        Ok(StopReason::Cancelled)
    } else {
        map::stop_reason(reason).ok_or_else(|| {
            Error::new(
                INTERNAL_ERROR,
                error.unwrap_or_else(|| "the turn failed".to_string()),
            )
        })
    };
    let _ = w.reply.send(answer);
}

/// `last test: <command> · exit <code> · fresh|stale (N edits since)` (spec 7.11.7).
fn evidence_line(e: &TestEvidence) -> String {
    let exit = e
        .exit_code
        .map_or_else(|| "killed".to_string(), |c| format!("exit {c}"));
    let fresh = if e.stale {
        format!(
            "stale ({} edits, {} commands since)",
            e.edits_since, e.execs_since
        )
    } else {
        "fresh".to_string()
    };
    format!("last test: {} · {exit} · {fresh}", e.command)
}

async fn ask_tool(
    conn: &Conn,
    s: &Session,
    request_id: String,
    call_id: String,
    tool: String,
    args: Value,
    diff: Option<DiffPreview>,
) {
    let root = s.root.clone();
    let (t, a) = (tool.clone(), args.clone());
    let unified = diff.map(|d| (d.truncated, d.unified));
    let (rule, content) = tokio::task::spawn_blocking(move || {
        let rule = tautly_core::exact_rule(&root, &t, &a);
        let content = match unified {
            Some((truncated, text)) => (!truncated)
                .then(|| map::preview_diffs(&root, &text))
                .flatten()
                .unwrap_or_else(|| vec![ToolCallContent::text(map::approval_text(&text))]),
            // No diff: the whole call, every line of a command, escaped and fenced.
            None => vec![ToolCallContent::text(map::call_text(&t, &a))],
        };
        (rule, content)
    })
    .await
    .unwrap_or((None, Vec::new()));
    let call = ToolCallUpdate {
        tool_call_id: call_id,
        title: Some(map::tool_title(&tool, &args)),
        kind: Some(map::tool_kind(&tool)),
        content: Some(content),
        locations: Some(map::locations(&s.root, &args)),
        raw_input: Some(args),
        ..ToolCallUpdate::default()
    };
    let params = permission::tool_request(&s.id, call, rule.as_deref());
    let result = conn.ask("session/request_permission", params).await;
    let answer = permission::answer(result.as_ref(), rule.as_deref());
    if s.take_approval(&request_id) {
        let _ = s
            .engine
            .send(permission::approval_command(&request_id, answer))
            .await;
    }
}

async fn ask_memory(conn: &Conn, s: &Session, proposal_id: String, change: MemoryChange) {
    let params = permission::memory_request(&s.id, &proposal_id, &change);
    let result = conn.ask("session/request_permission", params).await;
    let answer = permission::answer(result.as_ref(), None);
    if s.take_approval(&proposal_id) {
        let _ = s
            .engine
            .send(permission::memory_command(&proposal_id, &answer))
            .await;
    }
}

/// The files an undo restores and deletes, and its note, as lines to show (paths escaped).
fn undo_list(restored: &[String], deleted: &[String], note: Option<&str>) -> String {
    let mut text = String::new();
    for p in restored {
        text.push_str(&format!("\n- restore {}", map::single_line(p)));
    }
    for p in deleted {
        text.push_str(&format!("\n- delete {}", map::single_line(p)));
    }
    if let Some(n) = note {
        text.push_str(&format!("\n{}", map::single_line(n)));
    }
    text
}

/// `/undo` after its dry run: ask the editor; on `allow_once` undo for real (the prompt
/// then waits for that `Undone`), else end the prompt. A cancel meanwhile has already
/// answered the prompt.
async fn ask_undo(conn: &Conn, s: &Session, turn: u32, list: String) {
    let call = ToolCallUpdate {
        tool_call_id: format!("undo:{turn}"),
        title: Some(format!("Undo turn {turn}")),
        kind: Some(crate::schema::ToolKind::Edit),
        content: Some(vec![ToolCallContent::text(map::approval_text(
            list.trim_start(),
        ))]),
        ..ToolCallUpdate::default()
    };
    let params = json!({
        "sessionId": s.id,
        "toolCall": call,
        "options": permission::yes_no_options(),
    });
    let result = conn.ask("session/request_permission", params).await;
    let approved = permission::answer(result.as_ref(), None) == Answer::Once;
    let id = s.next_id();
    let waiter = {
        let mut st = lock(&s.st);
        match st.waiter.take_if(|w| w.until == Until::UndoAsk) {
            Some(mut w) if approved => {
                w.until = Until::Undone;
                w.cmd_id = Some(id.clone());
                st.waiter = Some(w);
                None
            }
            other => other,
        }
    };
    match waiter {
        Some(w) => {
            conn.update(&s.id, chunk("undo not done")).await;
            let _ = w.reply.send(Ok(StopReason::EndTurn));
        }
        None if approved && s.waiting_for(Until::Undone) => {
            let undo = Command::Undo {
                id: Some(id.clone()),
                turn: Some(turn),
                dry_run: false,
            };
            if s.send(undo).await.is_err()
                && let Some(w) = lock(&s.st).take_waiter(Until::Undone, Some(&id))
            {
                let _ = w
                    .reply
                    .send(Err(Error::new(INTERNAL_ERROR, "the session has stopped")));
            }
        }
        None => {}
    }
}
