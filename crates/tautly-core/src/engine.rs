//! The engine: one actor task per session that owns the session's state, takes commands
//! and runs each user message as a turn in a child task with a child cancellation token.
//!
//! While a turn runs, the actor still answers `Approve`/`Deny` (by request id), `Cancel`
//! and `Shutdown`; `Pin`/`Unpin` reach the turn at its next step start; everything else
//! (further messages included) waits for the turn to end. Every command is logged as it
//! arrives (one that arrives while a `SetModel` builds its provider, once the build ends).
//!
//! MCP servers (`mcp.rs`) start in the background after session start; `Approve`/`Deny`
//! also answer repository servers' approval requests. At shutdown the servers are stopped
//! before the left-behind process groups.
//!
//! `SetBudget` is applied at once, even mid-turn: the running turn sees the new limits at
//! its next tool boundary (`budget.rs`). `SetModel` waits for the turn to end like other
//! commands, so it applies at once when idle and otherwise before the next user turn
//! (`model_switch.rs`); while its provider is built the actor keeps taking commands, and a
//! `Shutdown` abandons the switch. At shutdown, after the turn has ended, every process
//! group the session's commands left behind is terminated.
//!
//! A resumed session (`EngineBuilder::resume`) starts from its log, rebuilt by
//! [`resume`](crate::resume) in the actor before it takes commands (the model of its last
//! `ModelSet`, and the `ModelSet` and `ReasoningStripped` records replayed at their
//! positions); `Fork` moves the engine to a branch of its log, and `Compact`/`Uncompact`
//! run between turns (all queued behind a running turn). `Compact` sends a summary
//! request, so it runs as a task like a turn: `Cancel` and `Shutdown` stop it.
//!
//! Project memory (`memory.rs`): `ApproveMemory`/`RejectMemory` reach a running turn at its
//! next step start (like `Pin`) and apply at once when idle; `ConsolidateMemory` asks the
//! side model, so it runs as a task like `Compact`. At shutdown, after the turn has ended,
//! the side model writes the session's episode (bounded) before anything else stops.

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tautly_context::Context;
use tautly_mcp::ServerSpec;
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
use crate::checkpoint::{self, Checkpointer, GitCheckpoints, Skip, StartCheckpoint};
use crate::config::{Config, LoadedConfig};
use crate::evidence::{EvidenceTracker, TestMatcher};
use crate::ledger::{CostLedger, LEDGER_FILE};
use crate::mcp::{McpSession, McpSetup, NoPreExec, PreExecHook};
use crate::memory::{MemoryMode, MemoryState};
use crate::model_switch::ModelSetup;
use crate::protocol::{BudgetScope, Command, ErrorKind, Event, FinishReason};
use crate::provider_setup::{ProviderFactory, provider_for, provider_label};
use crate::record::SessionRecord;
use crate::resume;
use crate::routing::Side;
use crate::session::{EventSink, Log, LogFailure, Session};
use crate::turn::TurnControl;
use crate::workspace::Workspace;

/// Capacity of the event channel returned by [`EngineBuilder::build`].
pub const EVENT_CAPACITY: usize = 256;
/// Commands buffered before [`Engine::send`] waits.
const COMMAND_CAPACITY: usize = 64;
/// At shutdown, how long left-behind process groups get between SIGTERM and SIGKILL.
const GROUP_GRACE: Duration = Duration::from_millis(500);
/// Longest a shutdown waits for the cost ledger to write the rows queued for it.
const LEDGER_FLUSH: Duration = Duration::from_secs(1);

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
    #[error("cannot resume the session: {0}")]
    Resume(#[from] resume::OpenError),
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
    /// Serves the side model (spec 7.5) when it is not the main model's provider. Default:
    /// the main provider when the side model has the same provider, else an adapter built
    /// from the configuration (which looks up an API key and may block, like `provider`).
    /// Always wrapped in `Retrying`.
    pub side_provider: Option<Arc<dyn Provider>>,
    /// Builds the provider for the model a `SetModel` names (on the blocking pool; it may
    /// read the keychain). Default: the adapter for that model's provider, with its key
    /// from the environment or the keychain. Wrapped in `Retrying` like `provider`.
    pub provider_factory: Option<ProviderFactory>,
    /// Default: a `LocalSandbox` on the root (or the workspace's sandbox).
    pub sandbox: Option<Arc<dyn Sandbox>>,
    /// A session to resume: the id of one that ran in this work tree (the sandbox root).
    /// Its log is reopened here (locked, and checked); its context is rebuilt from it in
    /// the session's actor, after the first event, off the async workers (a turn it ended
    /// in gets its missing `TurnEnd`). AGENTS.md and the repo map are the logged ones.
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
    /// Start repository-declared MCP servers the user has not approved, for this session
    /// only; nothing is remembered (`--approve-mcp`, spec 3.4).
    pub approve_mcp: bool,
    /// MCP servers from the client (an editor over ACP); trusted like the user's own.
    pub client_mcp: Vec<(String, ServerSpec)>,
    /// Runs before every stdio MCP call. Default: [`NoPreExec`].
    pub pre_exec: Option<Arc<dyn PreExecHook>>,
}

