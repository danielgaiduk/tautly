//! The engine: one actor task per session that owns the session's state, takes commands
//! and runs each user message as a turn in a child task with a child cancellation token.
//!
//! While a turn runs, the actor still answers `Approve`/`Deny` (by request id), `Cancel`
//! and `Shutdown`; `Pin`/`Unpin` reach the turn at its next step start; everything else
//! (further messages included) waits for the turn to end. Every command is logged as it
//! arrives (one that arrives while a `SetModel` builds its provider, once the build ends;
//! one other than `Cancel`, `Shutdown`, `Approve` and `Deny` that arrives while a `Fork`,
//! `Rewind` or `Switch` runs, once it ends, so it lands in the log it applies to).
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
//! [`resume`] in the actor before it takes commands (the model of its last
//! `ModelSet`, and the `ModelSet` and `ReasoningStripped` records replayed at their
//! positions); `Fork` moves the engine to a branch of its log, and `Compact`/`Uncompact`
//! run between turns (all queued behind a running turn). `Compact` sends a summary
//! request, `Undo` runs git steps and `Fork` copies and rebuilds the log, so each runs as
//! a task like a turn: the actor keeps reading `Cancel` and `Shutdown`, which stop a
//! compaction, and an undo or fork still waiting for the session-start checkpoint (an
//! undo that has started writing runs to its end). Commands answered in the actor wait a
//! bounded time for a client that is behind, so their answers are not dropped.
//!
//! Project memory (`memory.rs`): `ApproveMemory`/`RejectMemory` reach a running turn at its
//! next step start (like `Pin`) and apply at once when idle; `ConsolidateMemory` asks the
//! side model, so it runs as a task like `Compact`. At shutdown, after the turn has ended,
//! the side model writes the session's episode (bounded) before anything else stops.

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tautly_context::Context;
use tautly_mcp::ServerSpec;
use tautly_provider::{ModelId, Provider, Retrying};
use tautly_sandbox::{LocalSandbox, Policy, PolicyError, Sandbox, SandboxError};
use tautly_session::{HeaderInit, Origin, SessionWriter};
use tautly_tools::{ProcessGroups, ReadTracker, registry};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::approvals::{ApprovalFor, Approvals, Verdict};
use crate::attempts::AttemptLedger;
use crate::auth::{KeyringStore, SecretStore};
use crate::budget::{Budget, Budgets};
use crate::checkpoint::{self, Checkpointer, GitCheckpoints, Skip, StartCheckpoint};
use crate::config::{Config, LoadedConfig};
use crate::evidence::{EvidenceTracker, TestMatcher};
use crate::lease::Lease;
use crate::ledger::{CostLedger, LEDGER_FILE};
use crate::mcp::{McpSession, McpSetup, NoPreExec, PreExecHook};
use crate::memory::{MemoryMode, MemoryState};
use crate::model_switch::ModelSetup;
use crate::protocol::{BudgetScope, Command, ErrorKind, Event, FinishReason};
use crate::provider_setup::{ProviderFactory, provider_for_with, provider_label};
use crate::reasoning::ReasoningLevel;
use crate::record::SessionRecord;
use crate::resume;
use crate::routing::Side;
use crate::session::{EventSink, Log, LogFailure, Session};
use crate::turn::TurnControl;
use crate::workspace::{LiveSession, Workspace};

/// Capacity of the event channel returned by [`EngineBuilder::build`].
pub const EVENT_CAPACITY: usize = 256;
/// Commands buffered before [`Engine::send`] waits.
const COMMAND_CAPACITY: usize = 64;
/// At shutdown, how long left-behind process groups get between SIGTERM and SIGKILL.
const GROUP_GRACE: Duration = Duration::from_millis(500);
/// Default [`EngineBuilder::lease_wait`].
const LEASE_WAIT: Duration = Duration::from_secs(30);
/// Longest a shutdown waits for the cost ledger to write the rows queued for it.
const LEDGER_FLUSH: Duration = Duration::from_secs(1);

/// Session actors of this process that have not finished (their shutdown included).
static SESSIONS_RUNNING: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Counts one session actor in [`SESSIONS_RUNNING`] while it lives.
struct SessionRunning;

impl SessionRunning {
    fn new() -> SessionRunning {
        SESSIONS_RUNNING.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        SessionRunning
    }
}

