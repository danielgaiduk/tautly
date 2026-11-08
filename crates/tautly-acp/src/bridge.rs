//! The ACP connection (spec 7.11.9): one JSON-RPC 2.0 message per line in each direction.
//!
//! The reader handles each message in order. Whatever must reach an engine in order (a
//! prompt's message, a cancel, a model switch) is sent before the next message is read;
//! only the waiting (for a turn's end, a session's build, a client's answer) runs in a
//! task. Each session has a forwarding task that turns its events into `session/update`
//! notifications and its approval requests into `session/request_permission` requests,
//! each awaited in a task of its own so events keep flowing.
//!
//! Ordering: `session/new` answers before any of its session's updates; `session/load`
//! replays the session's history before it answers. Every line goes through one writer.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use serde_json::{Value, json};
use tautly_core::{
    Command, DiffPreview, EffectView, Engine, Event, FinishReason, McpState, MemoryChange,
    ReasoningLevel, SessionReader, SessionRecord, TestEvidence, printable,
};
use tautly_rpc::jsonrpc::{
    self, Error, INVALID_PARAMS, INVALID_REQUEST, Line, MAX_LINE, METHOD_NOT_FOUND,
};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};
use tokio::task::{JoinHandle, JoinSet};

use crate::map::{self, Slash};
use crate::permission::{self, Answer};
use crate::schema::{
    CancelParams, ContentBlock, InitializeParams, LoadSessionParams, NewSessionParams,
    PromptParams, SetConfigParams, StopReason, ToolCall, ToolCallContent, ToolCallStatus,
    ToolCallUpdate,
};
use crate::{ACP_PROTOCOL_VERSION, AcpEngineFactory, AcpError, ModelChoices, ModelLister};

/// The answer to a request, once what it waits for has happened.
type Reply = Pin<Box<dyn Future<Output = Result<Value, Error>> + Send>>;

/// Internal error (JSON-RPC): a session that could not be opened, a turn that failed.
const INTERNAL_ERROR: i64 = -32603;
/// ACP `resource_not_found`: no open session has that id.
const NOT_FOUND: i64 = -32002;
/// Lines waiting for the output before producers wait.
const OUTPUT_CAPACITY: usize = 1024;
/// Updates of a loaded session read ahead of the client.
const REPLAY_BOUND: usize = 64;

/// An ACP agent serving sessions from a factory.
pub struct Bridge {
    factory: AcpEngineFactory,
    models: Option<ModelLister>,
    data_home: Option<PathBuf>,
    stop: Option<Pin<Box<dyn Future<Output = ()> + Send>>>,
}

impl Bridge {
    pub fn new(factory: AcpEngineFactory) -> Bridge {
        Bridge {
            factory,
            models: None,
            data_home: None,
            stop: None,
        }
    }

    /// The data home whose cost ledger holds a loaded session's earlier spend, so its
    /// `usage_update` cost goes on from there (none: from 0).
    pub fn data_home(mut self, data_home: impl Into<PathBuf>) -> Bridge {
        self.data_home = Some(data_home.into());
        self
    }

    /// What the `model` config option offers (none: no config options).
    pub fn models(mut self, models: ModelLister) -> Bridge {
        self.models = Some(models);
        self
    }

    /// Stop serving when `stop` completes (SIGTERM, SIGHUP), as at the end of input.
    pub fn stop_on(mut self, stop: impl Future<Output = ()> + Send + 'static) -> Bridge {
        self.stop = Some(Box::pin(stop));
        self
    }

