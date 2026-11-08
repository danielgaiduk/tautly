//! The connection: what the reader and the tasks share, and the ACP request handlers.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tautly_core::{Command, Engine, Event, ReasoningLevel};
use tautly_rpc::jsonrpc::{self, Error, INVALID_PARAMS, INVALID_REQUEST, METHOD_NOT_FOUND};
use tokio::sync::{mpsc, oneshot};
use tokio::task::{JoinHandle, JoinSet};

use super::events::{ModelSwitch, Session, Until, Waiter, chunk, config_options, forward};
use super::replay::{ledger_spend, replay};
use super::{INTERNAL_ERROR, NOT_FOUND, REPLAY_BOUND, Reply, lock, log};
use crate::map::{self, Slash};
use crate::schema::{
    CancelParams, InitializeParams, LoadSessionParams, NewSessionParams, PromptParams,
    SetConfigParams, StopReason,
};
use crate::{ACP_PROTOCOL_VERSION, AcpEngineFactory, ModelChoices, ModelLister};

/// What the reader and the tasks share.
pub(super) struct Conn {
    pub(super) out: mpsc::Sender<String>,
    pub(super) factory: AcpEngineFactory,
    pub(super) models: Option<ModelLister>,
    pub(super) data_home: Option<PathBuf>,
    pub(super) next_id: AtomicU64,
    pub(super) state: Mutex<ConnState>,
}

#[derive(Default)]
pub(super) struct ConnState {
    initialized: bool,
    /// The client shows `notice` updates.
    notices: bool,
    pub(super) sessions: HashMap<String, Arc<Session>>,
    /// Our requests to the client, by id, waiting for its answer.
    pending: HashMap<u64, oneshot::Sender<Value>>,
    forwards: Vec<JoinHandle<()>>,
    closed: bool,
}

fn invalid(msg: impl Into<String>) -> Error {
    Error::new(INVALID_PARAMS, msg)
}

fn params<T: serde::de::DeserializeOwned>(method: &str, params: Value) -> Result<T, Error> {
    serde_json::from_value(params)
        .map_err(|e| invalid(format!("invalid params for `{method}`: {e}")))
}

impl Conn {
    pub(super) async fn send(&self, line: String) {
        let _ = self.out.send(line).await;
    }

    async fn reply(&self, id: &Value, result: Result<Value, Error>) {
        let line = match result {
            Ok(v) => jsonrpc::response(id, v),
            Err(e) => jsonrpc::error_response(id, &e),
        };
        self.send(line).await;
    }

    pub(super) async fn update(&self, session_id: &str, update: Value) {
        self.send(jsonrpc::notification(
            "session/update",
            json!({"sessionId": session_id, "update": update}),
        ))
        .await;
    }

    pub(super) fn notices(&self) -> bool {
        lock(&self.state).notices
    }

    /// A response to one of our requests is handed to whoever waits for it (None);
    /// anything else is given back.
    pub(super) fn route_response(&self, value: Value) -> Option<Value> {
        let Value::Object(msg) = &value else {
            return Some(value);
        };
        if msg.contains_key("method") || !(msg.contains_key("result") || msg.contains_key("error"))
        {
            return Some(value);
        }
        let waiter = msg
            .get("id")
            .and_then(Value::as_u64)
            .and_then(|id| lock(&self.state).pending.remove(&id));
        if let Some(waiter) = waiter {
            // An error answer is no answer: whoever asked treats it as a refusal.
            let _ = waiter.send(msg.get("result").cloned().unwrap_or(Value::Null));
        }
        None
    }