impl Drop for SessionRunning {
    fn drop(&mut self) {
        SESSIONS_RUNNING.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Whether a session of this process is still running or shutting down: a client about
/// to exit gives it a moment to finish, and otherwise need not wait for anything.
pub fn sessions_running() -> bool {
    SESSIONS_RUNNING.load(std::sync::atomic::Ordering::SeqCst) > 0
}

/// Why the engine could not be built or reached.
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
    #[error("cannot lock the working tree (the working-tree lease): {0}")]
    Lease(std::io::Error),
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
    /// Where API keys are looked up after the environment, for every provider the engine
    /// builds itself (main, side model, `SetModel`). Default: the OS keychain.
    pub secret_store: Option<Arc<dyn SecretStore>>,
    /// How long the session waits at start for the working-tree lease while a rewind or
    /// switch elsewhere holds it (spec 7.12.1); then it fails to start. Default 30 s.
    pub lease_wait: Duration,
    /// Who started a new session (its header's `origin`); `tautly eval` runs mark theirs
    /// `eval`, with the header tag `eval`. Default `user`.
    pub origin: Origin,
    /// How long the first request waits for the repo map and memory index, built in the
    /// background at start, before it goes without them. Default 300 ms; tests that check
    /// what the first request carries wait longer, so a loaded machine cannot change it.
    pub startup_wait: Duration,
}

impl EngineBuilder {
    /// A builder for a session on `root` with `config` and default parts.
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
            secret_store: None,
            lease_wait: LEASE_WAIT,
            origin: Origin::User,
            startup_wait: crate::session::STARTUP_WAIT,
        }
    }

    /// [`EngineBuilder::new`] with a loaded configuration, whose warnings the session reports.
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
        let catalog = Arc::new(config.effective_catalog());
        let info = catalog.get(&config.model);
        let store: Arc<dyn SecretStore> =
            self.secret_store.unwrap_or_else(|| Arc::new(KeyringStore));
        let env = |name: &str| std::env::var_os(name);
        let provider = match self.provider {
            Some(p) => p,
            None => provider_for_with(&config, &config.model, Arc::clone(&catalog), &env, &*store)
                .map_err(|e| EngineError::Provider(e.to_string()))?,
        };
        let provider: Arc<dyn Provider> = Arc::new(Retrying::new(provider));
        let (side, side_note) =
            Side::resolve_with(&config, &catalog, &provider, self.side_provider, |id| {
                provider_for_with(&config, id, Arc::clone(&catalog), &env, &*store)
            });
        let provider_factory = self.provider_factory.unwrap_or_else(|| {
            let (config, catalog, store) =
                (config.clone(), Arc::clone(&catalog), Arc::clone(&store));
            Arc::new(move |model: &ModelId| {
                provider_for_with(
                    &config,
                    model,
                    Arc::clone(&catalog),
                    &|name| std::env::var_os(name),
                    &*store,
                )
            })
        });
        let mut config_warnings = self.config_warnings;
        config_warnings.extend(side_note);
        // Held for the engine's life; rewind and switch take it exclusively.
        // Not waited for here (this may run on an async worker): while a rewind elsewhere
        // holds it exclusively, the actor takes it before its first command.
        // An engine never runs without it: a lock file that cannot be opened fails the build.
        let (lease, lease_pending) = match Lease::try_shared(&data_home, &root) {
            Ok(Some(lease)) => (Some(lease.for_workspace(&workspace)), false),
            Ok(None) => (None, true),
            Err(e) => return Err(EngineError::Lease(e)),
        };
        let init = HeaderInit {
            model: Some(config.model.as_str().to_string()),
            config_hash: Some(config.hash()),
            origin: self.origin,
            tags: match self.origin {
                Origin::Eval => vec!["eval".to_string()],
                _ => Vec::new(),
            },
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
        let tools = registry(Vec::new());
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
        let cost_ledger = Arc::new(CostLedger::new(data_home.join(LEDGER_FILE)));
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
                let cp = cp.with_env_remove(crate::mcp::provider_key_vars(&config));
                GitCheckpoints::shared(cp.with_process_groups(groups.clone()))
            });
        // Checkpoints are on in a git work tree, yet none can be taken: `.git` is a
        // symbolic link (to a repository that may not be this one's), names a repository
        // that does not name it back, or no git to run.
        let start_checkpoint = if !config.checkpoints.enabled || checkpoints.is_some() {
            StartCheckpoint::Idle
        } else if checkpoint::dot_git_is_link(&root) {
            StartCheckpoint::Skipped(Skip::GitDirSymlink)
        } else if checkpoint::dot_git_unverified(&root) {
            StartCheckpoint::Skipped(Skip::GitDirUnverified)
        } else if checkpoint::git_dir(&root).is_some() && checkpoint::git_on_path(&root).is_none() {
            StartCheckpoint::Skipped(Skip::NoGit)
        } else {
            StartCheckpoint::Idle
        };
        let session = Session {
            context,
            reads: Arc::new(ReadTracker::default()),
            policy,
            ledger: AttemptLedger::default(),
            log: Log::new(writer),
            events: EventSink::new(event_tx),
            sandbox,
            workspace,
            provider,
            setup: ModelSetup::new(config.clone(), Arc::clone(&catalog), Some(provider_factory)),
            reasoning_retry: false,
            sent_reasoning: None,
            model: config.model.clone(),
            info,
            side,
            tools,
            specs,
            max_turns: config.max_turns,
            compaction_threshold: config.compaction_threshold,
            startup: None,
            startup_wait: self.startup_wait,
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
            reasoning: Arc::new(Mutex::new(config.reasoning)),
            turn_timeout: config.turn_timeout_s.map(Duration::from_secs),
            cost_ledger: Arc::clone(&cost_ledger),
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
            receipt: None,
            gc_elided: Vec::new(),
            lease,
            end_recovery: None,
        };
        let current = Arc::new(Mutex::new((session_id.clone(), session_path.clone())));
        let reasoning = Arc::clone(&session.reasoning);
        let actor = Actor {
            _live: session.workspace.enter(),
            log: session.log.clone(),
            events: session.events.clone(),
            approvals: Approvals::new(self.interactive),
            shutdown: CancellationToken::new(),
            session_id: session_id.clone(),
            current: Arc::clone(&current),
            budgets,
            reasoning: Arc::clone(&reasoning),
            groups: groups.clone(),
            cost_ledger: Arc::clone(&session.cost_ledger),
        };
        let running = SessionRunning::new();
        let run = actor.run(
            session,
            command_rx,
            Start {
                config_warnings,
                config_pins: config.pins.clone(),
                resume: self.resume.is_some(),
                mcp,
                lease_pending,
                lease_wait: self.lease_wait,
            },
        );
        runtime.spawn(async move {
            let _running = running;
            run.await
        });
        Ok((
            Engine {
                commands: command_tx,
                session_id,
                session_path,
                current,
                groups,
                reasoning,
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

/// The root a session started in `start` runs on, as [`EngineBuilder::build`] picks it:
/// the git top level above `start` (canonical), else `start` itself. Sessions on the same
/// root share one [`Workspace`] (spec 3.4).
pub fn session_root(start: &Path) -> Result<PathBuf, EngineError> {
    Ok(sandbox_root(start)?)
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
    /// Shared with the session (`SetReasoning`).
    reasoning: Arc<Mutex<ReasoningLevel>>,
}

impl Engine {
    /// The session's reasoning level now (spec 4.7): the configured one until a
    /// `SetReasoning` is applied.
    pub fn reasoning(&self) -> ReasoningLevel {
        *crate::lock(&self.reasoning)
    }

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
        crate::lock(&self.current).clone()
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

/// What runs as a task, with its own cancellation, while the actor goes on reading
/// commands (`Cancel`, `Shutdown`): a user message's turn, a manual compaction (a summary
/// request), a memory consolidation (a side-model request), an undo (git steps) or a fork
/// (a log copy and rebuild, and maybe a provider build). An undo and a fork first wait for
/// the session-start checkpoint, since they read the log.
enum Job {
    Turn(String),
    Compact,
    Consolidate,
    Undo {
        turn: Option<u32>,
        dry_run: bool,
    },
    Fork {
        at: Option<u32>,
        label: Option<String>,
    },
    Rewind {
        at: u32,
        label: Option<String>,
        dry_run: bool,
        overwrite: Vec<String>,
    },
    Switch {
        session_id: String,
        dry_run: bool,
        overwrite: Vec<String>,
    },
    Replay {
        session_id: Option<String>,
        speed: f32,
        from_step: Option<u32>,
    },
}

/// The task in flight (a turn or a compaction). It hands back the session and the inbox,
/// whose leftovers (sent after the turn's last step start) are applied once it ends.
struct Running {
    handle: JoinHandle<(Session, mpsc::Receiver<Command>)>,
    cancel: CancellationToken,
    /// Holds at most [`COMMAND_CAPACITY`]; past that a command is refused.
    inbox: mpsc::Sender<Command>,
    /// The command that started it: a crash still sends the answer its client waits for
    /// (a user message's turn is ended).
    cmd: Command,
    /// A fork: commands other than [`Actor::urgent`] ones wait for its end.
    fork: bool,
}

struct Actor {
    /// Counts the session on its workspace while the actor lives (dropped first).
    _live: LiveSession,
    log: Log,
    events: EventSink,
    approvals: Approvals,
    shutdown: CancellationToken,
    session_id: String,
    /// Shared with the [`Engine`] handle.
    current: Arc<Mutex<(String, PathBuf)>>,
    budgets: Arc<Mutex<Budgets>>,
    /// Shared with the session (`SetReasoning`).
    reasoning: Arc<Mutex<ReasoningLevel>>,
    groups: ProcessGroups,
    /// The session's cost ledger, flushed at shutdown.
    cost_ledger: Arc<CostLedger>,
}

/// Collect the commands that arrive into `early` until a `Shutdown` (or the client going
/// away, which means the same) is among them. Cancel-safe: a received command is stored
/// (or refused, without waiting) before the next await, so dropping this future loses
/// none. Past [`COMMAND_CAPACITY`] a command other than `Shutdown` is refused, unlogged,
/// through `events`. The caller handles `early` (logging each) afterwards, so their
/// `Command` records follow what the interrupted command logged.
async fn until_shutdown(
    commands: &mut mpsc::Receiver<Command>,
    early: &mut Vec<Command>,
    events: &EventSink,
    session_id: &str,
) {
    loop {
        let cmd = commands.recv().await.unwrap_or(Command::Shutdown);
        let stop = cmd == Command::Shutdown;
        if !stop && early.len() >= COMMAND_CAPACITY {
            for ev in refusal(&cmd, &too_many(), None, session_id) {
                events.try_emit(ev);
            }
            continue;
        }
        early.push(cmd);
        if stop {
            return;
        }
    }
}

fn too_many() -> String {
    format!("too many commands are queued (more than {COMMAND_CAPACITY}); this one was not run")
}

/// The events that end a command the engine does not run (`why`): an `Error` (for a
/// event its client waits for, if it has one. The end event echoes the command's correlation
/// id, and so does the `Error` when it is the answer ([`Command::error_answers`]).
/// event its client waits for, if it has one. Both echo the command's correlation id.
fn refusal(cmd: &Command, why: &str, hint: Option<&str>, session_id: &str) -> Vec<Event> {
    let id = cmd.id().map(str::to_string);
    let (kind, message) = match cmd {
        Command::SetModel { model, .. } => (
            ErrorKind::ProviderFatal,
            format!("cannot switch to {model}: {why}"),
        ),
        _ => (ErrorKind::Internal, why.to_string()),
    };
    let mut out = vec![Event::Error {
        id: if cmd.error_answers() { id } else { None },
        kind,
        message,
        retryable: false,
        hint: hint.map(str::to_string),
    }];
    out.extend(failed_answer(cmd, session_id));
    out
}

/// The end event a client waits for after a command that did not run (or whose job
/// crashed): nothing was applied. None for commands whose `Error` is their answer.
fn failed_answer(cmd: &Command, session_id: &str) -> Option<Event> {
    let id = cmd.id().map(str::to_string);
    Some(match cmd {
        Command::SendMessage { .. } => Event::TurnFinished {
            id,
            reason: FinishReason::Error,
            evidence: None,
        },
        Command::Replay {
            session_id: replayed,
            ..
        } => Event::ReplayFinished {
            id,
            session_id: replayed.clone().unwrap_or_else(|| session_id.to_string()),
            events: 0,
            cancelled: false,
        },
        Command::Compact { .. } => Event::CompactFinished {
            id,
            applied: false,
            tokens_freed: 0,
        },
        Command::ConsolidateMemory { .. } => Event::ConsolidateFinished {
            id,
            applied: 0,
            failed: 0,
        },
        _ => return None,
    })
}

/// What the actor does at session start.
struct Start {
    config_warnings: Vec<String>,
    config_pins: Vec<String>,
    /// The session is resumed: its context is rebuilt from the log first.
    resume: bool,
    mcp: McpSetup,
    /// The working-tree lease was held exclusively elsewhere at build: taken (waiting up
    /// to `lease_wait`, off the async workers) before anything else; else the session does
    /// not start.
    lease_pending: bool,
    lease_wait: Duration,
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
            id: None,
            session_id: self.session_id.clone(),
        });
        session.workspace.start();
        // Commands sent while a resumed session sets up its model (a keychain prompt may
        // hold it), or while the lease is waited for, are kept; a `Shutdown` among them
        // abandons that.
        let mut early = Vec::new();
        // The lease first: nothing runs on the working tree without it. Tried in short
        // slices (each off the async workers), so a `Shutdown` (or SIGTERM, which sends
        // one) stops the wait at once.
        let mut unleased = None;
        if start.lease_pending {
            let deadline = tokio::time::Instant::now() + start.lease_wait;
            let stop = until_shutdown(&mut commands, &mut early, &self.events, &self.session_id);
            tokio::pin!(stop);
            loop {
                let (home, root) = (session.data_home.clone(), session.root().to_path_buf());
                let tried = tokio::task::spawn_blocking(move || Lease::try_shared(&home, &root))
                    .await
                    .map_err(std::io::Error::other)
                    .and_then(|r| r);
                match tried {
                    Ok(Some(lease)) => {
                        session.lease = Some(lease.for_workspace(&session.workspace));
                        break;
                    }
                    Ok(None) => {}
                    Err(e) => {
                        unleased = Some(e.to_string());
                        break;
                    }
                }
                if tokio::time::Instant::now() >= deadline {
                    unleased = Some(format!(
                        "a rewind or switch elsewhere held the working tree for over {}s",
                        start.lease_wait.as_secs()
                    ));
                    break;
                }
                tokio::select! {
                    () = &mut stop => {
                        unleased = Some("the session was shut down while it waited".into());
                        break;
                    }
                    () = tokio::time::sleep(Duration::from_millis(50)) => {}
                }
            }
        }
        for w in start.config_warnings {
            self.events.try_emit(Event::Warning { message: w });
        }
        let started = if let Some(why) = unleased {
            Err(format!(
                "the working-tree lease could not be had ({why}); this session does not start"
            ))
        } else if start.resume {
            session
                .resume(
                    &start.config_pins,
                    until_shutdown(&mut commands, &mut early, &self.events, &self.session_id),
                )
                .await
        } else {
            session.init(&start.config_pins).await;
            Ok(())
        };
        // A log that cannot be rebuilt leaves nothing to run: every command is refused.
        let mut state = match started {
            Ok(()) => Some(session),
            Err(why) => {
                let unleased = session.lease.is_none();
                self.events
                    .send_or_drop(Event::Error {
                        id: None,
                        kind: ErrorKind::Internal,
                        message: if unleased {
                            format!("cannot start the session: {why}")
                        } else {
                            format!("cannot resume the session: {why}")
                        },
                        retryable: false,
                        hint: Some(if unleased {
                            "start it again once the other rewind or switch has finished".into()
                        } else {
                            "the session log may be damaged".into()
                        }),
                    })
                    .await;
                None
            }
        };
        let mut running: Option<Running> = None;
        let mut queue: VecDeque<Command> = VecDeque::new();
        // Commands that arrived while a fork ran, for after it.
        let mut held: Vec<Command> = Vec::new();
        let mut stopping = false;
        // A replay sent during start-up is refused like one sent while a turn runs.
        for cmd in early {
            let lost = state.is_none();
            self.received(cmd, lost, None, true, &mut queue, &mut stopping)
                .await;
        }
        // Nothing more starts for a session that is already shutting down.
        if let Some(s) = state.as_mut().filter(|_| !stopping) {
            s.begin_checkpoints(start.resume).await;
            prune_checkpoints(s);
            s.mcp = McpSession::start(start.mcp, s, self.approvals.clone()).await;
        }
        loop {
            if running.is_none() && !stopping {
                while let Some(cmd) = queue.pop_front() {
                    let Some(s) = state.as_mut() else {
                        self.refuse_lost(&cmd).await;
                        continue;
                    };
                    let job = match cmd.clone() {
                        Command::SendMessage { id: _, text } => Job::Turn(text),
                        Command::Compact { .. } => Job::Compact,
                        Command::ConsolidateMemory { .. } => Job::Consolidate,
                        Command::Undo { turn, dry_run, .. } => Job::Undo { turn, dry_run },
                        Command::Fork { at, label, .. } => Job::Fork { at, label },
                        Command::Rewind {
                            at,
                            label,
                            dry_run,
                            overwrite,
                            ..
                        } => Job::Rewind {
                            at,
                            label,
                            dry_run,
                            overwrite,
                        },
                        Command::Switch {
                            session_id,
                            dry_run,
                            overwrite,
                            ..
                        } => Job::Switch {
                            session_id,
                            dry_run,
                            overwrite,
                        },
                        Command::Replay {
                            session_id,
                            speed,
                            from_step,
                            ..
                        } => Job::Replay {
                            session_id,
                            speed,
                            from_step,
                        },
                        Command::SetModel { model, .. } => {
                            // The provider build may wait on a keychain prompt: keep taking
                            // commands meanwhile (a replay among them is refused), and let a
                            // `Shutdown` abandon the switch.
                            let mut early = Vec::new();
                            let unbound = s.events.clone();
                            s.events = unbound.replying().tagged(&cmd);
                            let stop = until_shutdown(
                                &mut commands,
                                &mut early,
                                &self.events,
                                &self.session_id,
                            );
                            s.set_model(model, stop).await;
                            s.events = unbound;
                            for cmd in early {
                                self.received(cmd, false, None, true, &mut queue, &mut stopping)
                                    .await;
                            }
                            if stopping {
                                break;
                            }
                            continue;
                        }
                        other => {
                            self.idle_command(s, other).await;
                            continue;
                        }
                    };
                    let s = state.take().expect("checked above");
                    running = Some(self.spawn_job(s, job, cmd));
                    break;
                }
            }
            if stopping && running.is_none() {
                if !queue.is_empty() {
                    // Logged as they arrived, but never run.
                    let n = queue.len();
                    self.events
                        .send_or_drop(Event::Warning {
                            message: format!(
                                "{n} queued command{} {} not run: the session is shutting down",
                                if n == 1 { "" } else { "s" },
                                if n == 1 { "was" } else { "were" }
                            ),
                        })
                        .await;
                }
                break;
            }
            tokio::select! {
                cmd = commands.recv(), if !stopping => {
                    let cmd = cmd.unwrap_or(Command::Shutdown);
                    // A fork moves the log to its branch when it ends: what applies after it
                    // waits, unlogged, and is logged in the branch at the join.
                    // A replay is refused at once (idle only), never held.
                    if running.as_ref().is_some_and(|r| r.fork)
                        && !Self::urgent(&cmd)
                        && !matches!(cmd, Command::Replay { .. })
                    {
                        if held.len() < COMMAND_CAPACITY {
                            held.push(cmd);
                        } else {
                            for ev in refusal(&cmd, &too_many(), None, &self.session_id) {
                                self.events.try_emit(ev);
                            }
                        }
                        continue;
                    }
                    let lost = state.is_none() && running.is_none();
                    let busy = running.is_some();
                    self.received(cmd, lost, running.as_ref(), busy, &mut queue, &mut stopping)
                        .await;
                }
                joined = async { (&mut running.as_mut().expect("guarded").handle).await }, if running.is_some() => {
                    let started = running.take().map(|r| r.cmd);
                    match joined {
                        Ok((mut s, mut inbox)) => {
                            inbox.close();
                            while let Ok(cmd) = inbox.try_recv() {
                                s.context_command(cmd).await;
                            }
                            // A fork continues on its branch (`current` is the job's).
                            self.session_id.clone_from(&s.session_id);
                            state = Some(s);
                        }
                        Err(e) => {
                            if let Some(cmd) = started {
                                self.crashed(e, &cmd).await;
                            }
                        }
                    }
                    let held = std::mem::take(&mut held);
                    if stopping && !held.is_empty() {
                        // Logged as they arrived would have been, but never run.
                        let n = held.len();
                        let recs: Vec<SessionRecord> =
                            held.iter().map(|c| SessionRecord::Command(c.for_log())).collect();
                        self.with_log(move |l| recs.iter().try_for_each(|r| l.append(r)))
                            .await;
                        self.events.try_emit(Event::Warning {
                            message: format!(
                                "{n} command{} sent during the fork {} not run: the session \
                                 is shutting down",
                                if n == 1 { "" } else { "s" },
                                if n == 1 { "was" } else { "were" }
                            ),
                        });
                    } else {
                        for cmd in held {
                            let lost = state.is_none();
                            self.received(cmd, lost, None, false, &mut queue, &mut stopping)
                                .await;
                        }
                    }
                }
            }
        }
        if let Some(s) = state.as_mut() {
            s.finish_turn_end().await;
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
        // Closed, not just synced: the lock goes now, before the event stream ends, so a
        // client that resumes the session as soon as it closed finds it free (review M6).
        self.with_log(|l| l.close()).await;
        // The episode's usage included: a client that exits once the event stream closes
        // loses no row, unless the ledger is stuck. (Also after a crashed job lost the
        // session: the actor keeps the ledger's handle.)
        self.cost_ledger.flush(LEDGER_FLUSH).await;
    }

    /// A command from the channel: logged, then handled at once (shutdown, approvals,
    /// cancel, budgets, pins for the running turn) or queued for when no turn runs. `busy`:
    /// something runs (a job, the start-up, a `SetModel` build), so a `Replay` is refused.
    /// A command the queue or the turn's inbox has no room for is refused, unlogged.
    async fn received(
        &self,
        cmd: Command,
        lost: bool,
        running: Option<&Running>,
        busy: bool,
        queue: &mut VecDeque<Command>,
        stopping: &mut bool,
    ) {
        // A session rule is for tool calls only: on an MCP server approval it is refused
        // (not logged, the request left pending) rather than dropped.
        if let Command::Approve {
            request_id,
            session_rule: Some(_),
        } = &cmd
            && self.approvals.kind(request_id) == Some(ApprovalFor::McpServer)
        {
            self.answer_later(vec![Event::Error {
                id: None,
                kind: ErrorKind::Policy,
                message: format!(
                    "session_rule applies to tool calls only: the MCP server approval `{}` \
                     is still pending",
                    crate::printable(request_id)
                ),
                retryable: false,
                hint: Some("approve or deny it without a session_rule".into()),
            }]);
            return;
        }
        let to_inbox = matches!(
            cmd,
            Command::Pin { .. }
                | Command::Unpin { .. }
                | Command::ApproveMemory { .. }
                | Command::RejectMemory { .. }
        ) && running.is_some();
        let queued = !lost
            && !to_inbox
            && !Self::urgent(&cmd)
            && !matches!(
                cmd,
                Command::SetBudget { .. } | Command::SetReasoning { .. } | Command::Unknown
            )
            && !(busy && matches!(cmd, Command::Replay { .. }));
        let full = if to_inbox {
            running.is_some_and(|r| r.inbox.capacity() == 0)
        } else {
            queued && queue.len() >= COMMAND_CAPACITY
        };
        if full {
            for ev in refusal(&cmd, &too_many(), None, &self.session_id) {
                self.events.try_emit(ev);
            }
            return;
        }
        self.with_log({
            let rec = SessionRecord::Command(cmd.for_log());
            move |l| l.append(&rec)
        })
        .await;
        match cmd {
            Command::Shutdown => {
                *stopping = true;
                // Stop first, then say which approvals no longer wait.
                let withdrawn = self.approvals.deny_all("the session is shutting down");
                self.shutdown.cancel();
                self.answer_later(
                    withdrawn
                        .into_iter()
                        .map(|request_id| Event::ApprovalWithdrawn { request_id })
                        .collect(),
                );
            }
            // After a crashed turn (or a failed start) every command gets the same answer,
            // plus the end its client waits for.
            _ if lost => self.refuse_lost(&cmd).await,
            Command::Unknown => self.events.try_emit(Event::Error {
                id: None,
                kind: ErrorKind::Internal,
                message: "this engine does not know this command (sent by a newer client?); \
                          it was ignored"
                    .into(),
                retryable: false,
                hint: None,
            }),
            Command::Approve {
                request_id,
                session_rule,
            } => {
                self.resolve(&request_id, Verdict::Approved { session_rule });
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
            Command::SetReasoning { level } => {
                *crate::lock(&self.reasoning) = level;
            }
            cmd @ (Command::Pin { .. }
            | Command::Unpin { .. }
            | Command::ApproveMemory { .. }
            | Command::RejectMemory { .. })
                if running.is_some() =>
            {
                // Room was checked above; only the actor sends.
                if let Some(r) = running {
                    let _ = r.inbox.try_send(cmd);
                }
            }
            // A replay is a view of a log, never queued behind live work (spec 7.12.5).
            Command::Replay { id, .. } if busy => {
                self.answer_later(vec![Event::Error {
                    id,
                    kind: ErrorKind::Internal,
                    message: crate::replay::REPLAY_BUSY.into(),
                    retryable: false,
                    hint: None,
                }]);
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
        crate::lock(&self.budgets).set(
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

    fn spawn_job(&self, mut session: Session, job: Job, cmd: Command) -> Running {
        let cancel = self.shutdown.child_token();
        let (inbox_tx, inbox_rx) = mpsc::channel(COMMAND_CAPACITY);
        let answering = session.events.tagged(&cmd);
        let mut ctl = TurnControl {
            cancel: cancel.clone(),
            inbox: inbox_rx,
            approvals: self.approvals.clone(),
        };
        // A fork, rewind or switch moves the log: what arrives meanwhile waits for it.
        let fork = matches!(
            job,
            Job::Fork { .. } | Job::Rewind { .. } | Job::Switch { .. }
        );
        let current = Arc::clone(&self.current);
        let moved = move |id: &str, path: &Path| {
            *crate::lock(&current) = (id.to_string(), path.to_path_buf());
        };
        let handle = tokio::spawn(async move {
            // The job's answer echoes the command's correlation id (`EventSink::tagged`).
            let unbound = session.events.clone();
            session.events = answering;
            match job {
                Job::Turn(text) => session.run_turn(text, &mut ctl).await,
                Job::Compact => session.compact_command(&ctl).await,
                Job::Consolidate => session.consolidate_command(&ctl).await,
                Job::Undo { turn, dry_run } => session.undo_job(turn, dry_run, &ctl).await,
                Job::Fork { at, label } => session.fork_job(at, label, &ctl, moved).await,
                Job::Rewind {
                    at,
                    label,
                    dry_run,
                    overwrite,
                } => {
                    session
                        .rewind_job(at, label, dry_run, overwrite, &ctl, moved)
                        .await
                }
                Job::Switch {
                    session_id,
                    dry_run,
                    overwrite,
                } => {
                    session
                        .switch_job(session_id, dry_run, overwrite, &ctl, moved)
                        .await
                }
                Job::Replay {
                    session_id,
                    speed,
                    from_step,
                } => session.replay_job(session_id, speed, from_step, &ctl).await,
            }
            session.events = unbound;
            (session, ctl.inbox)
        });
        Running {
            handle,
            cancel,
            inbox: inbox_tx,
            cmd,
            fork,
        }
    }

    /// What is handled at once even while a fork runs: it stops the fork or answers it.
    fn urgent(cmd: &Command) -> bool {
        matches!(
            cmd,
            Command::Cancel | Command::Shutdown | Command::Approve { .. } | Command::Deny { .. }
        )
    }

    fn resolve(&self, request_id: &str, verdict: Verdict) {
        if !self.approvals.resolve(request_id, verdict) {
            self.events.try_emit(Event::Warning {
                message: format!("no approval request `{request_id}` is pending"),
            });
        }
    }

    /// A short command that runs between turns, in the actor. Its answer waits (a bounded
    /// time) for a client that is behind.
    async fn idle_command(&mut self, session: &mut Session, cmd: Command) {
        let unbound = session.events.clone();
        session.events = unbound.replying().tagged(&cmd);
        self.idle_command_inner(session, cmd).await;
        session.events = unbound;
    }

    async fn idle_command_inner(&mut self, session: &mut Session, cmd: Command) {
        match cmd {
            Command::Pin { .. }
            | Command::Unpin { .. }
            | Command::ApproveMemory { .. }
            | Command::RejectMemory { .. } => session.context_command(cmd).await,
            Command::Diff { id: _, turn } => session.diff(turn).await,
            Command::Uncompact => session.uncompact_command().await,
            Command::Label { label } => {
                session.log(SessionRecord::Labeled { label }).await;
                session.checkpoint().await;
            }
            other => {
                let name = serde_json::to_value(&other)
                    .ok()
                    .and_then(|v| v["type"].as_str().map(str::to_string))
                    .unwrap_or_default();
                self.events.try_emit(Event::Error {
                    id: None,
                    kind: ErrorKind::Internal,
                    message: format!("the `{name}` command is not supported yet"),
                    retryable: false,
                    hint: None,
                });
            }
        }
    }

    /// A job panicked: an engine bug. The state it held is gone, but the log is intact up
    /// to the crash (plus the panic's message), so the session can be resumed. Only a
    /// user message's turn is ended (`TurnEnd`, `TurnFinished`): a command's job (a
    /// compaction, a consolidation) started none, but still gets the end its client waits
    /// for (`CompactFinished`, `ReplayFinished`, ...).
    async fn crashed(&self, e: tokio::task::JoinError, cmd: &Command) {
        let turn = matches!(cmd, Command::SendMessage { .. });
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
        let mut recs = vec![SessionRecord::EngineError {
            message: what.clone(),
        }];
        if turn {
            recs.push(SessionRecord::TurnEnd {
                reason: FinishReason::Error,
                evidence: None,
                synthetic: false,
            });
        }
        self.with_log(move |l| {
            recs.iter().try_for_each(|r| l.append(r))?;
            l.checkpoint()
        })
        .await;
        let events = self.events.tagged(cmd);
        events
            .send_or_drop(Event::Error {
                id: None,
                kind: ErrorKind::Internal,
                message: format!(
                    "engine bug in session {}: {what}; the session log is intact",
                    self.session_id
                ),
                retryable: false,
                hint: Some("resume the session to continue".into()),
            })
            .await;
        if let Some(end) = failed_answer(cmd, &self.session_id) {
            events.emit_last(end).await;
        }
    }

    /// The answer to a command a lost session (a crashed job, a failed start) cannot run:
    /// an `Error`, then the end its client waits for (`TurnFinished{error}` for a message,
    /// `ReplayFinished`, `CompactFinished`, `ConsolidateFinished`; for a `SetModel` the
    /// error is its `cannot switch to <model>:` refusal), through
    /// [`answer_later`](Self::answer_later).
    async fn refuse_lost(&self, cmd: &Command) {
        let why = format!(
            "session {} stopped after an internal error and takes no more commands",
            self.session_id
        );
        let hint = Some("resume the session to continue");
        self.answer_later(refusal(cmd, &why, hint, &self.session_id));
    }

    /// Answers a client waits for, sent in order off the actor: each waits a bounded time
    /// for room (a busy client still gets it) while the actor goes on reading `Cancel` and
    /// `Shutdown` (re-review N1).
    fn answer_later(&self, events: Vec<Event>) {
        if events.is_empty() {
            return;
        }
        let sink = self.events.clone();
        tokio::spawn(async move {
            for ev in events {
                sink.emit_last(ev).await;
            }
        });
    }
}