    /// Serve until the input ends or the stop future fires; every session is shut down
    /// and its last events written before this returns. An error only when the input
    /// cannot be read (the sessions are shut down all the same).
    pub async fn serve(
        self,
        input: impl AsyncRead + Unpin,
        output: impl AsyncWrite + Unpin,
    ) -> Result<(), AcpError> {
        let (out, lines) = mpsc::channel(OUTPUT_CAPACITY);
        let conn = Arc::new(Conn {
            out,
            factory: self.factory,
            models: self.models,
            data_home: self.data_home,
            next_id: AtomicU64::new(1),
            state: Mutex::default(),
        });
        let stop = self
            .stop
            .unwrap_or_else(|| Box::pin(std::future::pending()));
        let reader = read_loop(conn, input, stop);
        let (result, ()) = tokio::join!(reader, write_lines(lines, output));
        result
    }
}

async fn write_lines(mut lines: mpsc::Receiver<String>, mut output: impl AsyncWrite + Unpin) {
    while let Some(mut line) = lines.recv().await {
        line.push('\n');
        if output.write_all(line.as_bytes()).await.is_err() || output.flush().await.is_err() {
            // The client is gone; keep draining so producers never wait on it.
            while lines.recv().await.is_some() {}
            return;
        }
    }
}

/// Diagnostics: stderr only (stdout carries the protocol), escaped for the terminal.
fn log(msg: &str) {
    eprintln!("tautly: {}", printable(msg));
}

async fn read_loop(
    conn: Arc<Conn>,
    input: impl AsyncRead + Unpin,
    mut stop: Pin<Box<dyn Future<Output = ()> + Send>>,
) -> Result<(), AcpError> {
    let mut input = BufReader::new(input);
    let mut reader = jsonrpc::LineReader::default();
    let mut tasks = JoinSet::new();
    let mut result = Ok(());
    loop {
        while tasks.try_join_next().is_some() {}
        let read = tokio::select! {
            read = reader.read(&mut input, MAX_LINE) => read,
            () = &mut stop => break,
        };
        match read {
            Ok(Line::Eof) => break,
            Err(e) => {
                result = Err(AcpError::Input(e));
                break;
            }
            Ok(Line::TooLong) => {
                let err = Error::new(
                    INVALID_REQUEST,
                    format!("a message may be at most {MAX_LINE} bytes"),
                );
                conn.send(jsonrpc::error_response(&Value::Null, &err)).await;
            }
            Ok(Line::Line(line)) => {
                if line.iter().all(u8::is_ascii_whitespace) {
                    continue;
                }
                let value = match jsonrpc::parse_json(&line) {
                    Ok(v) => v,
                    Err((id, err)) => {
                        let id = id.unwrap_or(Value::Null);
                        conn.send(jsonrpc::error_response(&id, &err)).await;
                        continue;
                    }
                };
                let Some(value) = conn.route_response(value) else {
                    continue;
                };
                match jsonrpc::parse_value(value) {
                    Err((Some(id), err)) => conn.send(jsonrpc::error_response(&id, &err)).await,
                    Err((None, _)) => {}
                    Ok(jsonrpc::Incoming::Notification { method, params }) => {
                        Arc::clone(&conn).notification(&method, params).await;
                    }
                    Ok(jsonrpc::Incoming::Request { id, method, params }) => {
                        Arc::clone(&conn)
                            .request(id, &method, params, &mut tasks)
                            .await;
                    }
                }
            }
        }
    }
    conn.close().await;
    tasks.shutdown().await;
    result
}

/// What the reader and the tasks share.
struct Conn {
    out: mpsc::Sender<String>,
    factory: AcpEngineFactory,
    models: Option<ModelLister>,
    data_home: Option<PathBuf>,
    next_id: AtomicU64,
    state: Mutex<ConnState>,
}

#[derive(Default)]
struct ConnState {
    initialized: bool,
    /// The client shows `notice` updates.
    notices: bool,
    sessions: HashMap<String, Arc<Session>>,
    /// Our requests to the client, by id, waiting for its answer.
    pending: HashMap<u64, oneshot::Sender<Value>>,
    forwards: Vec<JoinHandle<()>>,
    closed: bool,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn invalid(msg: impl Into<String>) -> Error {
    Error::new(INVALID_PARAMS, msg)
}

fn params<T: serde::de::DeserializeOwned>(method: &str, params: Value) -> Result<T, Error> {
    serde_json::from_value(params)
        .map_err(|e| invalid(format!("invalid params for `{method}`: {e}")))
}

impl Conn {
    async fn send(&self, line: String) {
        let _ = self.out.send(line).await;
    }

