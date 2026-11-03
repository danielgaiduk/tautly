//! The engine: one actor task per session that owns the session's state, takes commands
//! and runs each user message as a turn in a child task with a child cancellation token.
//!
//! While a turn runs, the actor still answers `Approve`/`Deny` (by request id), `Cancel`
//! and `Shutdown`; `Pin`/`Unpin` reach the turn at its next step start; everything else
//! (further messages included) waits for the turn to end. Every command is logged as it
//! arrives.
//!
//! Seams for later tasks: budgets, turn timeouts and the cost ledger (Task 25) hook into
//! the step loop after each `Usage` (`turn.rs`); resume and fork (Task 26) build the
//! `Session` from a log instead of [`Session::init`]'s fresh state (`EngineBuilder::resume`
//! is refused until then); side-model routing (Task 27) replaces the provider used by
//! `Session::summarize`; memory proposals (Task 28) plug in at the `ContextOp::Remember`
//! arm of `dispatch.rs` and fill `Session::pending_memory_index`, which the cache epoch
//! already applies.

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tautly_context::Context;
use tautly_provider::{Catalog, Provider, Retrying};
use tautly_sandbox::{LocalSandbox, Policy, PolicyError, Sandbox, SandboxError};
use tautly_session::SessionWriter;
use tautly_tools::{ReadTracker, registry};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::approvals::{Approvals, Verdict};
use crate::attempts::AttemptLedger;
use crate::config::{Config, LoadedConfig};
use crate::protocol::{Command, ErrorKind, Event, FinishReason};
use crate::provider_setup::provider_for;
use crate::record::SessionRecord;
use crate::session::{EventSink, Log, Session};
use crate::turn::TurnControl;
use crate::workspace::Workspace;

