//! The engine: one actor task per session that owns the session's state, takes commands
//! and runs each user message as a turn in a child task with a child cancellation token.
//!
//! While a turn runs, the actor still answers `Approve`/`Deny` (by request id), `Cancel`
//! and `Shutdown`; `Pin`/`Unpin` reach the turn at its next step start; everything else
//! (further messages included) waits for the turn to end. Every command is logged as it
//! arrives.
//!
//! `SetBudget` is applied at once, even mid-turn: the running turn sees the new limits at
//! its next tool boundary (`budget.rs`). `SetModel` waits for the turn to end like other
//! commands, so it applies at once when idle and otherwise before the next user turn
//! (`model_switch.rs`). At shutdown, after the turn has ended, every process group the
//! session's commands left behind is terminated.
//!
//! Seams for later tasks: resume and fork (Task 26) build the `Session` from a log
//! instead of [`Session::init`]'s fresh state (`EngineBuilder::resume` is refused until
//! then); a resumed session starts on `record::session_model` and sets
//! `Context::reasoning_provider` to that model's provider. Side-model routing (Task 27)
//! replaces `routing::side_model_id` and the provider used by `Session::summarize`;
//! memory proposals (Task 28) plug in at the `ContextOp::Remember` arm of `dispatch.rs`
//! and fill `Session::pending_memory_index`, which the cache epoch already applies.

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tautly_context::Context;
use tautly_provider::{Catalog, Provider, Retrying};
use tautly_sandbox::{LocalSandbox, Policy, PolicyError, Sandbox, SandboxError};
use tautly_session::{HeaderInit, SessionWriter};
use tautly_tools::{ProcessGroups, ReadTracker, registry};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::approvals::{Approvals, Verdict};
use crate::attempts::AttemptLedger;
use crate::budget::{Budget, Budgets};
use crate::config::{Config, LoadedConfig};
use crate::evidence::{EvidenceTracker, TestMatcher};
use crate::ledger::{CostLedger, LEDGER_FILE};
use crate::model_switch::ModelSetup;
use crate::protocol::{BudgetScope, Command, ErrorKind, Event, FinishReason};
use crate::provider_setup::{ProviderFactory, provider_for, provider_label};
use crate::record::SessionRecord;
use crate::routing;
use crate::session::{EventSink, Log, LogFailure, Session};
use crate::turn::TurnControl;
use crate::workspace::Workspace;

