//! A session's state, owned by the engine's actor between turns and by the turn task
//! while one runs, plus the two outlets everything goes through: the event channel and
//! the session log.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tautly_context::pinned::{self, PinSource, Pinned};
use tautly_context::{CompactionRecord, Context, agents_md};
use tautly_provider::{ModelId, ModelInfo, Provider, ToolSpec};
use tautly_sandbox::{Policy, Sandbox, cleanup_spills};
use tautly_session::{BlobRef, SessionWriter};
use tautly_tools::{ReadTracker, Tool};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::attempts::AttemptLedger;
use crate::protocol::{Command, ErrorKind, Event};
use crate::record::SessionRecord;
use crate::workspace::Workspace;

/// How long the first request waits for the repo map and the memory index (Task 38 note):
/// past this the slots stay empty for the session, so the stable prefix never changes.
const STARTUP_WAIT: Duration = Duration::from_millis(300);
/// Tokens of the memory index block (spec 7.4).
const MEMORY_INDEX_TOKENS: u32 = 500;
/// Spill files older than this are removed at engine start.
const SPILL_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 3600);
/// The spill store is cut back to this size at engine start.
const SPILL_MAX_BYTES: u64 = 256 * 1024 * 1024;

/// Where events go. A client that stopped listening is not an error.
#[derive(Clone)]
pub(crate) struct EventSink {
    tx: mpsc::Sender<Event>,
}

impl EventSink {
    pub fn new(tx: mpsc::Sender<Event>) -> Self {
        EventSink { tx }
    }

    pub async fn emit(&self, ev: Event) {
        let _ = self.tx.send(ev).await;
    }

    pub async fn warn(&self, message: impl Into<String>) {
        self.emit(Event::Warning {
            message: message.into(),
        })
        .await;
    }
}

/// The session log, shared by the actor (which logs commands as they arrive) and the turn.
/// A failure poisons the writer, so the session is no longer persisted; it is reported
/// once ([`Log::failure`]) and the session carries on in memory.
#[derive(Clone)]
pub(crate) struct Log {
    inner: Arc<Mutex<LogInner>>,
}

struct LogInner {
    writer: SessionWriter<SessionRecord>,
    reported: bool,
}

