//! The JSON-RPC server (spec 3.4): engine commands are methods, engine events are `event`
//! notifications `{session_id, event}`, one JSON message per line.
//!
//! # Methods
//!
//! - `initialize {protocol?, client?: {interactive, name, version}, session?}` must come
//!   first. It refuses a protocol whose major version is not 1 (`"1"`, `"1.4"` and
//!   `"1.0.3"` are all 1) and returns `{protocol: "1.0", session_id}`. The session is the
//!   *default session*, opened on the server's root (resuming the session the server was
//!   told to); `session: false` opens none (`session_id: null`), for clients that open
//!   their own (refused by the single-session form). With `client.interactive: true`,
//!   every session of the connection asks: `ask` emits `ToolApprovalRequested{request_id}`
//!   and waits for `approve`/`deny`, memory proposals wait for
//!   `approve_memory`/`reject_memory`. Otherwise the headless rules apply (`ask` resolves
//!   to deny, proposals are rejected unless the session's configuration approves them),
//!   and nothing ever waits for input. The default session is built off the reader, which
//!   goes on reading (and hearing the stop future) meanwhile; requests sent before
//!   `initialize` has answered are held (up to 256, more are refused with `-32002`) and
//!   handled in order once it has, also when the input ends right behind them.
//! - The engine commands, by their wire names, with the command's fields as params:
//!   `send_message`, `approve`, `deny`, `cancel`, `set_budget`, `compact`, `uncompact`,
//!   `pin`, `unpin`, `approve_memory`, `reject_memory`, `consolidate_memory`, `fork`,
//!   `set_model`, `diff`, `undo`, `rewind`, `switch`, `label` ([`command_for`]; `switch`
//!   names the session to switch to `target`, since `session_id` addresses the session
//!   the command is for; a value this version does not know, such
//!   as a `set_budget` scope, is `-32602`). Each takes an optional `session_id`:
//!   without one, the default session (or the only open session). The result is `null`
//!   once the session has the command; what it does arrives as events.
//! - `session/new {root, resume?, fork?: {session_id, at?, label?}}` opens a session on
//!   the work tree `root` (absolute) belongs to and returns `{session_id}`. Sessions on
//!   one root share a `Workspace` (code index, watcher, memory store: opened once, closed
//!   with the last session). `fork` branches a session that is not open here (its log up
//!   to step `at`) and opens the branch.
//! - `session/tree {root, session_id?}` returns `{nodes}`: the sessions of the project
//!   the work tree `root` (absolute) belongs to, as a tree in depth-first order (spec
//!   7.12.3; each node `{session_id, parent: [id, step] | null, label, origin, created,
//!   steps, last_prompt, cost_usd, live, current}`), `current` marking the session
//!   `session_id` names (default: the default session) is on now. A session that rewinds
//!   or switches announces it like a fork (below).
//! - `session/list` returns `{sessions: [{session_id, root}]}`; `session/close
//!   {session_id}` shuts one session down and answers once its last event is out.
//! - `shutdown` shuts every session down, answers `null` once their events are out, and
//!   ends the server.
//!
//! A session that stops by itself (a signal, a crash) is taken out of the server after its
//! last event and announced with a `session/closed {session_id}` notification; so is every
//! open session when the stop future fires (SIGTERM, SIGHUP), after it has shut down. Pending
//! approval requests are denied when a session shuts down, so a client that sends
//! `shutdown` or disconnects (end of input), or a stop future that fires
//! ([`Server::stop_on`]), leaves nothing waiting. A session that forks (`fork`) announces
//! the branch with `session_changed` under its old id and goes by the branch's id
//! afterwards; the old id stays an alias of it until it closes, so a command sent with it
//! right after `fork` reaches the session whether or not the move has been seen.

use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};