/// Capacity of the event channel returned by [`EngineBuilder::build`].
pub const EVENT_CAPACITY: usize = 256;
/// Commands buffered before [`Engine::send`] waits.
const COMMAND_CAPACITY: usize = 64;
/// At shutdown, how long left-behind process groups get between SIGTERM and SIGKILL.
const GROUP_GRACE: Duration = Duration::from_millis(500);

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
    #[error("invalid [evidence] test pattern: {0}")]
    Evidence(PolicyError),
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
    /// Builds the provider for the model a `SetModel` names (on the blocking pool; it may
    /// read the keychain). Default: the adapter for that model's provider, with its key
    /// from the environment or the keychain. Wrapped in `Retrying` like `provider`.
    pub provider_factory: Option<ProviderFactory>,
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
    /// Where session logs and the cost ledger (`ledger.sqlite`) live. Default:
    /// [`tautly_session::paths::data_home`].
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
            provider_factory: None,
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
    ///
    /// Without a `provider`, this looks up the API key, which may read the OS keychain and
    /// block (on macOS it can wait for the user to allow access): call it from
    /// `tokio::task::spawn_blocking` (keeping the runtime entered) or pass a provider.
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
        let test_matcher =
            Arc::new(TestMatcher::new(&config.evidence).map_err(EngineError::Evidence)?);
        let mut catalog = Catalog::builtin();
        if let Some(overrides) = config.catalog_overrides.clone() {
            catalog.merge(overrides);
        }
        let catalog = Arc::new(catalog);
        let info = catalog.get(&config.model);
        let provider = match self.provider {
            Some(p) => p,
            None => provider_for(&config, &config.model, Arc::clone(&catalog))
                .map_err(|e| EngineError::Provider(e.to_string()))?,
        };
        let provider: Arc<dyn Provider> = Arc::new(Retrying::new(provider));
        let init = HeaderInit {
            model: Some(config.model.as_str().to_string()),
            config_hash: Some(config.hash()),
            ..HeaderInit::default()
        };
        let writer = SessionWriter::<SessionRecord>::create_in(&data_home, &root, init)?;
        let session_id = writer.session_id().to_string();
        let session_path = writer.path().to_path_buf();

        let (event_tx, event_rx) = mpsc::channel(EVENT_CAPACITY);
        let (command_tx, command_rx) = mpsc::channel(COMMAND_CAPACITY);
        let tools = registry(&info.caps);
        let specs = tools.iter().map(|t| t.spec(&info.caps)).collect();
        let context = Context {
            root: Some(root.clone()),
            reasoning_provider: Some(provider_label(&config.model)),
            ..Context::default()
        };
        let side_model = routing::side_model_id(&config, &catalog);
        let budgets = Arc::new(Mutex::new(Budgets::new(
            &config.budget,
            info.price.is_some(),
            config.model.as_str(),
        )));
        let groups = ProcessGroups::default();
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
            setup: ModelSetup::new(config.clone(), Arc::clone(&catalog), self.provider_factory),
            side_model,
            reasoning_retry: false,
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
            user_turn: 0,
            prefix_log: Default::default(),
            last_request_path: last_request_path(&session_path),
            index_error_reported: false,
            session_id: session_id.clone(),
            budgets: Arc::clone(&budgets),
            turn_timeout: config.turn_timeout_s.map(Duration::from_secs),
            cost_ledger: CostLedger::new(data_home.join(LEDGER_FILE)),
            cost_ledger_reported: false,
            groups: groups.clone(),
            lost_write_paths: Default::default(),
            evidence: EvidenceTracker::new(test_matcher),
        };
        let actor = Actor {
            log: session.log.clone(),
            events: session.events.clone(),
            approvals: Approvals::new(self.interactive),
            shutdown: CancellationToken::new(),
            session_id: session_id.clone(),
            budgets,
            groups: groups.clone(),
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
                groups,
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
    groups: ProcessGroups,
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

    /// The command channel, held weakly (the signal listener).
    pub(crate) fn weak_commands(&self) -> mpsc::WeakSender<Command> {
        self.commands.downgrade()
    }

    /// The session's process groups (killed by a signal escalation).
    pub(crate) fn process_groups(&self) -> &ProcessGroups {
        &self.groups
    }

    /// The session log file.
    pub fn session_path(&self) -> &Path {
        &self.session_path
    }

    /// The file holding the latest provider request in full (`prompt show --full`),
    /// next to the session log; overwritten at every step and not fsynced.
    pub fn last_request_path(&self) -> PathBuf {
        last_request_path(&self.session_path)
    }
}

/// `<session-id>.last-request.json` next to `<session-id>.jsonl`.
pub(crate) fn last_request_path(session_path: &Path) -> PathBuf {
    session_path.with_extension("last-request.json")
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
    budgets: Arc<Mutex<Budgets>>,
    groups: ProcessGroups,
}

/// The actor's task dropped without a shutdown (the runtime went away): no process group
/// may outlive the session. After a normal shutdown the registry is already empty.
impl Drop for Actor {
    fn drop(&mut self) {
        self.groups.kill_all_now();
    }
}

