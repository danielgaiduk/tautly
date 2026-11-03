//! A session's state, owned by the engine's actor between turns and by the turn task
//! while one runs, plus the two outlets everything goes through: the event channel and
//! the session log.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tautly_context::pinned::{self, PinSource, Pinned};
use tautly_context::{CompactionRecord, Context, agents_md};
use tautly_provider::{CanonicalRequest, ModelId, ModelInfo, Provider, ToolSpec};
use tautly_sandbox::{Policy, Sandbox, cleanup_spills};
use tautly_session::{BlobRef, SessionWriter};
use tautly_tools::{ProcessGroups, ReadTracker, Tool};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::attempts::AttemptLedger;
use crate::budget::Budgets;
use crate::ledger::Ledger;
use crate::mcp::{McpSession, PreExecHook};
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

/// How long an event waits for room in the channel once the turn is cancelled (or for
/// the actor's last words) before it is dropped.
const SEND_GRACE: Duration = Duration::from_secs(1);
/// How long a cancelled turn's `TurnFinished` (and the drop warning before it) waits.
const FINISH_GRACE: Duration = Duration::from_secs(5);

/// Where events go. A client that stopped listening is not an error, and a client that
/// stopped reading cannot wedge the engine:
///
/// - A sink [`bound`](EventSink::bound) to a turn waits on a full channel until the turn is
///   cancelled. After that one event may wait [`SEND_GRACE`]; once one has timed out the
///   turn's sink is overflowing and every further event is dropped at once, except
///   `TurnFinished`, which (with the drop warning just before it) waits [`FINISH_GRACE`]
///   once, so a busy but reading client still learns that the turn ended.
/// - An unbound sink (the actor's, and the session's between turns) never waits.
///
/// Dropped events are counted and announced with a warning when there is room.
#[derive(Clone)]
pub(crate) struct EventSink {
    tx: mpsc::Sender<Event>,
    cancel: Option<CancellationToken>,
    dropped: Arc<AtomicU64>,
    /// Set for the turn once a post-cancel send timed out.
    overflowing: Arc<AtomicBool>,
}

impl EventSink {
    pub fn new(tx: mpsc::Sender<Event>) -> Self {
        EventSink {
            tx,
            cancel: None,
            dropped: Arc::default(),
            overflowing: Arc::default(),
        }
    }

    /// This sink, giving up on a full channel once `cancel` fires.
    pub fn bound(&self, cancel: CancellationToken) -> Self {
        EventSink {
            cancel: Some(cancel),
            overflowing: Arc::default(),
            ..self.clone()
        }
    }