    async fn reply(&self, id: &Value, result: Result<Value, Error>) {
        let line = match result {
            Ok(v) => jsonrpc::response(id, v),
            Err(e) => jsonrpc::error_response(id, &e),
        };
        self.send(line).await;
    }

    async fn update(&self, session_id: &str, update: Value) {
        self.send(jsonrpc::notification(
            "session/update",
            json!({"sessionId": session_id, "update": update}),
        ))
        .await;
    }

    fn notices(&self) -> bool {
        lock(&self.state).notices
    }

    /// A response to one of our requests is handed to whoever waits for it (None);
    /// anything else is given back.
    fn route_response(&self, value: Value) -> Option<Value> {
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
    async fn ask(&self, method: &str, params: Value) -> Option<Value> {
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

    async fn notification(self: Arc<Self>, method: &str, p: Value) {
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

    async fn request(self: Arc<Self>, id: Value, method: &str, p: Value, tasks: &mut JoinSet<()>) {
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
    async fn close(&self) {
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

/// What a prompt waits for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Until {
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
struct ModelSwitch {
    model: String,
    /// The `SetModel`'s correlation id: its refusal echoes it.
    id: String,
    reply: Option<oneshot::Sender<Result<(), String>>>,
}

struct Waiter {
    until: Until,
    /// The correlation id of the command whose answer this waits for.
    cmd_id: Option<String>,
    /// `session/cancel` came: the prompt answers `cancelled` whatever the turn says.
    cancelled: bool,
    reply: oneshot::Sender<Result<StopReason, Error>>,
}

/// What an open approval request is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pending {
    Tool,
    Mcp,
    Memory,
}

struct Session {
    /// The ACP session id: the engine's session id when it opened.
    id: String,
    root: PathBuf,
    engine: Engine,
    st: Mutex<SessionState>,
    /// The last correlation id given to a command (`acp-<n>`).
    ids: AtomicU64,
}

#[derive(Default)]
struct SessionState {
    waiter: Option<Waiter>,
    switch: Option<ModelSwitch>,
    /// Approval requests the client has not answered, by request (or proposal) id.
    approvals: HashMap<String, Pending>,
    models: Option<ModelChoices>,
    /// The `reasoning` option's value: the session's level at open, then what the client
    /// set (spec 4.7).
    reasoning: ReasoningLevel,
    /// The last `ContextStatus`: tokens used and the window.
    context: Option<(u32, u32)>,
    /// The session's priced spend so far.
    cost_usd: f64,
    /// The last turn's receipt, for its notice and `/cost`.
    receipt: Option<Event>,
    /// The last error of the running turn.
    last_error: Option<String>,
    /// Finished-edit updates being built off the forwarder (they may read the log); the
    /// turn's end waits for them, so they come before the prompt's answer.
    building: Vec<JoinHandle<()>>,
}

fn chunk(text: &str) -> Value {
    json!({"sessionUpdate": "agent_message_chunk", "content": ContentBlock::text(text)})
}

/// The `model` option (when the session offers a model choice), then `reasoning` with the
/// levels the RPC `set_reasoning` takes.
fn config_options(models: Option<&ModelChoices>, reasoning: ReasoningLevel) -> Value {
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
    fn options(&self) -> Value {
        config_options(self.models.as_ref(), self.reasoning)
    }

    fn cost_text(&self) -> String {
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
    fn new(engine: Engine, root: PathBuf, models: Option<ModelChoices>) -> Session {
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
    fn next_id(&self) -> String {
        format!("acp-{}", self.ids.fetch_add(1, Ordering::Relaxed) + 1)
    }

    fn opened_result(&self) -> Value {
        let st = lock(&self.st);
        json!({"sessionId": self.id, "configOptions": st.options()})
    }

    async fn send(&self, cmd: Command) -> Result<(), Error> {
        self.engine
            .send(cmd)
            .await
            .map_err(|e| Error::new(INTERNAL_ERROR, e.to_string()))
    }

    /// `session/cancel`: every open tool-call approval and memory proposal is answered as
    /// denied (and any that still comes before the turn ends is denied without asking),
    /// the turn is cancelled, and the prompt answers `cancelled` (a command's prompt at
    /// once).
    async fn cancel(&self) {
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
async fn forward(conn: Arc<Conn>, s: Arc<Session>, mut events: mpsc::Receiver<Event>) {
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

/// The `diff` contents of a finished edit: from its applied diff and the files on disk
/// when they are still as the call wrote them (the common case, no log read); else from
/// the texts the session log holds for the call (a truncated diff, a file changed since);
/// else the applied unified diff as text (a file too large or not text).
async fn finished_diffs(
    s: &Session,
    call_id: &str,
    effects: &[EffectView],
    diff: Option<DiffPreview>,
) -> Vec<ToolCallContent> {
    let log = s.engine.current_session().1;
    let (root, id) = (s.root.clone(), call_id.to_string());
    let written: Vec<map::Written> = effects
        .iter()
        .filter_map(|e| match e {
            EffectView::Edited {
                path, post_hash, ..
            } => Some((path.clone(), Some(post_hash.clone()))),
            EffectView::Deleted { path } => Some((path.clone(), None)),
            _ => None,
        })
        .collect();
    let applied = diff
        .as_ref()
        .filter(|d| !d.truncated)
        .map(|d| d.unified.clone());
    let texts = tokio::task::spawn_blocking(move || {
        if let Some(found) = applied.and_then(|u| map::applied_diffs(&root, &u, &written)) {
            return Some(found);
        }
        let files = call_edits(&log, &id)?;
        files
            .into_iter()
            .map(|(rel, pre, post)| {
                let old = pre.map(String::from_utf8).transpose().ok()?;
                let new = post
                    .map(String::from_utf8)
                    .transpose()
                    .ok()?
                    .unwrap_or_default();
                map::diff_content(&map::absolute(&root, &rel), old.as_deref(), &new)
            })
            .collect::<Option<Vec<_>>>()
    })
    .await
    .ok()
    .flatten()
    .filter(|v| !v.is_empty());
    match (texts, diff) {
        (Some(t), _) => t,
        (None, Some(d)) => vec![ToolCallContent::text(map::fenced(&d.unified))],
        (None, None) => Vec::new(),
    }
}

/// A file one call changed: root-relative path, content before (None: created) and after
/// (None: deleted).
type FileChange = (String, Option<Vec<u8>>, Option<Vec<u8>>);

/// The files call `id` changed, from the `Edit` records that follow its `ToolResult` in
/// the log at `path`; None when the log does not have them (or a blob is missing).
fn call_edits(path: &Path, id: &str) -> Option<Vec<FileChange>> {
    let reader = SessionReader::<SessionRecord>::open(path).ok()?;
    let mut found: Option<Vec<FileChange>> = None;
    let mut collecting = false;
    for rec in reader.records() {
        let (_, rec) = rec.ok()?;
        match rec {
            SessionRecord::ToolResult { id: rid, .. } => {
                collecting = rid == id;
                if collecting {
                    found = Some(Vec::new());
                }
            }
            SessionRecord::Edit {
                path,
                pre_image,
                post_image_blob,
                asserted: false,
                ..
            } if collecting => {
                let pre = match &pre_image {
                    Some(b) => Some(reader.read_blob(b).ok()?),
                    None => None,
                };
                let post = match &post_image_blob {
                    Some(b) => Some(reader.read_blob(b).ok()?),
                    None => None,
                };
                found.as_mut()?.push((path.as_str().to_string(), pre, post));
            }
            _ => collecting = false,
        }
    }
    found
}

/// The updates that replay a resumed session (spec 7.11.9): its user messages, the
/// agent's messages and its tool calls in their final state, from core's playback mapping
/// ([`tautly_core::replay::events`]) with each finished edit's diff from the `Edit` records
/// behind its result (a rewind's asserted contents left out, as in turn diffs). `out`
/// returning false (nobody reads any more) stops it. Blocking file I/O.
fn replay(log: &Path, root: &Path, out: &mut dyn FnMut(Value) -> bool) {
    let Ok(reader) = SessionReader::<SessionRecord>::open(log) else {
        return;
    };
    // Each call's `Edit` records (blob references only), read ahead of the events.
    let mut edits: HashMap<String, Vec<SessionRecord>> = HashMap::new();
    let mut after: Option<String> = None;
    for rec in reader.records() {
        let Ok((_, rec)) = rec else {
            break;
        };
        match rec {
            SessionRecord::ToolResult { id, .. } => after = Some(id),
            rec @ SessionRecord::Edit {
                asserted: false, ..
            } => {
                if let Some(id) = &after {
                    edits.entry(id.clone()).or_default().push(rec);
                }
            }
            _ => after = None,
        }
    }
    let text = |b: &Option<_>| match b {
        Some(b) => reader
            .read_blob(b)
            .ok()
            .and_then(|v| String::from_utf8(v).ok())
            .map(Some),
        None => Some(None),
    };
    let mut calls: HashMap<String, (String, Value)> = HashMap::new();
    for (_, _, ev) in tautly_core::replay::events(&reader, None) {
        let update = match ev {
            Event::UserMessage { text } => json!({
                "sessionUpdate": "user_message_chunk",
                "content": ContentBlock::text(text),
            }),
            Event::TextDelta { text } => chunk(&text),
            Event::ToolCallProposed {
                call_id,
                tool,
                args,
            } => {
                calls.insert(call_id, (tool, args));
                continue;
            }
            Event::ToolFinished {
                call_id,
                output,
                is_error,
                blocked,
                ..
            } => {
                let (tool, args) = calls.remove(&call_id).unwrap_or_default();
                let mut content = vec![map::output_content(&output)];
                for rec in edits.remove(&call_id).unwrap_or_default() {
                    if let SessionRecord::Edit {
                        path,
                        pre_image,
                        post_image_blob,
                        ..
                    } = &rec
                        && let (Some(old), Some(new)) = (text(pre_image), text(post_image_blob))
                        && let Some(d) = map::diff_content(
                            &map::absolute(root, path.as_str()),
                            old.as_deref(),
                            new.as_deref().unwrap_or(""),
                        )
                    {
                        content.push(d);
                    }
                }
                let call = ToolCall {
                    tool_call_id: call_id,
                    title: map::tool_title(&tool, &args),
                    kind: map::tool_kind(&tool),
                    status: if is_error || blocked {
                        ToolCallStatus::Failed
                    } else {
                        ToolCallStatus::Completed
                    },
                    content,
                    locations: map::locations(root, &args),
                    raw_input: Some(args),
                };
                let mut v = serde_json::to_value(call).unwrap_or_default();
                v["sessionUpdate"] = json!("tool_call");
                v
            }
            _ => continue,
        };
        if !out(update) {
            return;
        }
    }
}

/// What session `id` has cost so far, from the cost ledger under `data_home` (None: no
/// ledger, or nothing priced). Blocking.
fn ledger_spend(data_home: &Path, id: &str) -> Option<f64> {
    let file = data_home.join(tautly_core::ledger::LEDGER_FILE);
    if !file.exists() {
        return None;
    }
    let totals = tautly_core::Ledger::open(&file)
        .ok()?
        .session_totals(id)
        .ok()?;
    (totals.requests > totals.unpriced).then_some(totals.usd)
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