impl Actor {
    async fn run(
        self,
        mut session: Session,
        mut commands: mpsc::Receiver<Command>,
        config_warnings: Vec<String>,
        config_pins: Vec<String>,
    ) {
        self.events.try_emit(Event::SessionChanged {
            session_id: self.session_id.clone(),
        });
        session.workspace.start();
        for w in config_warnings {
            self.events.try_emit(Event::Warning { message: w });
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
                            None => self.lost(),
                        }
                    } else if let Some(s) = state.as_mut() {
                        self.idle_command(s, cmd).await;
                    } else {
                        self.lost();
                    }
                }
            }
            if stopping && running.is_none() {
                break;
            }
            tokio::select! {
                cmd = commands.recv(), if !stopping => {
                    let cmd = cmd.unwrap_or(Command::Shutdown);
                    self.with_log({
                        let rec = SessionRecord::Command(cmd.clone());
                        move |l| l.append(&rec)
                    })
                    .await;
                    let lost = state.is_none() && running.is_none();
                    match cmd {
                        Command::Shutdown => {
                            stopping = true;
                            self.approvals.deny_all("the session is shutting down");
                            self.shutdown.cancel();
                        }
                        // After a crashed turn every command gets the same answer.
                        _ if lost => self.lost(),
                        Command::Approve { request_id } => {
                            self.resolve(&request_id, Verdict::Approved);
                        }
                        Command::Deny { request_id, reason } => {
                            self.resolve(&request_id, Verdict::Denied { reason });
                        }
                        Command::Cancel => {
                            if let Some(r) = &running {
                                r.cancel.cancel();
                            }
                        }
                        Command::SetBudget { scope, max_usd, max_tokens } => {
                            self.set_budget(scope, max_usd, max_tokens);
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
        // The turn has ended (its own command was killed with it); what is left are groups
        // with background children, such as dev servers.
        self.groups.terminate_all(GROUP_GRACE).await;
        self.with_log(|l| l.checkpoint()).await;
    }

    /// `SetBudget`: replace one scope's limits. Clients are trusted (the project layer's
    /// tighten-only rule applies to configuration files only).
    fn set_budget(&self, scope: BudgetScope, max_usd: Option<f64>, max_tokens: Option<u64>) {
        if max_usd.is_some_and(|usd| !usd.is_finite() || usd < 0.0) {
            self.events.try_emit(Event::Warning {
                message: "SetBudget ignored: max_usd must be a number of at least 0".into(),
            });
            return;
        }
        self.budgets
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .set(
                scope,
                Budget {
                    max_usd,
                    max_tokens,
                },
            );
    }

    /// Run `f` on the log off the async workers; a first failure is shown.
    async fn with_log(&self, f: impl FnOnce(&Log) -> Result<(), LogFailure> + Send + 'static) {
        let log = self.log.clone();
        if let Ok(Err(failure)) = tokio::task::spawn_blocking(move || f(&log)).await {
            if let Some(ev) = failure.event() {
                self.events.try_emit(ev);
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

    fn resolve(&self, request_id: &str, verdict: Verdict) {
        if !self.approvals.resolve(request_id, verdict) {
            self.events.try_emit(Event::Warning {
                message: format!("no approval request `{request_id}` is pending"),
            });
        }
    }

    /// A command that runs between turns.
    async fn idle_command(&self, session: &mut Session, cmd: Command) {
        match cmd {
            Command::Pin { .. } | Command::Unpin { .. } => session.context_command(cmd).await,
            Command::Diff { turn } => session.diff(turn).await,
            Command::SetModel { model } => session.set_model(model).await,
            other => {
                let name = serde_json::to_value(&other)
                    .ok()
                    .and_then(|v| v["type"].as_str().map(str::to_string))
                    .unwrap_or_default();
                self.events.try_emit(Event::Error {
                    kind: ErrorKind::Internal,
                    message: format!("the `{name}` command is not supported yet"),
                    retryable: false,
                    hint: None,
                });
            }
        }
    }

    /// A turn task panicked: an engine bug. The state it held is gone, but the log is
    /// intact up to the crash (plus the panic's message), so the session can be resumed.
    async fn crashed(&self, e: tokio::task::JoinError) {
        let what = if e.is_panic() {
            let payload = e.into_panic();
            let text = payload
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "a panic without a message".to_string());
            format!("the turn panicked: {text}")
        } else {
            format!("the turn stopped: {e}")
        };
        let recs = [
            SessionRecord::EngineError {
                message: what.clone(),
            },
            SessionRecord::TurnEnd {
                reason: FinishReason::Error,
                evidence: None,
            },
        ];
        self.with_log(move |l| {
            recs.iter().try_for_each(|r| l.append(r))?;
            l.checkpoint()
        })
        .await;
        self.events
            .send_or_drop(Event::Error {
                kind: ErrorKind::Internal,
                message: format!(
                    "engine bug in session {}: {what}; the session log is intact",
                    self.session_id
                ),
                retryable: false,
                hint: Some("resume the session to continue".into()),
            })
            .await;
        self.events
            .send_or_drop(Event::TurnFinished {
                reason: FinishReason::Error,
                evidence: None,
            })
            .await;
    }

    fn lost(&self) {
        self.events.try_emit(Event::Error {
            kind: ErrorKind::Internal,
            message: format!(
                "session {} stopped after an internal error and takes no more commands",
                self.session_id
            ),
            retryable: false,
            hint: Some("resume the session to continue".into()),
        });
    }
}