impl EngineBuilder {
    pub fn new(config: Config, root: impl Into<PathBuf>) -> Self {
        EngineBuilder {
            config,
            root: root.into(),
            provider: None,
            side_provider: None,
            provider_factory: None,
            sandbox: None,
            resume: None,
            workspace: None,
            interactive: false,
            data_home: None,
            config_warnings: Vec::new(),
            approve_mcp: false,
            client_mcp: Vec::new(),
            pre_exec: None,
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
    /// AGENTS.md, cleaning the spill store and, for a resumed session, rebuilding its
    /// context from the log happen after the first event (`SessionChanged`), off this
    /// path; commands sent meanwhile wait.
    ///
    /// Without a `provider`, this looks up the API key, which may read the OS keychain and
    /// block (on macOS it can wait for the user to allow access): call it from
    /// `tokio::task::spawn_blocking` (keeping the runtime entered) or pass a provider.
    pub fn build(self) -> Result<(Engine, mpsc::Receiver<Event>), EngineError> {
        let runtime = tokio::runtime::Handle::try_current().map_err(|_| EngineError::NoRuntime)?;
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
        let (side, side_note) = Side::resolve(&config, &catalog, &provider, self.side_provider);
        let mut config_warnings = self.config_warnings;
        config_warnings.extend(side_note);
        let init = HeaderInit {
            model: Some(config.model.as_str().to_string()),
            config_hash: Some(config.hash()),
            ..HeaderInit::default()
        };
        let writer = match &self.resume {
            None => SessionWriter::<SessionRecord>::create_in(&data_home, &root, init.clone())?,
            Some(id) => resume::open(&data_home, &root, id)?,
        };
        let session_id = writer.session_id().to_string();
        let session_path = writer.path().to_path_buf();

        let (event_tx, event_rx) = mpsc::channel(EVENT_CAPACITY);
        let (command_tx, command_rx) = mpsc::channel(COMMAND_CAPACITY);
        // MCP tools join at the first user-turn start (`Session::refresh_mcp_tools`).
        let tools = registry(&info.caps, Vec::new());
        let specs = tools.iter().map(|t| t.spec(&info.caps)).collect();
        let mcp = McpSetup::new(&config, self.client_mcp, self.approve_mcp, &data_home);
        let context = Context {
            root: Some(root.clone()),
            reasoning_provider: Some(provider_label(&config.model)),
            ..Context::default()
        };
        let budgets = Arc::new(Mutex::new(Budgets::new(
            &config.budget,
            info.price.is_some(),
            config.model.as_str(),
        )));
        let groups = ProcessGroups::default();
        let checkpoints = config
            .checkpoints
            .enabled
            .then(|| {
                Checkpointer::new(
                    Arc::clone(&sandbox),
                    root.clone(),
                    session_id.clone(),
                    config.checkpoints,
                )
            })
            .flatten()
            .map(|cp| {
                // Each checkpoint's exclude rules are kept next to the project's session
                // logs (owner-only), shared by its sessions (resumes and forks read them).
                let cp = match session_path.parent() {
                    Some(dir) => cp.with_excludes_store(dir.join(checkpoint::EXCLUDES_DIR)),
                    None => cp,
                };
                GitCheckpoints::shared(cp.with_process_groups(groups.clone()))
            });
        // Checkpoints are on in a git work tree, yet none can be taken: `.git` is a
        // symbolic link (to a repository that may not be this one's), or no git to run.
        let start_checkpoint = if !config.checkpoints.enabled || checkpoints.is_some() {
            StartCheckpoint::Idle
        } else if checkpoint::dot_git_is_link(&root) {
            StartCheckpoint::Skipped(Skip::GitDirSymlink)
        } else if checkpoint::git_dir(&root).is_some() && checkpoint::git_on_path(&root).is_none() {
            StartCheckpoint::Skipped(Skip::NoGit)
        } else {
            StartCheckpoint::Idle
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
            setup: ModelSetup::new(config.clone(), Arc::clone(&catalog), self.provider_factory),
            reasoning_retry: false,
            model: config.model.clone(),
            info,
            side,
            tools,
            specs,
            max_turns: config.max_turns,
            compaction_threshold: config.compaction_threshold,
            startup: None,
            latest_compaction: None,
            pending_memory_index: None,
            memory: MemoryState::new(MemoryMode::new(config.approve_memory, self.interactive)),
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
            lost_write_probes: Default::default(),
            evidence: EvidenceTracker::new(test_matcher),
            mcp: None,
            pre_exec: self.pre_exec.unwrap_or_else(|| Arc::new(NoPreExec)),
            checkpoints,
            start_checkpoint,
            data_home,
            header_init: init,
        };
        let current = Arc::new(Mutex::new((session_id.clone(), session_path.clone())));
        let actor = Actor {
            log: session.log.clone(),
            events: session.events.clone(),
            approvals: Approvals::new(self.interactive),
            shutdown: CancellationToken::new(),
            session_id: session_id.clone(),
            current: Arc::clone(&current),
            budgets,
            groups: groups.clone(),
        };
        runtime.spawn(actor.run(
            session,
            command_rx,
            Start {
                config_warnings,
                config_pins: config.pins.clone(),
                resume: self.resume.is_some(),
                mcp,
            },
        ));
        Ok((
            Engine {
                commands: command_tx,
                session_id,
                session_path,
                current,
                groups,
            },
            event_rx,
        ))
    }
}

/// Delete checkpoint refs older than `[checkpoints] keep_days` (spec 7.11.11), in the
/// background after the session-start checkpoint, one git snapshot at a time.
fn prune_checkpoints(session: &Session) {
    let Some(shared) = session.checkpoints.clone() else {
        return;
    };
    let sandbox = Arc::clone(&session.sandbox);
    tokio::spawn(async move {
        let st = shared.lock().await;
        let keep_days = st.cp.config().keep_days;
        let root = sandbox.root().to_path_buf();
        let excludes = st.cp.excludes_dir().map(Path::to_path_buf);
        // Best effort: what is not pruned now is at the next start.
        let _ = checkpoint::prune(sandbox.as_ref(), &root, keep_days, excludes.as_deref()).await;
        drop(st);
    });
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
    /// The session the engine is on now (a fork moves it to the branch).
    current: Arc<Mutex<(String, PathBuf)>>,
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

    /// The session the engine was built on (created or resumed); see
    /// [`current_session`](Self::current_session) after a fork.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// The id and log of the session the engine is on now: after a `Fork`, the branch
    /// (also announced by `SessionChanged`).
    pub fn current_session(&self) -> (String, PathBuf) {
        self.current
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The command channel, held weakly (the signal listener).
    pub(crate) fn weak_commands(&self) -> mpsc::WeakSender<Command> {
        self.commands.downgrade()
    }

    /// The session's process groups (killed by a signal escalation).
    pub(crate) fn process_groups(&self) -> &ProcessGroups {
        &self.groups
    }

    /// The log of the session the engine was built on.
    pub fn session_path(&self) -> &Path {
        &self.session_path
    }

    /// The file holding the latest provider request in full (`prompt show --full`),
    /// next to the current session's log; overwritten at every step and not fsynced.
    pub fn last_request_path(&self) -> PathBuf {
        last_request_path(&self.current_session().1)
    }
}

/// `<session-id>.last-request.json` next to `<session-id>.jsonl`.
pub(crate) fn last_request_path(session_path: &Path) -> PathBuf {
    session_path.with_extension("last-request.json")
}

/// What runs as a task, with its own cancellation: a user message's turn, a manual
/// compaction (a summary request) or a memory consolidation (a side-model request).
enum Job {
    Turn(String),
    Compact,
    Consolidate,
}

/// The task in flight (a turn or a compaction). It hands back the session and the inbox,
/// whose leftovers (sent after the turn's last step start) are applied once it ends.
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
    /// Shared with the [`Engine`] handle.
    current: Arc<Mutex<(String, PathBuf)>>,
    budgets: Arc<Mutex<Budgets>>,
    groups: ProcessGroups,
}

/// Collect the commands that arrive into `early` until a `Shutdown` (or the client going
/// away, which means the same) is among them. Cancel-safe: a received command is stored
/// before the next await, so dropping this future loses none. The caller handles `early`
/// (logging each) afterwards, so their `Command` records follow what the interrupted
/// command logged.
async fn until_shutdown(commands: &mut mpsc::Receiver<Command>, early: &mut Vec<Command>) {
    loop {
        let cmd = commands.recv().await.unwrap_or(Command::Shutdown);
        let stop = cmd == Command::Shutdown;
        early.push(cmd);
        if stop {
            return;
        }
    }
}

/// What the actor does at session start.
struct Start {
    config_warnings: Vec<String>,
    config_pins: Vec<String>,
    /// The session is resumed: its context is rebuilt from the log first.
    resume: bool,
    mcp: McpSetup,
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
        mut self,
        mut session: Session,
        mut commands: mpsc::Receiver<Command>,
        start: Start,
    ) {
        self.events.try_emit(Event::SessionChanged {
            session_id: self.session_id.clone(),
        });
        session.workspace.start();
        for w in start.config_warnings {
            self.events.try_emit(Event::Warning { message: w });
        }
        let started = if start.resume {
            session.resume(&start.config_pins).await
        } else {
            session.init(&start.config_pins).await;
            Ok(())
        };
        // A log that cannot be rebuilt leaves nothing to run: every command is refused.
        let mut state = match started {
            Ok(()) => {
                session.begin_checkpoints().await;
                prune_checkpoints(&session);
                session.mcp = McpSession::start(start.mcp, &session, self.approvals.clone()).await;
                Some(session)
            }
            Err(why) => {
                self.events
                    .send_or_drop(Event::Error {
                        kind: ErrorKind::Internal,
                        message: format!("cannot resume the session: {why}"),
                        retryable: false,
                        hint: Some("the session log may be damaged".into()),
                    })
                    .await;
                None
            }
        };
        let mut running: Option<Running> = None;
        let mut queue: VecDeque<Command> = VecDeque::new();
        let mut stopping = false;
        loop {
            if running.is_none() && !stopping {
                while let Some(cmd) = queue.pop_front() {
                    let job = match cmd {
                        Command::SendMessage { text } => Job::Turn(text),
                        Command::Compact => Job::Compact,
                        Command::ConsolidateMemory => Job::Consolidate,
                        Command::SetModel { model } => {
                            match state.as_mut() {
                                Some(s) => {
                                    // The provider build may wait on a keychain prompt: keep
                                    // taking commands meanwhile, and let a `Shutdown` abandon
                                    // the switch.
                                    let mut early = Vec::new();
                                    s.set_model(model, until_shutdown(&mut commands, &mut early))
                                        .await;
                                    for cmd in early {
                                        self.received(cmd, false, None, &mut queue, &mut stopping)
                                            .await;
                                    }
                                    if stopping {
                                        break;
                                    }
                                }
                                None => self.lost(),
                            }
                            continue;
                        }
                        other => {
                            match state.as_mut() {
                                Some(s) => self.idle_command(s, other).await,
                                None => self.lost(),
                            }
                            continue;
                        }
                    };
                    match state.take() {
                        Some(s) => {
                            running = Some(self.spawn_job(s, job));
                            break;
                        }
                        None => self.lost(),
                    }
                }
            }
            if stopping && running.is_none() {
                break;
            }
            tokio::select! {
                cmd = commands.recv(), if !stopping => {
                    let cmd = cmd.unwrap_or(Command::Shutdown);
                    let lost = state.is_none() && running.is_none();
                    self.received(cmd, lost, running.as_ref(), &mut queue, &mut stopping)
                        .await;
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
        if let Some(s) = state.as_mut() {
            s.abandon_start_checkpoint();
        }
        // MCP servers end with their process groups. (A session lost to a crashed turn
        // dropped its servers, which kills their groups.)
        if let Some(mcp) = state.as_ref().and_then(|s| s.mcp.as_ref()) {
            mcp.shutdown().await;
        }
        // The turn has ended (its own command was killed with it); what is left are groups
        // with background children, such as dev servers.
        self.groups.terminate_all(GROUP_GRACE).await;
        // The session's episode, by the side model (spec 7.4), which needs neither: last,
        // so nothing the session started outlives it while the side model writes.
        if let Some(s) = state.as_mut() {
            s.end_episode().await;
        }
        self.with_log(|l| l.checkpoint()).await;
        // The episode's usage included: a client that exits once the event stream closes
        // loses no row, unless the ledger is stuck.
        if let Some(s) = state.as_ref() {
            s.cost_ledger.flush(LEDGER_FLUSH).await;
        }
    }

    /// A command from the channel: logged, then handled at once (shutdown, approvals,
    /// cancel, budgets, pins for the running turn) or queued for when no turn runs.
    async fn received(
        &self,
        cmd: Command,
        lost: bool,
        running: Option<&Running>,
        queue: &mut VecDeque<Command>,
        stopping: &mut bool,
    ) {
        self.with_log({
            let rec = SessionRecord::Command(cmd.clone());
            move |l| l.append(&rec)
        })
        .await;
        match cmd {
            Command::Shutdown => {
                *stopping = true;
                self.approvals.deny_all("the session is shutting down");
                self.shutdown.cancel();
            }
            // After a crashed turn every command gets the same answer.
            _ if lost => self.lost(),
            Command::Unknown => self.events.try_emit(Event::Error {
                kind: ErrorKind::Internal,
                message: "this engine does not know this command (sent by a newer client?); \
                          it was ignored"
                    .into(),
                retryable: false,
                hint: None,
            }),
            Command::Approve { request_id } => {
                self.resolve(&request_id, Verdict::Approved);
            }
            Command::Deny { request_id, reason } => {
                self.resolve(&request_id, Verdict::Denied { reason });
            }
            Command::Cancel => {
                if let Some(r) = running {
                    r.cancel.cancel();
                }
            }
            Command::SetBudget {
                scope,
                max_usd,
                max_tokens,
            } => {
                self.set_budget(scope, max_usd, max_tokens);
            }
            cmd @ (Command::Pin { .. }
            | Command::Unpin { .. }
            | Command::ApproveMemory { .. }
            | Command::RejectMemory { .. })
                if running.is_some() =>
            {
                if let Some(r) = running {
                    let _ = r.inbox.send(cmd);
                }
            }
            other => queue.push_back(other),
        }
    }

    /// `SetBudget`: replace one scope's limits. Clients are trusted (the project layer's
    /// tighten-only rule applies to configuration files only).
    fn set_budget(&self, scope: BudgetScope, max_usd: Option<f64>, max_tokens: Option<u64>) {
        if scope == BudgetScope::Unknown {
            self.events.try_emit(Event::Warning {
                message: "SetBudget ignored: unknown scope (sent by a newer client?)".into(),
            });
            return;
        }
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
        if let Ok(Err(failure)) = tokio::task::spawn_blocking(move || f(&log)).await
            && let Some(ev) = failure.event()
        {
            self.events.try_emit(ev);
        }
    }

    fn spawn_job(&self, mut session: Session, job: Job) -> Running {
        let cancel = self.shutdown.child_token();
        let (inbox_tx, inbox_rx) = mpsc::unbounded_channel();
        let mut ctl = TurnControl {
            cancel: cancel.clone(),
            inbox: inbox_rx,
            approvals: self.approvals.clone(),
        };
        let handle = tokio::spawn(async move {
            match job {
                Job::Turn(text) => session.run_turn(text, &mut ctl).await,
                Job::Compact => session.compact_command(&ctl).await,
                Job::Consolidate => session.consolidate_command(&ctl).await,
            }
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
    async fn idle_command(&mut self, session: &mut Session, cmd: Command) {
        // A fork or undo reads the log, which must have the session-start checkpoint.
        session.settle_start_checkpoint().await;
        match cmd {
            Command::Pin { .. }
            | Command::Unpin { .. }
            | Command::ApproveMemory { .. }
            | Command::RejectMemory { .. } => session.context_command(cmd).await,
            Command::Diff { turn } => session.diff(turn).await,
            Command::Fork { at, label } => match session.fork(at, label).await {
                Ok((id, path)) => {
                    self.session_id.clone_from(&id);
                    *self.current.lock().unwrap_or_else(PoisonError::into_inner) =
                        (id.clone(), path);
                    self.events
                        .send_or_drop(Event::SessionChanged { session_id: id })
                        .await;
                }
                Err(why) => {
                    self.events
                        .send_or_drop(Event::Error {
                            kind: ErrorKind::Internal,
                            message: format!("fork: {why}"),
                            retryable: false,
                            hint: None,
                        })
                        .await;
                }
            },
            Command::Uncompact => session.uncompact_command().await,
            Command::Undo { turn, dry_run } => session.undo_command(turn, dry_run).await,
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