use serde_json::{Value, json};
use tautly_core::{BudgetScope, Command, Engine, Event, PROTOCOL_VERSION, Workspace, Workspaces};
use tokio::io::{AsyncBufRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::{JoinHandle, JoinSet};

use crate::RpcError;
use crate::jsonrpc::{
    self, Error, INVALID_PARAMS, INVALID_REQUEST, Incoming, Line, MAX_LINE, METHOD_NOT_FOUND,
    NOT_INITIALIZED, SESSION_FAILED, UNKNOWN_SESSION, UNSUPPORTED_PROTOCOL,
};

/// Why an [`EngineFactory`] could not open a session; its text goes to the client.
pub type FactoryError = Box<dyn std::error::Error + Send + Sync>;

/// What a new session is to be.
pub struct SessionSpec {
    /// Where the client asked for it (the server's root for the default session).
    pub root: PathBuf,
    /// A session to resume (`EngineBuilder::resume`).
    pub resume: Option<String>,
    /// The client answers approval requests (`EngineBuilder::interactive`).
    pub interactive: bool,
    /// The workspace of `root`'s work tree, shared with the other sessions on it
    /// (`EngineBuilder::workspace`).
    pub workspace: Arc<Workspace>,
}

/// Opens a session: loads the configuration for its root and builds the engine with the
/// spec's settings. Called on the blocking pool (building may read the keychain).
pub type EngineFactory =
    Arc<dyn Fn(SessionSpec) -> Result<(Engine, mpsc::Receiver<Event>), FactoryError> + Send + Sync>;

/// The methods that are engine commands.
const COMMAND_METHODS: &[&str] = &[
    "send_message",
    "approve",
    "deny",
    "cancel",
    "set_budget",
    "compact",
    "uncompact",
    "pin",
    "unpin",
    "approve_memory",
    "reject_memory",
    "consolidate_memory",
    "fork",
    "set_model",
    "diff",
    "undo",
    "rewind",
    "switch",
    "label",
    "shutdown",
];

/// Lines waiting for the output before producers wait.
const OUTPUT_CAPACITY: usize = 1024;

/// The command a method names, from its params (by name; `session_id` is the transport's
/// and is ignored). Method names are the commands' wire names, so `send_message
/// {text}` is `Command::SendMessage{text}`.
pub fn command_for(method: &str, params: &Value) -> Result<Command, Error> {
    if !COMMAND_METHODS.contains(&method) {
        return Err(Error::new(
            METHOD_NOT_FOUND,
            format!("unknown method `{method}`"),
        ));
    }
    let mut fields = params.as_object().cloned().unwrap_or_default();
    fields.remove("session_id");
    // `session_id` addresses the session, so `switch` names the session to switch to as
    // `target` (the command's own `session_id`).
    if method == "switch"
        && let Some(target) = fields.remove("target")
    {
        fields.insert("session_id".into(), target);
    }
    // Last, so a `type` among the params cannot name another command.
    fields.insert("type".into(), Value::String(method.to_string()));
    let command = serde_json::from_value(Value::Object(fields)).map_err(|e| {
        Error::new(
            INVALID_PARAMS,
            format!("invalid params for `{method}`: {e}"),
        )
    })?;
    // The engine reads a value it does not know as `Unknown` (so old logs stay readable);
    // a client is told instead that it was not understood.
    match command {
        Command::Unknown
        | Command::SetBudget {
            scope: BudgetScope::Unknown,
            ..
        } => Err(Error::new(
            INVALID_PARAMS,
            format!("invalid params for `{method}`: a value this server does not know"),
        )),
        command => Ok(command),
    }
}

/// Serve one prebuilt session over `stdin`/`stdout` (the single-session form): `initialize`
/// returns its id, and it runs with the approval mode it was built with. Ends at
/// `shutdown` or the end of input, after the session has shut down.
pub async fn serve(
    engine: Engine,
    events: mpsc::Receiver<Event>,
    stdin: impl AsyncBufRead + Unpin,
    stdout: impl AsyncWrite + Unpin + Send + 'static,
) -> Result<(), RpcError> {
    Server {
        factory: None,
        root: PathBuf::new(),
        resume: None,
        start_commands: Vec::new(),
        prebuilt: Some((engine, events)),
        stop: None,
    }
    .serve(stdin, stdout)
    .await
}

/// A JSON-RPC server that opens its sessions itself (see the module docs).
pub struct Server {
    factory: Option<(EngineFactory, Workspaces)>,
    root: PathBuf,
    resume: Option<String>,
    start_commands: Vec<Command>,
    prebuilt: Option<(Engine, mpsc::Receiver<Event>)>,
    stop: Option<Pin<Box<dyn Future<Output = ()> + Send>>>,
}

impl Server {
    /// A server opening sessions with `factory`, sharing workspaces through `workspaces`;
    /// the default session runs on `root`.
    pub fn new(factory: EngineFactory, workspaces: Workspaces, root: impl Into<PathBuf>) -> Server {
        Server {
            factory: Some((factory, workspaces)),
            root: root.into(),
            resume: None,
            start_commands: Vec::new(),
            prebuilt: None,
            stop: None,
        }
    }

    /// The default session resumes session `id` (`--resume`, `--continue`).
    pub fn resume(mut self, id: Option<String>) -> Server {
        self.resume = id;
        self
    }

    /// Commands every new session gets before any of the client's (`--budget-usd`).
    pub fn start_commands(mut self, commands: Vec<Command>) -> Server {
        self.start_commands = commands;
        self
    }

    /// Stop serving when `stop` completes (`tautly --rpc`: SIGTERM, SIGHUP): as on a
    /// disconnect, but each session is announced with `session/closed` once its last event
    /// is out.
    pub fn stop_on(mut self, stop: impl Future<Output = ()> + Send + 'static) -> Server {
        self.stop = Some(Box::pin(stop));
        self
    }

    /// Serve until `shutdown`, the end of `input` (a disconnect) or the stop future; every
    /// session is shut down (its pending approvals denied) and its events written before
    /// this returns (a session still being built is not waited for: it is shut down
    /// whenever its build ends).
    /// An error only when `input` cannot be read (the sessions are shut down all the same).
    pub async fn serve(
        self,
        mut input: impl AsyncBufRead + Unpin,
        output: impl AsyncWrite + Unpin + Send + 'static,
    ) -> Result<(), RpcError> {
        let (out, lines) = mpsc::channel(OUTPUT_CAPACITY);
        let mut writer = tokio::spawn(write_lines(lines, output));
        let mut stop = self
            .stop
            .unwrap_or_else(|| Box::pin(std::future::pending()));
        let mut conn = Conn {
            shared: Arc::new(Shared {
                factory: self.factory,
                start_commands: self.start_commands,
                out,
                state: Mutex::default(),
                closed: watch::Sender::new(false),
            }),
            root: self.root,
            resume: self.resume,
            prebuilt: self.prebuilt,
            interactive: false,
            tasks: JoinSet::new(),
            init_done: None,
            deferred: VecDeque::new(),
        };
        let mut reader = jsonrpc::LineReader::default();
        let mut eof = false;
        let mut result = Ok(());
        let mut writer_done = false;
        let mut stopped = false;
        loop {
            while conn.tasks.try_join_next().is_some() {}
            // The input ended: what was sent before it still gets its answer, so wait for
            // `initialize` and the requests behind it.
            if eof && conn.init_done.is_none() {
                break;
            }
            let read = tokio::select! {
                read = reader.read(&mut input, MAX_LINE), if !eof => Some(read),
                () = conn.init_finished() => None,
                _ = &mut writer => {
                    // The output is gone: the client cannot hear us any more.
                    writer_done = true;
                    break;
                }
                () = &mut stop => {
                    stopped = true;
                    break;
                }
            };
            let Some(read) = read else {
                // `initialize` has answered: the requests that came behind it, in order.
                if conn.run_deferred().await == Flow::Stop {
                    break;
                }
                continue;
            };
            match read {
                Ok(Line::Eof) => eof = true,
                Err(e) => {
                    result = Err(RpcError::Input(e));
                    break;
                }
                Ok(Line::TooLong) => {
                    let err = Error::new(
                        INVALID_REQUEST,
                        format!("a message may be at most {MAX_LINE} bytes"),
                    );
                    conn.shared
                        .send(jsonrpc::error_response(&Value::Null, &err))
                        .await;
                }
                Ok(Line::Line(line)) => {
                    if line.iter().all(u8::is_ascii_whitespace) {
                        continue;
                    }
                    match jsonrpc::parse(&line) {
                        Err((id, err)) => conn.shared.reply(id.as_ref(), Err(err)).await,
                        Ok(msg) => {
                            if conn.handle(msg).await == Flow::Stop {
                                break;
                            }
                        }
                    }
                }
            }
        }
        conn.close_all(stopped).await;
        drop(conn);
        if !writer_done {
            let _ = writer.await;
        }
        result
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Flow {
    Go,
    Stop,
}

/// What the reader and the tasks it spawns share.
struct Shared {
    factory: Option<(EngineFactory, Workspaces)>,
    start_commands: Vec<Command>,
    out: mpsc::Sender<String>,
    state: Mutex<State>,
    /// Set once the server closes: a build still running is no longer waited for.
    closed: watch::Sender<bool>,
}

#[derive(Default)]
struct State {
    sessions: HashMap<u64, Slot>,
    /// Session ids to slots: each session's current id, and the ids it had before a fork
    /// (aliases until it closes, unless a later session takes the id).
    ids: HashMap<String, u64>,
    default: Option<u64>,
    next: u64,
    /// `shutdown` or a disconnect: sessions that finish opening are shut down at once.
    closing: bool,
    init: Init,
    /// Sessions taken out by a stop (a signal) that are announced with `session/closed`
    /// when their events end, though no longer reachable.
    announce: HashSet<u64>,
    /// The MCP server approval requests each session has sent (by slot): an `approve` of
    /// one with a `session_rule` is refused (`-32602`), since session rules are for tool
    /// calls only.
    server_requests: HashSet<(u64, String)>,
}

/// How far `initialize` has got.
#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum Init {
    #[default]
    Not,
    /// The default session is being built (off the reader, which goes on reading).
    Building,
    Done,
}

struct Slot {
    id: String,
    /// The work tree; unknown for a prebuilt engine.
    root: Option<PathBuf>,
    engine: Arc<Engine>,
    forwarder: JoinHandle<()>,
}

impl Shared {
    /// Queue a line for the output; dropped when the output is gone.
    async fn send(&self, line: String) {
        let _ = self.out.send(line).await;
    }

    async fn reply(&self, id: Option<&Value>, result: Result<Value, Error>) {
        let Some(id) = id else {
            return;
        };
        let line = match result {
            Ok(v) => jsonrpc::response(id, v),
            Err(e) => jsonrpc::error_response(id, &e),
        };
        self.send(line).await;
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Open a session (on the blocking pool) and hand it the start commands. Returns the
    /// engine, its events and the root of its work tree. If the server closes before the
    /// build ends (it may wait on a keychain dialog), it is not waited for: an error is
    /// returned, and the session is shut down whenever the build finishes.
    async fn open(
        &self,
        root: PathBuf,
        resume: Option<String>,
        interactive: bool,
    ) -> Result<(Engine, mpsc::Receiver<Event>, PathBuf), Error> {
        let Some((factory, workspaces)) = self.factory.clone() else {
            return Err(Error::new(
                SESSION_FAILED,
                "this server serves one session and cannot open others",
            ));
        };
        let mut build = tokio::task::spawn_blocking(move || -> Result<_, FactoryError> {
            let workspace = workspaces.get(&root)?;
            let session_root = workspace.root().to_path_buf();
            let (engine, events) = factory(SessionSpec {
                root,
                resume,
                interactive,
                workspace,
            })?;
            Ok((engine, events, session_root))
        });
        let mut closed = self.closed.subscribe();
        let opened = tokio::select! {
            joined = &mut build => joined,
            _ = closed.wait_for(|closed| *closed) => {
                tokio::spawn(async move {
                    if let Ok(Ok((engine, events, _))) = build.await {
                        shut_down(engine, events).await;
                    }
                });
                return Err(Error::new(SESSION_FAILED, "the server is shutting down"));
            }
        }
        .map_err(|e| Error::new(SESSION_FAILED, format!("cannot open the session: {e}")))?;
        let (engine, events, root) = opened
            .map_err(|e| Error::new(SESSION_FAILED, format!("cannot open the session: {e}")))?;
        for cmd in &self.start_commands {
            let _ = engine.send(cmd.clone()).await;
        }
        Ok((engine, events, root))
    }

    /// Make an open session reachable under `id` and start forwarding its events (after
    /// `early`, events already read), which wait until the returned sender fires or is
    /// dropped: the caller first queues its response naming the session. A session opened
    /// while the server is closing is shut down instead (its events dropped) before this
    /// returns.
    async fn register(
        self: &Arc<Self>,
        id: String,
        root: Option<PathBuf>,
        engine: Engine,
        events: mpsc::Receiver<Event>,
        early: Vec<Event>,
        default: bool,
    ) -> Option<oneshot::Sender<()>> {
        {
            let mut st = self.lock();
            if !st.closing {
                let (go, wait) = oneshot::channel();
                let key = st.next;
                st.next += 1;
                let forwarder = tokio::spawn(forward(
                    Arc::clone(self),
                    key,
                    id.clone(),
                    early,
                    events,
                    wait,
                ));
                st.ids.insert(id.clone(), key);
                st.sessions.insert(
                    key,
                    Slot {
                        id,
                        root,
                        engine: Arc::new(engine),
                        forwarder,
                    },
                );
                if default {
                    st.default = Some(key);
                }
                return Some(go);
            }
        }
        shut_down(engine, events).await;
        None
    }

    /// The session a method addresses: `session_id`, else the default session, else the
    /// only one open.
    fn session_key(&self, params: &Value) -> Result<(u64, Arc<Engine>), Error> {
        let st = self.lock();
        let key = match params.get("session_id") {
            Some(Value::String(id)) => *st
                .ids
                .get(id)
                .ok_or_else(|| Error::new(UNKNOWN_SESSION, format!("no open session `{id}`")))?,
            Some(Value::Null) | None => match (st.default, st.sessions.len()) {
                (Some(key), _) => key,
                (None, 1) => *st.sessions.keys().next().expect("one session"),
                (None, 0) => return Err(Error::new(UNKNOWN_SESSION, "no session is open")),
                (None, n) => {
                    return Err(Error::new(
                        UNKNOWN_SESSION,
                        format!("{n} sessions are open: name one with `session_id`"),
                    ));
                }
            },
            Some(_) => return Err(Error::new(INVALID_PARAMS, "`session_id` must be a string")),
        };
        Ok((key, Arc::clone(&st.sessions[&key].engine)))
    }

    /// Session `key` sent an MCP server approval request `request_id`.
    fn server_request(&self, key: u64, request_id: &str) {
        self.lock()
            .server_requests
            .insert((key, request_id.to_string()));
    }

    fn is_server_request(&self, key: u64, request_id: &str) -> bool {
        self.lock()
            .server_requests
            .contains(&(key, request_id.to_string()))
    }

    /// Take session `id` (its current id or an alias) out of the server: none of its ids
    /// can be addressed any more.
    fn remove(&self, id: &str) -> Option<Slot> {
        let key = *self.lock().ids.get(id)?;
        self.remove_key(key)
    }

    fn remove_key(&self, key: u64) -> Option<Slot> {
        let mut st = self.lock();
        let slot = st.sessions.remove(&key)?;
        st.ids.retain(|_, k| *k != key);
        st.server_requests.retain(|(k, _)| *k != key);
        if st.default == Some(key) {
            st.default = None;
        }
        Some(slot)
    }

    /// A session's events ended: whether it is to be announced as closed (it was still open
    /// here, or a stop took it out and asked for the announcement); it is out either way.
    fn ended(&self, key: u64) -> bool {
        let announced = self.lock().announce.remove(&key);
        self.remove_key(key).is_some() || announced
    }

    /// Stop new sessions and take every open one out; with `announce`, each is still
    /// announced as closed when its events end.
    fn remove_all(&self, announce: bool) -> Vec<Slot> {
        self.closed.send_replace(true);
        let mut st = self.lock();
        st.closing = true;
        st.ids.clear();
        st.server_requests.clear();
        st.default = None;
        let mut slots: Vec<(u64, Slot)> = st.sessions.drain().collect();
        slots.sort_by_key(|(key, _)| *key);
        if announce {
            st.announce.extend(slots.iter().map(|(key, _)| *key));
        }
        slots.into_iter().map(|(_, slot)| slot).collect()
    }

    /// A session forked: it goes by `new` from now on, and its old id stays an alias.
    fn renamed(&self, key: u64, new: &str) {
        let mut st = self.lock();
        if let Some(slot) = st.sessions.get_mut(&key) {
            slot.id = new.to_string();
            st.ids.insert(new.to_string(), key);
        }
    }

    /// Whether a session open here goes by `id` now (aliases aside).
    fn is_open(&self, id: &str) -> bool {
        self.lock().sessions.values().any(|s| s.id == id)
    }
}

/// Shut sessions down together: `Shutdown` to each (which denies its pending approvals),
/// then wait until each one's last event is out.
async fn close(slots: Vec<Slot>) {
    for slot in &slots {
        let _ = slot.engine.send(Command::Shutdown).await;
    }
    for slot in slots {
        drop(slot.engine);
        let _ = slot.forwarder.await;
    }
}

/// Shut down a session nobody will address, dropping its events.
async fn shut_down(engine: Engine, mut events: mpsc::Receiver<Event>) {
    let _ = engine.send(Command::Shutdown).await;
    drop(engine);
    while events.recv().await.is_some() {}
}

/// Write session `key`'s events as notifications, once `go` fires (or is dropped), until
/// its event stream ends. A `session_changed` naming another id (a fork) is sent under the
/// old id; later events carry the new one. If the output is gone, events are still read
/// (and dropped), so the session never waits on a client that left. A session whose
/// stream ends while it is still open here (it stopped by itself: a signal, a crash) is
/// taken out and announced with `session/closed {session_id}`, after its last event.
async fn forward(
    shared: Arc<Shared>,
    key: u64,
    mut id: String,
    early: Vec<Event>,
    mut events: mpsc::Receiver<Event>,
    go: oneshot::Receiver<()>,
) {
    let _ = go.await;
    let mut early = early.into_iter();
    loop {
        let ev = match early.next() {
            Some(ev) => ev,
            None => match events.recv().await {
                Some(ev) => ev,
                None => {
                    if shared.ended(key) {
                        let params = json!({ "session_id": id });
                        shared
                            .send(jsonrpc::notification("session/closed", params))
                            .await;
                    }
                    return;
                }
            },
        };
        let renamed = match &ev {
            Event::SessionChanged { session_id } if *session_id != id => Some(session_id.clone()),
            _ => None,
        };
        // Routed by the new id before the client can learn it.
        if let Some(new) = &renamed {
            shared.renamed(key, new);
        }
        // Known before the client can answer it.
        if let Event::McpServerApprovalRequested { request_id, .. } = &ev {
            shared.server_request(key, request_id);
        }
        let event = serde_json::to_value(&ev).unwrap_or_else(
            |e| json!({"type": "warning", "message": format!("an event could not be sent: {e}")}),
        );
        let params = json!({"session_id": id, "event": event});
        shared.send(jsonrpc::notification("event", params)).await;
        if let Some(new) = renamed {
            id = new;
        }
    }
}

/// Write `lines` to `output`, one per line, until the senders are gone or a write fails.
async fn write_lines(mut lines: mpsc::Receiver<String>, mut output: impl AsyncWrite + Unpin) {
    while let Some(mut line) = lines.recv().await {
        line.push('\n');
        if output.write_all(line.as_bytes()).await.is_err() || output.flush().await.is_err() {
            return;
        }
    }
}

/// A checked `initialize`.
struct Handshake {
    interactive: bool,
    open_default: bool,
}

/// The connection: what only the reader touches.
struct Conn {
    shared: Arc<Shared>,
    root: PathBuf,
    resume: Option<String>,
    prebuilt: Option<(Engine, mpsc::Receiver<Event>)>,
    interactive: bool,
    /// Requests answered by a task of their own (`session/new`, `session/close`).
    tasks: JoinSet<()>,
    /// Fires when the task building the default session is done (`initialize` answered).
    init_done: Option<oneshot::Receiver<()>>,
    /// Messages that came while it was building, handled once it is done.
    deferred: VecDeque<Incoming>,
}

/// Messages held while `initialize` builds the default session; more are refused.
const MAX_DEFERRED: usize = 256;

impl Conn {
    async fn handle(&mut self, msg: Incoming) -> Flow {
        let id = msg.id().cloned();
        if self.init_done.is_some() {
            // Requests sent right behind `initialize` run in order once it has answered.
            if self.deferred.len() >= MAX_DEFERRED {
                let err = Error::new(
                    NOT_INITIALIZED,
                    "too many requests before `initialize` answered: wait for its response",
                );
                self.shared.reply(id.as_ref(), Err(err)).await;
            } else {
                self.deferred.push_back(msg);
            }
            return Flow::Go;
        }
        let method = msg.method().to_string();
        let params = msg.params().clone();
        if self.shared.lock().init != Init::Done && method != "initialize" {
            let err = Error::new(NOT_INITIALIZED, "send `initialize` first");
            self.shared.reply(id.as_ref(), Err(err)).await;
            return Flow::Go;
        }
        match method.as_str() {
            "initialize" => self.initialize(id, &params).await,
            "session/new" => {
                let shared = Arc::clone(&self.shared);
                let interactive = self.interactive;
                self.tasks.spawn(async move {
                    match session_new(&shared, &params, interactive).await {
                        Ok((reply, go)) => {
                            shared.reply(id.as_ref(), Ok(reply)).await;
                            drop(go);
                        }
                        Err(e) => shared.reply(id.as_ref(), Err(e)).await,
                    }
                });
            }
            "session/list" => {
                let sessions: Vec<Value> = {
                    let st = self.shared.lock();
                    let mut slots: Vec<(&u64, &Slot)> = st.sessions.iter().collect();
                    slots.sort_by_key(|(key, _)| **key);
                    slots
                        .into_iter()
                        .map(|(_, s)| {
                            // Lossy: a path that is not UTF-8 must not fail the listing.
                            let root = s.root.as_ref().map(|r| r.to_string_lossy());
                            json!({"session_id": s.id, "root": root})
                        })
                        .collect()
                };
                let result = Ok(json!({ "sessions": sessions }));
                self.shared.reply(id.as_ref(), result).await;
            }
            "session/tree" => {
                let shared = Arc::clone(&self.shared);
                self.tasks.spawn(async move {
                    let result = session_tree(&shared, &params).await;
                    shared.reply(id.as_ref(), result).await;
                });
            }
            "session/close" => {
                let shared = Arc::clone(&self.shared);
                let slot = match params.get("session_id") {
                    Some(Value::String(sid)) => shared.remove(sid).ok_or_else(|| {
                        Error::new(UNKNOWN_SESSION, format!("no open session `{sid}`"))
                    }),
                    _ => Err(Error::new(INVALID_PARAMS, "`session_id` must be a string")),
                };
                match slot {
                    Ok(slot) => {
                        self.tasks.spawn(async move {
                            close(vec![slot]).await;
                            shared.reply(id.as_ref(), Ok(Value::Null)).await;
                        });
                    }
                    Err(e) => self.shared.reply(id.as_ref(), Err(e)).await,
                }
            }
            "shutdown" => {
                self.close_all(false).await;
                self.shared.reply(id.as_ref(), Ok(Value::Null)).await;
                return Flow::Stop;
            }
            _ => {
                let result = self.command(&method, &params).await;
                self.shared.reply(id.as_ref(), result).await;
            }
        }
        Flow::Go
    }

    /// Completes when the default session's build is done (never while none is under
    /// way). Cancel safe.
    async fn init_finished(&mut self) {
        match self.init_done.as_mut() {
            Some(done) => {
                let _ = done.await;
            }
            None => std::future::pending().await,
        }
    }

    /// The build is done: handle the messages held for it.
    async fn run_deferred(&mut self) -> Flow {
        self.init_done = None;
        for msg in std::mem::take(&mut self.deferred) {
            if self.handle(msg).await == Flow::Stop {
                return Flow::Stop;
            }
        }
        Flow::Go
    }

    /// `initialize`: the checks and the single-session forms answer here; building the
    /// default session (it may wait on a keychain dialog) is a task of its own, so the
    /// reader goes on reading input and hearing the stop future meanwhile. What comes in
    /// until it answers is held and handled, in order, afterwards.
    async fn initialize(&mut self, id: Option<Value>, params: &Value) {
        let plan = match self.check_handshake(params) {
            Ok(plan) => plan,
            Err(e) => {
                self.shared.reply(id.as_ref(), Err(e)).await;
                return;
            }
        };
        let shared = Arc::clone(&self.shared);
        let version = format!("{}.{}", PROTOCOL_VERSION.0, PROTOCOL_VERSION.1);
        if !plan.open_default {
            shared.lock().init = Init::Done;
            let reply = json!({"protocol": version, "session_id": null});
            shared.reply(id.as_ref(), Ok(reply)).await;
            return;
        }
        if let Some((engine, events)) = self.prebuilt.take() {
            let sid = engine.session_id().to_string();
            let go = shared
                .register(sid.clone(), None, engine, events, Vec::new(), true)
                .await;
            shared.lock().init = Init::Done;
            shared
                .reply(
                    id.as_ref(),
                    Ok(json!({"protocol": version, "session_id": sid})),
                )
                .await;
            drop(go);
            return;
        }
        let (root, resume, interactive) =
            (self.root.clone(), self.resume.clone(), plan.interactive);
        let (done, init_done) = oneshot::channel();
        self.init_done = Some(init_done);
        self.tasks.spawn(async move {
            let _done = done;
            match shared.open(root, resume, interactive).await {
                Ok((engine, events, root)) => {
                    let sid = engine.session_id().to_string();
                    let go = shared
                        .register(sid.clone(), Some(root), engine, events, Vec::new(), true)
                        .await;
                    shared.lock().init = Init::Done;
                    let reply = json!({"protocol": version, "session_id": sid});
                    shared.reply(id.as_ref(), Ok(reply)).await;
                    drop(go);
                }
                Err(e) => {
                    shared.lock().init = Init::Not;
                    shared.reply(id.as_ref(), Err(e)).await;
                }
            }
        });
    }

    /// Check the handshake's params and mark `initialize` as under way.
    fn check_handshake(&mut self, params: &Value) -> Result<Handshake, Error> {
        if self.shared.lock().init != Init::Not {
            return Err(Error::new(INVALID_REQUEST, "already initialized"));
        }
        check_protocol(params.get("protocol"))?;
        let interactive = match params.get("client") {
            None | Some(Value::Null) => false,
            Some(Value::Object(client)) => match client.get("interactive") {
                None | Some(Value::Null) => false,
                Some(Value::Bool(b)) => *b,
                Some(_) => {
                    return Err(Error::new(
                        INVALID_PARAMS,
                        "`client.interactive` must be a boolean",
                    ));
                }
            },
            Some(_) => return Err(Error::new(INVALID_PARAMS, "`client` must be an object")),
        };
        let open_default = match params.get("session") {
            None | Some(Value::Null) => true,
            Some(Value::Bool(b)) => *b,
            Some(_) => return Err(Error::new(INVALID_PARAMS, "`session` must be a boolean")),
        };
        if !open_default && self.prebuilt.is_some() {
            return Err(Error::new(
                INVALID_PARAMS,
                "this server serves one session: `session: false` would leave it unserved",
            ));
        }
        self.interactive = interactive;
        self.shared.lock().init = Init::Building;
        Ok(Handshake {
            interactive,
            open_default,
        })
    }

    async fn command(&self, method: &str, params: &Value) -> Result<Value, Error> {
        let cmd = command_for(method, params)?;
        let (key, engine) = self.shared.session_key(params)?;
        if let Command::Approve {
            request_id,
            session_rule: Some(_),
        } = &cmd
            && self.shared.is_server_request(key, request_id)
        {
            return Err(Error::new(
                INVALID_PARAMS,
                format!(
                    "invalid params for `approve`: `{request_id}` is an MCP server approval; \
                     session_rule applies to tool calls only"
                ),
            ));
        }
        engine
            .send(cmd)
            .await
            .map_err(|_| Error::new(SESSION_FAILED, "the session has stopped"))?;
        Ok(Value::Null)
    }

    /// Shut every session down, the prebuilt one too if it was never served, and wait
    /// for the requests in flight (a session that finishes opening meanwhile is shut down
    /// at once; a build still running is not waited for), until all their events are out.
    /// With `announce` (a signal), each session is then announced with `session/closed`.
    async fn close_all(&mut self, announce: bool) {
        let slots = self.shared.remove_all(announce);
        let prebuilt = self.prebuilt.take();
        let tasks = &mut self.tasks;
        tokio::join!(
            close(slots),
            async { while tasks.join_next().await.is_some() {} },
            async {
                if let Some((engine, events)) = prebuilt {
                    shut_down(engine, events).await;
                }
            }
        );
    }
}

/// `initialize`'s `protocol`: absent (this version), or `"<major>[.<minor>[.<patch>]]"`
/// with major 1 (minor and patch are not checked: minor versions are compatible).
fn check_protocol(protocol: Option<&Value>) -> Result<(), Error> {
    let supported = format!("{}.{}", PROTOCOL_VERSION.0, PROTOCOL_VERSION.1);
    let refuse = |what: String| {
        Err(Error::new(UNSUPPORTED_PROTOCOL, what).with_data(json!({ "supported": supported })))
    };
    let text = match protocol {
        None | Some(Value::Null) => return Ok(()),
        Some(Value::String(s)) => s.as_str(),
        Some(other) => return refuse(format!("`protocol` must be a version string, not {other}")),
    };
    let numbers: Option<Vec<u32>> = text.split('.').map(|p| p.parse::<u32>().ok()).collect();
    let major = match numbers {
        Some(n) if n.len() <= 3 => n.first().copied(),
        _ => None,
    };
    match major {
        Some(major) if major == PROTOCOL_VERSION.0 => Ok(()),
        Some(_) => refuse(format!(
            "protocol {text} is not supported: this server speaks {supported}"
        )),
        _ => refuse(format!("`{text}` is not a protocol version")),
    }
}

/// `session/tree`: the project's sessions as a tree.
async fn session_tree(shared: &Arc<Shared>, params: &Value) -> Result<Value, Error> {
    let root = match params.get("root") {
        Some(Value::String(r)) if Path::new(r).is_absolute() => PathBuf::from(r),
        Some(Value::String(_)) => {
            return Err(Error::new(
                INVALID_PARAMS,
                "`root` must be an absolute path",
            ));
        }
        _ => return Err(Error::new(INVALID_PARAMS, "`root` (a path) is required")),
    };
    let current = match params.get("session_id") {
        Some(Value::String(_)) | None | Some(Value::Null) => shared
            .session_key(params)
            .ok()
            .map(|(_, engine)| engine.current_session().0),
        Some(_) => return Err(Error::new(INVALID_PARAMS, "`session_id` must be a string")),
    };
    let data_home = match &shared.factory {
        Some((_, workspaces)) => workspaces.data_home().to_path_buf(),
        None => tautly_core::data_home()
            .map_err(|e| Error::new(SESSION_FAILED, format!("no data directory: {e}")))?,
    };
    let nodes = tokio::task::spawn_blocking(move || {
        tautly_core::tree::for_root(&data_home, &root, current.as_deref())
    })
    .await
    .map_err(|e| Error::new(SESSION_FAILED, e.to_string()))?
    .map_err(|e| Error::new(SESSION_FAILED, format!("cannot list the sessions: {e}")))?;
    Ok(json!({ "nodes": nodes }))
}

/// `session/new`: the reply and the new session's go-ahead.
async fn session_new(
    shared: &Arc<Shared>,
    params: &Value,
    interactive: bool,
) -> Result<(Value, Option<oneshot::Sender<()>>), Error> {
    let root = match params.get("root") {
        Some(Value::String(r)) if Path::new(r).is_absolute() => PathBuf::from(r),
        Some(Value::String(_)) => {
            return Err(Error::new(
                INVALID_PARAMS,
                "`root` must be an absolute path",
            ));
        }
        _ => return Err(Error::new(INVALID_PARAMS, "`root` (a path) is required")),
    };
    let resume = match params.get("resume") {
        None | Some(Value::Null) => None,
        Some(Value::String(id)) => Some(id.clone()),
        Some(_) => return Err(Error::new(INVALID_PARAMS, "`resume` must be a session id")),
    };
    let fork = match params.get("fork") {
        None | Some(Value::Null) => None,
        Some(f) => Some(ForkSpec::parse(f)?),
    };
    match (resume, fork) {
        (Some(_), Some(_)) => Err(Error::new(
            INVALID_PARAMS,
            "`resume` and `fork` cannot be combined",
        )),
        (resume, None) => {
            let (engine, events, root) = shared.open(root, resume, interactive).await?;
            let sid = engine.session_id().to_string();
            let go = shared
                .register(sid.clone(), Some(root), engine, events, Vec::new(), false)
                .await;
            Ok((json!({ "session_id": sid }), go))
        }
        (None, Some(fork)) => {
            if shared.is_open(&fork.session_id) {
                return Err(Error::new(
                    SESSION_FAILED,
                    format!(
                        "session `{}` is open here: send it `fork` (it moves to the branch), \
                         or close it first",
                        fork.session_id
                    ),
                ));
            }
            let parent = fork.session_id.clone();
            let (engine, mut events, root) =
                shared.open(root, Some(parent.clone()), interactive).await?;
            let sent = engine
                .send(Command::Fork {
                    at: fork.at,
                    label: fork.label,
                })
                .await;
            let mut early = Vec::new();
            let mut failure = sent.err().map(|_| "the session stopped".to_string());
            let mut branch = None;
            while failure.is_none() && branch.is_none() {
                match events.recv().await {
                    None => failure = Some("the session stopped".into()),
                    Some(Event::SessionChanged { session_id }) if session_id == parent => {}
                    Some(Event::SessionChanged { session_id }) => {
                        branch = Some(session_id.clone());
                        early.push(Event::SessionChanged { session_id });
                    }
                    Some(Event::Error { message, .. }) => failure = Some(message),
                    Some(ev) => early.push(ev),
                }
            }
            match (branch, failure) {
                (Some(id), _) => {
                    let go = shared
                        .register(id.clone(), Some(root), engine, events, early, false)
                        .await;
                    Ok((json!({ "session_id": id }), go))
                }
                (None, why) => {
                    shut_down(engine, events).await;
                    Err(Error::new(
                        SESSION_FAILED,
                        format!("cannot fork `{parent}`: {}", why.unwrap_or_default()),
                    ))
                }
            }
        }
    }
}

/// `session/new`'s `fork`.
struct ForkSpec {
    session_id: String,
    at: Option<u32>,
    label: Option<String>,
}

impl ForkSpec {
    fn parse(v: &Value) -> Result<ForkSpec, Error> {
        let invalid = |m: &str| Error::new(INVALID_PARAMS, m.to_string());
        let Value::Object(f) = v else {
            return Err(invalid("`fork` must be an object"));
        };
        let session_id = match f.get("session_id") {
            Some(Value::String(s)) => s.clone(),
            _ => return Err(invalid("`fork.session_id` (a session id) is required")),
        };
        let at = match f.get("at") {
            None | Some(Value::Null) => None,
            Some(v) => Some(
                v.as_u64()
                    .and_then(|n| u32::try_from(n).ok())
                    .ok_or_else(|| invalid("`fork.at` must be a step number"))?,
            ),
        };
        let label = match f.get("label") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) => Some(s.clone()),
            Some(_) => return Err(invalid("`fork.label` must be a string")),
        };
        if f.get("worktree").and_then(Value::as_bool) == Some(true) {
            return Err(invalid("worktree forks are not supported yet"));
        }
        if f.get("model").is_some_and(|m| !m.is_null()) {
            return Err(invalid(
                "`fork.model` is not supported yet: send `set_model` to the branch",
            ));
        }
        Ok(ForkSpec {
            session_id,
            at,
            label,
        })
    }
}