    /// Ask the client; its result, or None when it answered with an error or went away.
    pub(super) async fn ask(&self, method: &str, params: Value) -> Option<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        {
            let mut st = lock(&self.state);
            if st.closed {
                return None;
            }
            st.pending.insert(id, tx);
        }
        self.send(
            json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string(),
        )
        .await;
        rx.await.ok().filter(|v| !v.is_null())
    }

    fn session(&self, id: &str) -> Result<Arc<Session>, Error> {
        lock(&self.state)
            .sessions
            .get(id)
            .cloned()
            .ok_or_else(|| Error::new(NOT_FOUND, format!("no open session has the id `{id}`")))
    }

    pub(super) async fn notification(self: Arc<Self>, method: &str, p: Value) {
        if method != "session/cancel" || !lock(&self.state).initialized {
            return;
        }
        let Ok(p) = params::<CancelParams>(method, p) else {
            return;
        };
        if let Ok(session) = self.session(&p.session_id) {
            session.cancel().await;
        }
    }

    pub(super) async fn request(
        self: Arc<Self>,
        id: Value,
        method: &str,
        p: Value,
        tasks: &mut JoinSet<()>,
    ) {
        if method == "initialize" {
            let result = self.initialize(p);
            return self.reply(&id, result).await;
        }
        if !lock(&self.state).initialized {
            let err = Error::new(INVALID_REQUEST, "`initialize` must come first");
            return self.reply(&id, Err(err)).await;
        }
        let pending: Result<Reply, Error> = match method {
            "session/new" | "session/load" => {
                let opening = if method == "session/new" {
                    params::<NewSessionParams>(method, p).map(|p| (p.cwd, None, p.mcp_servers))
                } else {
                    params::<LoadSessionParams>(method, p)
                        .map(|p| (p.cwd, Some(p.session_id), p.mcp_servers))
                };
                match opening {
                    Ok((cwd, resume, servers)) => {
                        tasks.spawn(self.open_session(id, cwd, resume, servers));
                        return;
                    }
                    Err(e) => Err(e),
                }
            }
            "session/prompt" => match params(method, p) {
                Ok(p) => Arc::clone(&self).prompt(p).await,
                Err(e) => Err(e),
            },
            "session/set_config_option" => match params(method, p) {
                Ok(p) => Arc::clone(&self).set_config(p).await,
                Err(e) => Err(e),
            },
            "authenticate" => Err(invalid(
                "no authentication methods: keys come from the environment or the keychain",
            )),
            _ => Err(Error::new(
                METHOD_NOT_FOUND,
                format!("unknown method `{method}`"),
            )),
        };
        match pending {
            Err(e) => self.reply(&id, Err(e)).await,
            Ok(fut) => {
                tasks.spawn(async move {
                    let result = fut.await;
                    self.reply(&id, result).await;
                });
            }
        }
    }

    fn initialize(&self, p: Value) -> Result<Value, Error> {
        let p: InitializeParams = params("initialize", p)?;
        {
            let mut st = lock(&self.state);
            st.initialized = true;
            st.notices = p.notices();
        }
        // Whatever the client asks for, this is the version spoken; a client that cannot
        // speak it disconnects.
        let _ = p.protocol_version;
        Ok(json!({
            "protocolVersion": ACP_PROTOCOL_VERSION,
            "agentCapabilities": {
                "loadSession": true,
                // The engine takes text messages: images are not passed on (yet).
                "promptCapabilities": {"image": false, "audio": false, "embeddedContext": true},
                "mcpCapabilities": {"http": true, "sse": false},
            },
            "authMethods": [],
            "agentInfo": {"name": "tautly", "title": "Tautly", "version": env!("CARGO_PKG_VERSION")},
        }))
    }

    /// Build a session's engine off the async workers; the root and its model choices.
    async fn open(
        self: &Arc<Self>,
        cwd: PathBuf,
        resume: Option<String>,
        servers: Vec<crate::schema::McpServer>,
    ) -> Result<(Engine, mpsc::Receiver<Event>, PathBuf, Option<ModelChoices>), Error> {
        if !cwd.is_absolute() {
            return Err(invalid("`cwd` must be an absolute path"));
        }
        let (mcp, skipped) = map::client_mcp(servers);
        skipped.iter().for_each(|s| log(s));
        let factory = Arc::clone(&self.factory);
        let models = self.models.clone();
        let built = tokio::task::spawn_blocking(move || {
            let root = tautly_core::session_root(&cwd).map_err(|e| e.to_string())?;
            let choices = models.and_then(|m| m(&root));
            let (engine, events) = factory(root.clone(), resume, mcp).map_err(|e| e.to_string())?;
            Ok::<_, String>((engine, events, root, choices))
        })
        .await
        .map_err(|e| {
            Error::new(
                INTERNAL_ERROR,
                format!("the session could not be opened: {e}"),
            )
        })?;
        built.map_err(|e| Error::new(INTERNAL_ERROR, e))
    }

    /// `session/new` (no `resume`) or `session/load`, answering request `id`. A loaded
    /// session's history is replayed before the response. The session is open (prompts
    /// find it) from just before the response; its events, starting with the advertised
    /// commands, follow the response.
    async fn open_session(
        self: Arc<Self>,
        id: Value,
        cwd: PathBuf,
        resume: Option<String>,
        servers: Vec<crate::schema::McpServer>,
    ) {
        if let Some(r) = &resume
            && self.session(r).is_ok()
        {
            let err = invalid(format!("session `{r}` is already open"));
            return self.reply(&id, Err(err)).await;
        }
        let replaying = resume.is_some();
        let (engine, events, root, models) = match self.open(cwd, resume, servers).await {
            Ok(built) => built,
            Err(e) => return self.reply(&id, Err(e)).await,
        };
        let session = Arc::new(Session::new(engine, root, models));
        if replaying {
            let log = session.engine.session_path().to_path_buf();
            let root = session.root.clone();
            let (home, id) = (self.data_home.clone(), session.id.clone());
            let spent =
                tokio::task::spawn_blocking(move || home.and_then(|h| ledger_spend(&h, &id)))
                    .await
                    .ok()
                    .flatten();
            lock(&session.st).cost_usd = spent.unwrap_or(0.0);
            // Streamed through a bounded channel: memory stays bounded however large the
            // history, and the first update goes out before the rest is read.
            let (tx, mut rx) = mpsc::channel(REPLAY_BOUND);
            tokio::task::spawn_blocking(move || {
                // A dropped receiver (the connection went away) stops the reader.
                replay(&log, &root, &mut |u| tx.blocking_send(u).is_ok());
            });
            while let Some(u) = rx.recv().await {
                self.update(&session.id, u).await;
            }
        }
        let registered = {
            let mut st = lock(&self.state);
            if !st.closed {
                st.sessions.insert(session.id.clone(), Arc::clone(&session));
            }
            !st.closed
        };
        if !registered {
            let err = Error::new(INTERNAL_ERROR, "the connection is closing");
            return self.reply(&id, Err(err)).await;
        }
        self.reply(&id, Ok(session.opened_result())).await;
        self.update(
            &session.id,
            json!({"sessionUpdate": "available_commands_update",
                   "availableCommands": map::available_commands()}),
        )
        .await;
        let mut st = lock(&self.state);
        if !st.closed {
            let conn = Arc::clone(&self);
            st.forwards
                .push(tokio::spawn(forward(conn, session, events)));
        }
    }

    /// A prompt: its command is sent now; the returned future waits for the answer.
    async fn prompt(self: Arc<Self>, p: PromptParams) -> Result<Reply, Error> {
        let session = self.session(&p.session_id)?;
        let text = map::prompt_text(&p.prompt).map_err(invalid)?;
        let done = |stop: StopReason| -> Reply {
            Box::pin(async move { Ok(json!({"stopReason": stop})) })
        };
        // A command's answer is matched by its correlation id: a late answer to an
        // earlier (cancelled) prompt never answers this one.
        let cmd_id = session.next_id();
        let (until, command) = match map::slash(&text) {
            None => (
                Until::Turn,
                Command::SendMessage {
                    id: Some(cmd_id.clone()),
                    text,
                },
            ),
            Some(Slash::Compact) => (
                Until::Compact,
                Command::Compact {
                    id: Some(cmd_id.clone()),
                },
            ),
            Some(Slash::Diff(turn)) => (
                Until::Diff,
                Command::Diff {
                    id: Some(cmd_id.clone()),
                    turn,
                },
            ),
            // A dry run first: the editor is asked before anything is undone (spec
            // 7.11.8's `/undo`).
            Some(Slash::Undo(turn)) => (
                Until::UndoPreview,
                Command::Undo {
                    id: Some(cmd_id.clone()),
                    turn,
                    dry_run: true,
                },
            ),
            Some(Slash::Pin(text)) => {
                // The engine applies it (between turns, or at the next step) and warns
                // only when it refuses.
                let shown = format!("pinning: {}", map::single_line(&text));
                session.send(Command::Pin { text }).await?;
                self.update(&session.id, chunk(&shown)).await;
                return Ok(done(StopReason::EndTurn));
            }
            Some(Slash::Cost) => {
                let text = lock(&session.st).cost_text();
                self.update(&session.id, chunk(&text)).await;
                return Ok(done(StopReason::EndTurn));
            }
            Some(Slash::Usage(usage)) => {
                self.update(&session.id, chunk(usage)).await;
                return Ok(done(StopReason::EndTurn));
            }
        };
        let (tx, rx) = oneshot::channel();
        {
            let mut st = lock(&session.st);
            if st.waiter.is_some() {
                return Err(invalid("a prompt is already running in this session"));
            }
            st.waiter = Some(Waiter {
                until,
                cmd_id: command.id().map(str::to_string),
                cancelled: false,
                reply: tx,
            });
            st.last_error = None;
        }
        if let Err(e) = session.send(command).await {
            lock(&session.st).waiter = None;
            return Err(e);
        }
        Ok(Box::pin(async move {
            match rx.await {
                Ok(Ok(stop)) => Ok(json!({"stopReason": stop})),
                Ok(Err(e)) => Err(e),
                Err(_) => Err(Error::new(INTERNAL_ERROR, "the session has stopped")),
            }
        }))
    }

    /// `session/set_config_option {configId: "model"}`: `SetModel` now; the answer once
    /// the engine has switched (at once when idle; while a turn runs the switch waits for
    /// the next turn, and the answer shows the requested model: should the switch fail
    /// then, a `config_option_update` puts the real model back).
    async fn set_config(self: Arc<Self>, p: SetConfigParams) -> Result<Reply, Error> {
        let session = self.session(&p.session_id)?;
        if p.config_id == "reasoning" {
            // `SetReasoning` has no answer: applied from the next request (spec 4.7).
            let level = p
                .value
                .as_str()
                .and_then(|v| v.parse::<ReasoningLevel>().ok())
                .ok_or_else(|| {
                    invalid(format!(
                        "the `reasoning` option takes one of {}",
                        ReasoningLevel::NAMES
                    ))
                })?;
            session.send(Command::SetReasoning { level }).await?;
            let options = {
                let mut st = lock(&session.st);
                st.reasoning = level;
                st.options()
            };
            return Ok(Box::pin(
                async move { Ok(json!({"configOptions": options})) },
            ));
        }
        if p.config_id != "model" {
            return Err(invalid(format!("no config option `{}`", p.config_id)));
        }
        let Some(model) = p.value.as_str().map(str::to_string) else {
            return Err(invalid("the `model` option takes a model id"));
        };
        if lock(&session.st).models.is_none() {
            return Err(invalid("this session offers no model choice"));
        }
        let (tx, rx) = oneshot::channel();
        let id = session.next_id();
        let busy = {
            let mut st = lock(&session.st);
            let busy = st.waiter.is_some();
            st.switch = Some(ModelSwitch {
                model: model.clone(),
                id: id.clone(),
                reply: (!busy).then_some(tx),
            });
            busy
        };
        session
            .send(Command::SetModel {
                id: Some(id),
                model: model.as_str().into(),
            })
            .await?;
        Ok(Box::pin(async move {
            if busy {
                let st = lock(&session.st);
                let mut shown = st.models.clone().unwrap_or(ModelChoices {
                    current: model.clone(),
                    available: Vec::new(),
                });
                shown.current = model;
                return Ok(json!({"configOptions": config_options(Some(&shown), st.reasoning)}));
            }
            match rx.await {
                Ok(Ok(())) => {
                    let st = lock(&session.st);
                    Ok(json!({"configOptions": st.options()}))
                }
                Ok(Err(msg)) => Err(Error::new(INVALID_PARAMS, msg)),
                Err(_) => Err(Error::new(INTERNAL_ERROR, "the session has stopped")),
            }
        }))
    }

    /// Shut every session down (pending approvals are denied by the engine) and wait for
    /// their last events; nothing waits for the client any more.
    pub(super) async fn close(&self) {
        let (sessions, forwards) = {
            let mut st = lock(&self.state);
            st.closed = true;
            st.pending.clear();
            (
                st.sessions.drain().map(|(_, s)| s).collect::<Vec<_>>(),
                std::mem::take(&mut st.forwards),
            )
        };
        for s in &sessions {
            let _ = s.engine.send(Command::Shutdown).await;
        }
        drop(sessions);
        for f in forwards {
            let _ = f.await;
        }
    }
}