    pub async fn emit(&self, ev: Event) {
        let Some(cancel) = &self.cancel else {
            return self.try_emit(ev);
        };
        if !cancel.is_cancelled() {
            self.announce_dropped();
            let permit = tokio::select! {
                permit = self.tx.reserve() => Some(permit),
                () = cancel.cancelled() => None,
            };
            match permit {
                Some(Ok(permit)) => return permit.send(ev),
                Some(Err(_)) => return,
                None => {}
            }
        }
        if matches!(ev, Event::TurnFinished { .. }) {
            return self.finish(ev).await;
        }
        if self.overflowing.load(Ordering::Relaxed) {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        if let Err(mpsc::error::SendTimeoutError::Timeout(_)) =
            self.tx.send_timeout(ev, SEND_GRACE).await
        {
            self.overflowing.store(true, Ordering::Relaxed);
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// A cancelled turn's last event: the drop warning (if any), then `ev`, within one
    /// [`FINISH_GRACE`].
    async fn finish(&self, ev: Event) {
        let deadline = tokio::time::Instant::now() + FINISH_GRACE;
        let n = self.dropped.swap(0, Ordering::Relaxed);
        if n > 0 {
            match tokio::time::timeout_at(deadline, self.tx.reserve()).await {
                Ok(Ok(permit)) => permit.send(dropped_warning(n)),
                Ok(Err(_)) => return,
                Err(_) => {
                    self.dropped.fetch_add(n, Ordering::Relaxed);
                }
            }
        }
        match tokio::time::timeout_at(deadline, self.tx.reserve()).await {
            Ok(Ok(permit)) => permit.send(ev),
            Ok(Err(_)) => {}
            Err(_) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Send within [`SEND_GRACE`], else drop and count.
    pub async fn send_or_drop(&self, ev: Event) {
        if let Err(mpsc::error::SendTimeoutError::Timeout(_)) =
            self.tx.send_timeout(ev, SEND_GRACE).await
        {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Send now if there is room, else drop and count. Never waits.
    pub fn try_emit(&self, ev: Event) {
        self.announce_dropped();
        if let Err(mpsc::error::TrySendError::Full(_)) = self.tx.try_send(ev) {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub async fn warn(&self, message: impl Into<String>) {
        self.emit(Event::Warning {
            message: message.into(),
        })
        .await;
    }

    /// Say how many events were dropped, if any and if there is room now.
    fn announce_dropped(&self) {
        let n = self.dropped.swap(0, Ordering::Relaxed);
        if n == 0 {
            return;
        }
        if self.tx.try_send(dropped_warning(n)).is_err() {
            self.dropped.fetch_add(n, Ordering::Relaxed);
        }
    }
}

fn dropped_warning(n: u64) -> Event {
    Event::Warning {
        message: format!(
            "{n} event{} dropped because the client was not reading events",
            if n == 1 { " was" } else { "s were" }
        ),
    }
}

/// The session log, shared by the actor (which logs commands as they arrive) and the turn.
/// A failure poisons the writer, so the session is no longer persisted; it is reported
/// once ([`LogFailure::event`]) and the session carries on in memory.
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
    /// Where [`Session::log_request`] keeps the latest request.
    pub last_request_path: PathBuf,
    pub index_error_reported: bool,
    pub session_id: String,
    /// Limits and spend; `SetBudget` changes the limits from the actor at any time.
    pub budgets: Arc<Mutex<Budgets>>,
    /// Wall-clock limit per user message.
    pub turn_timeout: Option<Duration>,
    /// The cost ledger file; opened at [`Session::init`].
    pub cost_ledger_path: PathBuf,
    /// None when it could not be opened (said once).
    pub cost_ledger: Option<Arc<Ledger>>,
    /// A ledger problem was reported; later ones are not.
    pub cost_ledger_reported: bool,
    /// The process groups this session's commands started (killed at shutdown).
    pub groups: ProcessGroups,
    /// The session's MCP servers, once started (`mcp.rs`); their tools join `tools` at
    /// user-turn starts only.
    pub mcp: Option<McpSession>,
    /// Runs before every stdio MCP call (checkpoints, Task 47).
    pub pre_exec: Arc<dyn PreExecHook>,
}

impl Session {
    pub async fn emit(&self, ev: Event) {
        self.events.emit(ev).await;
    }

    pub async fn warn(&self, message: impl Into<String>) {
        self.events.warn(message).await;
    }

    pub async fn report(&self, failure: LogFailure) {
        if let Some(ev) = failure.event() {
            self.emit(ev).await;
        }
    }

    /// Run `f` on the log off the async workers (the log writes files and fsyncs). A
    /// failure is reported once and otherwise ignored: the session goes on in memory.
    pub async fn with_log<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Log) -> Result<T, LogFailure> + Send + 'static,
    ) -> Option<T> {
        let log = self.log.clone();
        match tokio::task::spawn_blocking(move || f(&log)).await {
            Ok(Ok(t)) => Some(t),
            Ok(Err(failure)) => {
                self.report(failure).await;
                None
            }
            Err(_) => None,
        }
    }

    /// Append a record (buffered; durable at the next checkpoint).
    pub async fn log(&self, rec: SessionRecord) {
        self.with_log(move |l| l.append(&rec)).await;
    }

    /// Append several records with one trip off the async workers.
    pub async fn log_all(&self, recs: Vec<SessionRecord>) {
        self.with_log(move |l| recs.iter().try_for_each(|r| l.append(r)))
            .await;
    }

    pub async fn blob(&self, bytes: &[u8]) -> Option<BlobRef> {
        let bytes = bytes.to_vec();
        self.with_log(move |l| l.blob(&bytes)).await
    }

    /// Flush and fsync: at tool-run and turn boundaries.
    pub async fn checkpoint(&self) {
        self.with_log(|l| l.checkpoint()).await;
    }

    /// Log a request by its hash only, and keep the request itself, unsynced, in the
    /// session's last-request file (for `prompt show --full`): a blob per request would
    /// make the log grow with the square of the session length.
    pub async fn log_request(&self, req: &CanonicalRequest) {
        let bytes = serde_json::to_vec(req).unwrap_or_default();
        let hash = BlobRef::parse(blake3::hash(&bytes).to_hex().as_str())
            .expect("a blake3 hex digest is a blob reference");
        let path = self.last_request_path.clone();
        self.with_log(move |l| {
            write_private(&path, &bytes);
            l.append(&SessionRecord::Request { hash })
        })
        .await;
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

    /// The cost ledger cannot be used: said once, and the session runs without it.
    pub async fn ledger_unavailable(&mut self, why: String) {
        self.cost_ledger = None;
        if !std::mem::replace(&mut self.cost_ledger_reported, true) {
            self.warn(format!(
                "the cost ledger is unavailable ({why}); this session's usage is not recorded"
            ))
            .await;
        }
    }

    /// Session start, after the first event: spill cleanup in the background, the cost
    /// ledger, config pins, AGENTS.md (once), and the background build of the repo map and
    /// memory index.
    pub async fn init(&mut self, config_pins: &[String]) {
        let root = self.sandbox.root().to_path_buf();
        tokio::task::spawn_blocking(move || cleanup_spills(&root, SPILL_MAX_AGE, SPILL_MAX_BYTES));
        let path = self.cost_ledger_path.clone();
        match tokio::task::spawn_blocking(move || Ledger::open(&path)).await {
            Ok(Ok(ledger)) => self.cost_ledger = Some(Arc::new(ledger)),
            Ok(Err(e)) => self.ledger_unavailable(e.to_string()).await,
            Err(e) => self.ledger_unavailable(e.to_string()).await,
        }
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

/// Replace `path` with `bytes`, readable by the user only (requests hold the whole
/// conversation). Best effort, not synced: it is a debugging aid, not the record.
fn write_private(path: &Path, bytes: &[u8]) {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .and_then(|mut f| f.write_all(bytes));
    drop(written);
}