impl Log {
    pub fn new(writer: SessionWriter<SessionRecord>) -> Self {
        Log {
            inner: Arc::new(Mutex::new(LogInner {
                writer,
                reported: false,
            })),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, LogInner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn append(&self, rec: &SessionRecord) -> Result<(), LogFailure> {
        let mut g = self.lock();
        let r = g.writer.append(rec).map(drop);
        Self::fail(&mut g, r)
    }

    pub fn blob(&self, bytes: &[u8]) -> Result<BlobRef, LogFailure> {
        let mut g = self.lock();
        let r = g.writer.blob(bytes);
        Self::fail(&mut g, r)
    }

    /// Flush and fsync (tool and turn boundaries).
    pub fn checkpoint(&self) -> Result<(), LogFailure> {
        let mut g = self.lock();
        let r = g.writer.checkpoint();
        Self::fail(&mut g, r)
    }

    fn fail<T>(g: &mut LogInner, r: tautly_session::Result<T>) -> Result<T, LogFailure> {
        r.map_err(|e| {
            let first = !std::mem::replace(&mut g.reported, true);
            LogFailure(first.then(|| e.to_string()))
        })
    }
}

/// A log write failed; the message is set the first time only.
pub(crate) struct LogFailure(Option<String>);

impl LogFailure {
    /// The error event to show, the first time the log fails.
    pub fn event(self) -> Option<Event> {
        self.0.map(|why| Event::Error {
            kind: ErrorKind::Internal,
            message: format!(
                "the session log could not be written ({why}); this session is no longer saved"
            ),
            retryable: false,
            hint: Some("start a new session to keep a log".into()),
        })
    }
}

/// The stable-prefix parts that need the code index: built in the background at session
/// start and taken by the first request if ready within [`STARTUP_WAIT`].
#[derive(Default)]
pub(crate) struct Prefix {
    pub repo_map: Option<String>,
    pub memory_index: Option<String>,
}

/// The background start-up work of one session; dropping it stops the work.
pub(crate) struct Startup {
    prefix: Option<oneshot::Receiver<Prefix>>,
    tasks: Vec<JoinHandle<()>>,
}

impl Drop for Startup {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

/// What a session's state is made of; see the module docs.
pub(crate) struct Session {
    pub context: Context,
    pub reads: Arc<ReadTracker>,
    pub policy: Arc<Policy>,
    pub ledger: AttemptLedger,
    pub log: Log,
    pub events: EventSink,
    pub sandbox: Arc<dyn Sandbox>,
    pub workspace: Arc<Workspace>,
    /// Wrapped in `Retrying`.
    pub provider: Arc<dyn Provider>,
    pub model: ModelId,
    pub info: ModelInfo,
    pub tools: Vec<Arc<dyn Tool>>,
    pub specs: Vec<ToolSpec>,
    pub max_turns: u32,
    pub compaction_threshold: Option<f32>,
    pub startup: Option<Startup>,
    /// The latest compaction, for `Uncompact` (Task 26); older ones live only in the log.
    pub latest_compaction: Option<CompactionRecord>,
    /// A memory index text waiting for the next cache epoch (spec 7.4/7.7). Set by the
    /// memory flows (Task 28); applied, and logged as `MemoryIndexApplied`, at an epoch.
    pub pending_memory_index: Option<String>,
    /// The provider whose reasoning items the history holds; a request to another provider
    /// first strips them (they are provider-specific).
    pub history_provider: String,
    /// Non-UTF-8 paths already reported as not tracked.
    pub untracked_paths: HashSet<PathBuf>,
    pub index_error_reported: bool,
}

impl Session {
    pub async fn emit(&self, ev: Event) {
        self.events.emit(ev).await;
    }

    pub async fn warn(&self, message: impl Into<String>) {
        self.events.warn(message).await;
    }

    async fn report(&self, failure: LogFailure) {
        if let Some(ev) = failure.event() {
            self.emit(ev).await;
        }
    }

    /// Append a record; a failure is reported once and otherwise ignored.
    pub async fn log(&self, rec: SessionRecord) {
        if let Err(f) = self.log.append(&rec) {
            self.report(f).await;
        }
    }

    pub async fn blob(&self, bytes: &[u8]) -> Option<BlobRef> {
        match self.log.blob(bytes) {
            Ok(b) => Some(b),
            Err(f) => {
                self.report(f).await;
                None
            }
        }
    }

    pub async fn checkpoint(&self) {
        if let Err(f) = self.log.checkpoint() {
            self.report(f).await;
        }
    }

    /// Say once per path that a non-UTF-8 path is left out of the log.
    pub async fn untracked(&mut self, paths: Vec<PathBuf>) {
        for p in paths {
            if self.untracked_paths.insert(p.clone()) {
                self.warn(format!(
                    "non-UTF-8 path not tracked: {} (the change applies, but resume and undo \
                     will not know about it)",
                    p.to_string_lossy()
                ))
                .await;
            }
        }
    }

    /// Session start, after the first event: spill cleanup in the background, config pins,
    /// AGENTS.md (once), and the background build of the repo map and memory index.
    pub async fn init(&mut self, config_pins: &[String]) {
        let root = self.sandbox.root().to_path_buf();
        tokio::task::spawn_blocking(move || cleanup_spills(&root, SPILL_MAX_AGE, SPILL_MAX_BYTES));
        for pin in config_pins {
            pinned::add(
                &mut self.context.pinned,
                Pinned::new(pin, PinSource::Config),
            );
        }
        let sb = Arc::clone(&self.sandbox);
        let found = tokio::task::spawn_blocking(move || agents_md::load(sb.as_ref()))
            .await
            .unwrap_or_default();
        for w in &found.warnings {
            self.warn(w.clone()).await;
        }
        self.context.set_agents_md(found);
        self.startup = Some(self.spawn_startup());
    }

    fn spawn_startup(&self) -> Startup {
        let ws = Arc::clone(&self.workspace);
        let events = self.events.clone();
        let warn_task = tokio::spawn(async move {
            for w in &ws.opened().await.warnings {
                events.warn(w.clone()).await;
            }
        });
        let ws = Arc::clone(&self.workspace);
        let events = self.events.clone();
        let tokens = self.context.tokens.clone();
        let (tx, rx) = oneshot::channel();
        let prefix_task = tokio::spawn(async move {
            let opened = ws.opened().await;
            let Some(index) = opened.index.clone() else {
                let _ = tx.send(Prefix::default());
                return;
            };
            // Bounded only by the session: the first request stops waiting long before.
            if !index.wait_ready(Duration::from_secs(24 * 3600)).await {
                let _ = tx.send(Prefix::default());
                return;
            }
            let est = tokens.clone();
            let map = tokio::task::spawn_blocking(move || {
                let mut scratch = Context {
                    tokens: est,
                    ..Context::default()
                };
                // A false return (nothing to map) leaves the slot empty for the session.
                scratch.init_repo_map(&index);
                scratch.repo_map
            })
            .await
            .ok()
            .flatten();
            let memory = match opened.memory.clone() {
                Some(store) => tokio::task::spawn_blocking(move || {
                    let mut store = store.write().unwrap_or_else(PoisonError::into_inner);
                    store.refresh_staleness();
                    let error = store.staleness_error().map(str::to_string);
                    let (text, _over) =
                        store.index_block(MEMORY_INDEX_TOKENS, &|s| tokens.estimate(s));
                    (text, error)
                })
                .await
                .ok(),
                None => None,
            };
            let mut memory_index = None;
            if let Some((text, error)) = memory {
                if let Some(e) = error {
                    events
                        .warn(format!("memory staleness could not be checked: {e}"))
                        .await;
                }
                memory_index = Some(text).filter(|t| !t.trim().is_empty());
            }
            let _ = tx.send(Prefix {
                repo_map: map,
                memory_index,
            });
        });
        Startup {
            prefix: Some(rx),
            tasks: vec![warn_task, prefix_task],
        }
    }

    /// Before the first request: take the repo map and memory index if they are ready
    /// within [`STARTUP_WAIT`], else run the session without them.
    pub async fn apply_startup(&mut self) {
        let Some(rx) = self.startup.as_mut().and_then(|s| s.prefix.take()) else {
            return;
        };
        match tokio::time::timeout(STARTUP_WAIT, rx).await {
            Ok(Ok(prefix)) => {
                self.context.repo_map = prefix.repo_map;
                if let Some(text) = prefix.memory_index {
                    self.context.memory_index = Some(text.clone());
                    self.log(SessionRecord::MemoryIndexApplied { text }).await;
                }
            }
            _ => {
                self.warn(
                    "the code index is still building: this session runs without a repo map \
                     and memory index",
                )
                .await;
            }
        }
    }

    /// `Command::Pin` (spec 6.2): a user pin, shown in the state tail until the next epoch.
    pub async fn user_pin(&mut self, text: String) {
        match self.context.pin(text, PinSource::User) {
            Ok(true) => {}
            Ok(false) => self.warn("that text is already pinned").await,
            Err(e) => self.warn(format!("pin refused: {e}")).await,
        }
    }

    /// `Command::Unpin`: remove the pin whose text is exactly `text`.
    pub async fn unpin(&mut self, text: &str) {
        let before = self.context.pinned.len() + self.context.session_pins.len();
        self.context.pinned.retain(|p| p.text != text);
        self.context.session_pins.retain(|p| p.text != text);
        if before == self.context.pinned.len() + self.context.session_pins.len() {
            self.warn("no pin has exactly that text").await;
        }
    }

    /// A command that changes the context, applied between steps.
    pub async fn context_command(&mut self, cmd: Command) {
        match cmd {
            Command::Pin { text } => self.user_pin(text).await,
            Command::Unpin { text } => self.unpin(&text).await,
            _ => {}
        }
    }

    pub fn root(&self) -> &Path {
        self.sandbox.root()
    }
}