/// Capacity of the event channel returned by [`EngineBuilder::build`].
pub const EVENT_CAPACITY: usize = 256;
/// Commands buffered before [`Engine::send`] waits.
const COMMAND_CAPACITY: usize = 64;

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("the engine has stopped")]
    Closed,
    #[error("the engine must be built inside a Tokio runtime")]
    NoRuntime,
    #[error("cannot open the session log: {0}")]
    Session(#[from] tautly_session::Error),
    #[error("cannot set up the sandbox: {0}")]
    Sandbox(#[from] SandboxError),
    #[error("invalid permission rules: {0}")]
    Policy(#[from] PolicyError),
    #[error("{0}")]
    Provider(String),
    #[error("the shared workspace is for {}, not {}", workspace.display(), session.display())]
    WorkspaceRoot {
        workspace: PathBuf,
        session: PathBuf,
    },
    #[error("resuming a session is not available yet")]
    ResumeUnsupported,
}

/// Everything a session needs; see [`EngineBuilder::build`].
pub struct EngineBuilder {
    pub config: Config,
    /// Where the session starts. The sandbox root is the git top level when this is inside
    /// a git work tree, else this directory.
    pub root: PathBuf,
    /// Default: the adapter for `config.model`, with its key from the environment or the
    /// keychain. Always wrapped in `Retrying`.
    pub provider: Option<Arc<dyn Provider>>,
    /// Default: a `LocalSandbox` on the root (or the workspace's sandbox).
    pub sandbox: Option<Arc<dyn Sandbox>>,
    /// A session id to resume (Task 26).
    pub resume: Option<String>,
    /// Index, watcher and memory store shared with other sessions on the same root
    /// (spec 3.4). Default: the builder opens its own.
    pub workspace: Option<Arc<Workspace>>,
    /// An interactive client answers `ToolApprovalRequested`; otherwise (headless) `ask`
    /// resolves to deny and nothing ever waits for input.
    pub interactive: bool,
    /// Default: [`tautly_session::paths::data_home`].
    pub data_home: Option<PathBuf>,
    /// Lines from loading the configuration ([`LoadedConfig::warnings`]), shown as
    /// warnings at session start.
    pub config_warnings: Vec<String>,
}

impl EngineBuilder {
    pub fn new(config: Config, root: impl Into<PathBuf>) -> Self {
        EngineBuilder {
            config,
            root: root.into(),
            provider: None,
            sandbox: None,
            resume: None,
            workspace: None,
            interactive: false,
            data_home: None,
            config_warnings: Vec::new(),
        }
    }

    pub fn from_loaded(loaded: LoadedConfig, root: impl Into<PathBuf>) -> Self {
        EngineBuilder {
            config_warnings: loaded.warnings,
            ..EngineBuilder::new(loaded.config, root)
        }
    }

    /// Create the session log and start the session's actor; returns the handle and the
    /// event stream (capacity [`EVENT_CAPACITY`]; the engine waits when it is full). Must
    /// be called inside a Tokio runtime. Opening the code index and memory store, reading
    /// AGENTS.md and cleaning the spill store happen after the first event
    /// (`SessionChanged`), off this path.
    pub fn build(self) -> Result<(Engine, mpsc::Receiver<Event>), EngineError> {
        let runtime = tokio::runtime::Handle::try_current().map_err(|_| EngineError::NoRuntime)?;
        if self.resume.is_some() {
            return Err(EngineError::ResumeUnsupported);
        }
        let data_home = match self.data_home {
            Some(d) => d,
            None => tautly_session::paths::data_home()?,
        };
        let (sandbox, workspace) = match (self.sandbox, self.workspace) {
            (sandbox, Some(ws)) => {
                let sandbox = sandbox.unwrap_or_else(|| Arc::clone(ws.sandbox()));
                if sandbox.root() != ws.root() {
                    return Err(EngineError::WorkspaceRoot {
                        workspace: ws.root().to_path_buf(),
                        session: sandbox.root().to_path_buf(),
                    });
                }
                (sandbox, ws)
            }
            (Some(sandbox), None) => {
                let ws = Workspace::new(Arc::clone(&sandbox), &data_home);
                (sandbox, ws)
            }
            (None, None) => {
                let sandbox: Arc<dyn Sandbox> =
                    Arc::new(LocalSandbox::new(sandbox_root(&self.root)?)?);
                let ws = Workspace::new(Arc::clone(&sandbox), &data_home);
                (sandbox, ws)
            }
        };
        let root = sandbox.root().to_path_buf();
        let config = self.config;
        let policy = Policy::new(config.permissions.clone(), root.clone())?;
        let mut catalog = Catalog::builtin();
        if let Some(overrides) = config.catalog_overrides.clone() {
            catalog.merge(overrides);
        }
        let catalog = Arc::new(catalog);
        let info = catalog.get(&config.model);
        let provider = match self.provider {
            Some(p) => p,
            None => provider_for(&config, &config.model, Arc::clone(&catalog))
                .map_err(EngineError::Provider)?,
        };
        let provider: Arc<dyn Provider> = Arc::new(Retrying::new(provider));
        let writer = SessionWriter::<SessionRecord>::create_in(&data_home, &root, None)?;
        let session_id = writer.session_id().to_string();
        let session_path = writer.path().to_path_buf();

        let (event_tx, event_rx) = mpsc::channel(EVENT_CAPACITY);
        let (command_tx, command_rx) = mpsc::channel(COMMAND_CAPACITY);
        let tools = registry(&info.caps);
        let specs = tools.iter().map(|t| t.spec(&info.caps)).collect();
        let context = Context {
            root: Some(root.clone()),
            ..Context::default()
        };
        let session = Session {
            context,
            reads: Arc::new(ReadTracker::default()),
            policy: Arc::new(policy),
            ledger: AttemptLedger::default(),
            log: Log::new(writer),
            events: EventSink::new(event_tx),
            sandbox,
            workspace,
            provider,
            history_provider: config.model.provider().to_string(),
            model: config.model.clone(),
            info,
            tools,
            specs,
            max_turns: config.max_turns,
            compaction_threshold: config.compaction_threshold,
            startup: None,
            latest_compaction: None,
            pending_memory_index: None,
            untracked_paths: HashSet::new(),
            index_error_reported: false,
        };
        let actor = Actor {
            log: session.log.clone(),
            events: session.events.clone(),
            approvals: Approvals::new(self.interactive),
            shutdown: CancellationToken::new(),
            session_id: session_id.clone(),
        };
        runtime.spawn(actor.run(
            session,
            command_rx,
            self.config_warnings,
            config.pins.clone(),
        ));
        Ok((
            Engine {
                commands: command_tx,
                session_id,
                session_path,
            },
            event_rx,
        ))
    }
}

/// The git top level above `start` (a `.git` directory or file), else `start` itself.
fn sandbox_root(start: &Path) -> Result<PathBuf, SandboxError> {
    let start = start.canonicalize()?;
    let top = start
        .ancestors()
        .find(|dir| std::fs::symlink_metadata(dir.join(".git")).is_ok())
        .unwrap_or(&start)
        .to_path_buf();
    Ok(top)
}

/// A running session. Dropping it shuts the session down.
pub struct Engine {
    commands: mpsc::Sender<Command>,
    session_id: String,
    session_path: PathBuf,
}

impl Engine {
    /// Hand a command to the session; an error only when the session has stopped.
    pub async fn send(&self, cmd: Command) -> Result<(), EngineError> {
        self.commands
            .send(cmd)
            .await
            .map_err(|_| EngineError::Closed)
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// The session log file.
    pub fn session_path(&self) -> &Path {
        &self.session_path
    }
}

/// The turn task in flight. It hands back the session and the inbox, whose leftovers
/// (sent after the turn's last step start) are applied once it ends.
struct Running {
    handle: JoinHandle<(Session, mpsc::UnboundedReceiver<Command>)>,
    cancel: CancellationToken,
    inbox: mpsc::UnboundedSender<Command>,
}

struct Actor {
    log: Log,
    events: EventSink,
    approvals: Approvals,
    shutdown: CancellationToken,
    session_id: String,
}

impl Actor {
    async fn run(
        self,
        mut session: Session,
        mut commands: mpsc::Receiver<Command>,
        config_warnings: Vec<String>,
        config_pins: Vec<String>,
    ) {
        self.events
            .emit(Event::SessionChanged {
                session_id: self.session_id.clone(),
            })
            .await;
        session.workspace.start();
        for w in config_warnings {
            self.events.warn(w).await;
        }
        session.init(&config_pins).await;

        let mut state = Some(session);
        let mut running: Option<Running> = None;
        let mut queue: VecDeque<Command> = VecDeque::new();
        let mut stopping = false;
        loop {
            if running.is_none() && !stopping {
                while let Some(cmd) = queue.pop_front() {
                    if let Command::SendMessage { text } = cmd {
                        match state.take() {
                            Some(s) => {
                                running = Some(self.spawn_turn(s, text));
                                break;
                            }
                            None => self.lost().await,
                        }
                    } else if let Some(s) = state.as_mut() {
                        self.idle_command(s, cmd).await;
                    }
                }
            }
            if stopping && running.is_none() {
                break;
            }
            tokio::select! {
                cmd = commands.recv(), if !stopping => {
                    let cmd = cmd.unwrap_or(Command::Shutdown);
                    if let Err(f) = self.log.append(&SessionRecord::Command(cmd.clone())) {
                        if let Some(ev) = f.event() {
                            self.events.emit(ev).await;
                        }
                    }
                    match cmd {
                        Command::Approve { request_id } => {
                            self.resolve(&request_id, Verdict::Approved).await;
                        }
                        Command::Deny { request_id, reason } => {
                            self.resolve(&request_id, Verdict::Denied { reason }).await;
                        }
                        Command::Cancel => {
                            if let Some(r) = &running {
                                r.cancel.cancel();
                            }
                        }
                        Command::Shutdown => {
                            stopping = true;
                            self.approvals.deny_all("the session is shutting down");
                            self.shutdown.cancel();
                        }
                        cmd @ (Command::Pin { .. } | Command::Unpin { .. }) if running.is_some() => {
                            if let Some(r) = &running {
                                let _ = r.inbox.send(cmd);
                            }
                        }
                        other => queue.push_back(other),
                    }
                }
                joined = async { (&mut running.as_mut().expect("guarded").handle).await }, if running.is_some() => {
                    running = None;
                    match joined {
                        Ok((mut s, mut inbox)) => {
                            inbox.close();
                            while let Ok(cmd) = inbox.try_recv() {
                                s.context_command(cmd).await;
                            }
                            state = Some(s);
                        }
                        Err(e) => self.crashed(e).await,
                    }
                }
            }
        }
        if let Err(f) = self.log.checkpoint() {
            if let Some(ev) = f.event() {
                self.events.emit(ev).await;
            }
        }
    }

    fn spawn_turn(&self, mut session: Session, text: String) -> Running {
        let cancel = self.shutdown.child_token();
        let (inbox_tx, inbox_rx) = mpsc::unbounded_channel();
        let mut ctl = TurnControl {
            cancel: cancel.clone(),
            inbox: inbox_rx,
            approvals: self.approvals.clone(),
        };
        let handle = tokio::spawn(async move {
            session.run_turn(text, &mut ctl).await;
            (session, ctl.inbox)
        });
        Running {
            handle,
            cancel,
            inbox: inbox_tx,
        }
    }

    async fn resolve(&self, request_id: &str, verdict: Verdict) {
        if !self.approvals.resolve(request_id, verdict) {
            self.events
                .warn(format!("no approval request `{request_id}` is pending"))
                .await;
        }
    }

    /// A command that runs between turns.
    async fn idle_command(&self, session: &mut Session, cmd: Command) {
        match cmd {
            Command::Pin { .. } | Command::Unpin { .. } => session.context_command(cmd).await,
            other => {
                let name = serde_json::to_value(&other)
                    .ok()
                    .and_then(|v| v["type"].as_str().map(str::to_string))
                    .unwrap_or_default();
                self.events
                    .emit(Event::Error {
                        kind: ErrorKind::Internal,
                        message: format!("the `{name}` command is not supported yet"),
                        retryable: false,
                        hint: None,
                    })
                    .await;
            }
        }
    }

    /// A turn task panicked: an engine bug. The state it held is gone, but the log is
    /// intact up to the crash, so the session can be resumed.
    async fn crashed(&self, e: tokio::task::JoinError) {
        let _ = self.log.append(&SessionRecord::TurnEnd {
            reason: FinishReason::Error,
        });
        let _ = self.log.checkpoint();
        self.events
            .emit(Event::Error {
                kind: ErrorKind::Internal,
                message: format!(
                    "engine bug in session {}: the turn stopped ({e}); the session log is intact",
                    self.session_id
                ),
                retryable: false,
                hint: Some("resume the session to continue".into()),
            })
            .await;
        self.events
            .emit(Event::TurnFinished {
                reason: FinishReason::Error,
            })
            .await;
    }

    async fn lost(&self) {
        self.events
            .emit(Event::Error {
                kind: ErrorKind::Internal,
                message: format!(
                    "session {} stopped after an internal error and takes no more messages",
                    self.session_id
                ),
                retryable: false,
                hint: Some("resume the session to continue".into()),
            })
            .await;
    }
}
