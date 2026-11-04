//! A session's state, owned by the engine's actor between turns and by the turn task
//! while one runs, plus the two outlets everything goes through: the event channel and
//! the session log.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use tautly_context::pinned::{self, PinSource, Pinned};
use tautly_context::{CompactionRecord, Context, Turn, agents_md};
use tautly_provider::{Block, CanonicalRequest, ModelId, ModelInfo, Provider, ToolSpec};
use tautly_sandbox::{Policy, Sandbox, cleanup_spills};
use tautly_session::{BlobRef, HeaderInit, SessionWriter};
use tautly_tools::{ProcessGroups, ReadTracker, Tool};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::attempts::AttemptLedger;
use crate::budget::Budgets;
use crate::checkpoint::{GitCheckpoints, StartCheckpoint};
use crate::evidence::EvidenceTracker;
use crate::ledger::CostLedger;
use crate::mcp::{McpSession, PreExecHook};
use crate::memory::MemoryState;
use crate::model_switch::ModelSetup;
use crate::protocol::{Command, ErrorKind, Event};
use crate::receipt::ReceiptBuilder;
use crate::record::{BlockHashes, LoggedPin, SessionRecord};
use crate::routing::Side;
use crate::workspace::Workspace;

/// How long the first request waits for the repo map and the memory index (Task 38 note):
/// past this the slots stay empty for the session, so the stable prefix never changes.
const STARTUP_WAIT: Duration = Duration::from_millis(300);
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
/// - A [`replying`](EventSink::replying) sink (the session's while it answers a command
///   between turns) waits [`SEND_GRACE`] for room; once one has timed out every further
///   event of that answer is dropped at once.
/// - An unbound sink (the actor's, and the session's between turns) never waits.
///
/// Dropped events are counted and announced with a warning when there is room.
#[derive(Clone)]
pub(crate) struct EventSink {
    tx: mpsc::Sender<Event>,
    cancel: Option<CancellationToken>,
    /// Waits [`SEND_GRACE`] on a full channel (an answer the client asked for).
    replying: bool,
    dropped: Arc<AtomicU64>,
    /// Set for the turn (or the answer) once a send timed out.
    overflowing: Arc<AtomicBool>,
}

impl EventSink {
    pub fn new(tx: mpsc::Sender<Event>) -> Self {
        EventSink {
            tx,
            cancel: None,
            replying: false,
            dropped: Arc::default(),
            overflowing: Arc::default(),
        }
    }

    /// This sink, giving up on a full channel once `cancel` fires.
    pub fn bound(&self, cancel: CancellationToken) -> Self {
        EventSink {
            cancel: Some(cancel),
            replying: false,
            overflowing: Arc::default(),
            ..self.clone()
        }
    }

    /// This sink, for the answer to a command the client sent (between turns, from the
    /// actor): each event waits [`SEND_GRACE`] for room, until one times out.
    pub fn replying(&self) -> Self {
        EventSink {
            cancel: None,
            replying: true,
            overflowing: Arc::default(),
            ..self.clone()
        }
    }

    pub async fn emit(&self, ev: Event) {
        let Some(cancel) = &self.cancel else {
            if self.replying && !self.overflowing.load(Ordering::Relaxed) {
                self.announce_dropped();
                if let Err(mpsc::error::SendTimeoutError::Timeout(_)) =
                    self.tx.send_timeout(ev, SEND_GRACE).await
                {
                    self.overflowing.store(true, Ordering::Relaxed);
                    self.dropped.fetch_add(1, Ordering::Relaxed);
                }
                return;
            }
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
        // The receipt right before `TurnFinished` gets the same chance (spec 7.11.1).
        if matches!(ev, Event::TurnFinished { .. } | Event::TurnReceipt { .. }) {
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

    /// Send now if there is room; false when it was not sent (not counted as dropped: the
    /// caller sends it again later). Never waits.
    pub fn try_send(&self, ev: Event) -> bool {
        self.announce_dropped();
        self.tx.try_send(ev).is_ok()
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

    /// The log file.
    pub fn path(&self) -> PathBuf {
        self.lock().writer.path().to_path_buf()
    }

    /// Write to `writer` from now on (a fork's log), for every holder of this log; the
    /// old writer is flushed and closed.
    pub fn replace(&self, writer: SessionWriter<SessionRecord>) {
        let mut g = self.lock();
        g.writer = writer;
        g.reported = false;
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
/// start and taken by the first request if ready within [`STARTUP_WAIT`]. The memory index
/// is rendered when it is taken, from a store whose staleness was checked once the index
/// was ready.
#[derive(Default)]
pub(crate) struct Prefix {
    pub repo_map: Option<String>,
    /// The memory store is open and its staleness checked.
    pub memory_live: bool,
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
    /// Where side work goes (spec 7.5); the main model when the catalog has no small one.
    pub side: Side,
    pub tools: Vec<Arc<dyn Tool>>,
    pub specs: Vec<ToolSpec>,
    pub max_turns: u32,
    pub compaction_threshold: Option<f32>,
    pub startup: Option<Startup>,
    /// The latest compaction, for `Uncompact` (Task 26); older ones live only in the log.
    pub latest_compaction: Option<CompactionRecord>,
    /// A memory index text waiting for the next cache epoch (spec 7.4/7.7). Set by the
    /// memory flows (`memory.rs`); applied, and logged as `MemoryIndexApplied`, at an epoch.
    pub pending_memory_index: Option<String>,
    /// Proposals, staleness and episodes (`memory.rs`).
    pub memory: MemoryState,
    /// Configuration, catalog and provider factory for `SetModel` (`model_switch.rs`).
    pub setup: ModelSetup,
    /// Set by a switch within one provider: the next request, if refused for its
    /// reasoning, is sent again once without it.
    pub reasoning_retry: bool,
    /// Paths already reported as not tracked (outside the root, or not valid UTF-8).
    pub untracked_paths: HashSet<PathBuf>,
    /// User turns started in this session (1-based once the first message runs).
    pub user_turn: u32,
    /// The stable-prefix inputs last logged (`PrefixApplied`).
    pub prefix_log: PrefixLog,
    /// Where [`Session::log_request`] keeps the latest request.
    pub last_request_path: PathBuf,
    pub index_error_reported: bool,
    pub session_id: String,
    /// Limits and spend; `SetBudget` changes the limits from the actor at any time.
    pub budgets: Arc<Mutex<Budgets>>,
    /// Wall-clock limit per user message.
    pub turn_timeout: Option<Duration>,
    /// Its background writer starts at [`Session::init`].
    pub cost_ledger: Arc<CostLedger>,
    /// A dropped ledger row was reported; later ones are not.
    pub cost_ledger_reported: bool,
    /// The process groups this session's commands started (killed at shutdown).
    pub groups: ProcessGroups,
    /// Paths of writes abandoned at their bound, with `(mtime, len)` last seen: a blocking
    /// write may still land after its future was dropped.
    pub lost_write_paths: HashMap<PathBuf, Option<(SystemTime, u64)>>,
    /// A metadata probe of a lost-write path that did not answer in time, still running:
    /// the next check waits on it again instead of starting another (on a hung file
    /// system each probe holds a blocking thread).
    pub lost_write_probes: HashMap<PathBuf, JoinHandle<Option<(SystemTime, u64)>>>,
    /// The running turn's test evidence (spec 7.11.7); replaced at every turn start.
    pub evidence: EvidenceTracker,
    /// The session's MCP servers, once started (`mcp.rs`); their tools join `tools` at
    /// user-turn starts only.
    pub mcp: Option<McpSession>,
    /// Runs before every command (`bash`, a stdio MCP call), after its checkpoint.
    pub pre_exec: Arc<dyn PreExecHook>,
    /// Checkpoints of the work tree (spec 7.11.11); none outside git or when off.
    pub checkpoints: Option<Arc<tokio::sync::Mutex<GitCheckpoints>>>,
    /// The session-start checkpoint (spec 7.11.11), taken in the background.
    pub start_checkpoint: StartCheckpoint,
    /// Where session logs live (a fork creates its log here).
    pub data_home: PathBuf,
    /// The header fields every log of this session shares (model, config hash).
    pub header_init: HeaderInit,
    /// The running user turn's receipt (spec 7.11.1); none between turns.
    pub receipt: Option<ReceiptBuilder>,
    /// Each collection of this session: the call ids it elided and the tokens it freed,
    /// so a request can tell how many elided tokens are still out of the context.
    pub gc_elided: Vec<(Vec<String>, u32)>,
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

    /// Log the request of provider step `step` by its hash only (with the user turn, the
    /// model and the hash of each block, H6), and keep the request itself, unsynced, in the
    /// session's last-request file (for `prompt show --full`): a blob per request would
    /// make the log grow with the square of the session length. A `PrefixApplied` record
    /// goes first when the stable-prefix inputs differ from the last one logged (H5); it
    /// counts as logged only once the write succeeded.
    ///
    /// The serializing and hashing (the whole request and history, every step) run off the
    /// async workers, on copies taken here.
    pub async fn log_request(&mut self, req: &CanonicalRequest, step: u32) {
        let req = req.clone();
        let history = self.context.history.clone();
        let pinned = pinned::render(&self.context.pinned);
        let agents_md = self.context.agents_md.clone();
        let repo_map = self.context.repo_map.clone();
        // A resumed session's check is for its first request only.
        let mut prefix_log = self.prefix_log.clone();
        self.prefix_log.check = false;
        let (turn, path) = (self.user_turn, self.last_request_path.clone());
        let logged = self
            .with_log(move |l| {
                let prefix = prefix_log.due(agents_md.as_deref(), repo_map.as_deref(), &req);
                let changed = prefix_log.changed(prefix.as_ref().map(|(rec, _)| rec));
                if let Some((rec, texts)) = &prefix {
                    for t in texts {
                        l.blob(t.as_bytes())?;
                    }
                    l.append(rec)?;
                }
                let bytes = serde_json::to_vec(&req).unwrap_or_default();
                let rec = SessionRecord::Request {
                    hash: blob_ref(&bytes),
                    turn,
                    step,
                    model: req.model.clone(),
                    blocks: block_hashes(&pinned, &history, &req),
                };
                write_private(&path, &bytes);
                l.append(&rec)?;
                Ok((prefix.map(|(rec, _)| rec), changed))
            })
            .await;
        let Some((rec, changed)) = logged else {
            return;
        };
        if let Some(rec) = rec {
            self.prefix_log.logged(rec);
        }
        if let Some(what) = changed {
            self.warn(format!(
                "this version's {what} from the ones this session was logged with: the \
                 provider's prompt cache is cold for this request"
            ))
            .await;
        }
    }

    /// Log a question to the user (an `ApprovalRequested` record) durably, before its
    /// event is shown (H8). Waiting for the user follows, so the fsync costs nothing felt.
    pub async fn log_approval(&self, rec: SessionRecord) {
        self.with_log(move |l| {
            l.append(&rec)?;
            l.checkpoint()
        })
        .await;
    }

    /// Say once per path that a path a record cannot hold (outside the root, or not valid
    /// UTF-8) is left out of the log.
    pub async fn untracked(&mut self, paths: Vec<PathBuf>) {
        for p in paths {
            if self.untracked_paths.insert(p.clone()) {
                self.warn(format!(
                    "path not tracked: {} (outside the project root or not valid UTF-8; the \
                     change applies, but resume and undo will not know about it)",
                    p.to_string_lossy()
                ))
                .await;
            }
        }
    }

    /// A usage row was dropped: said once; the ledger stays in use.
    pub async fn ledger_row_lost(&mut self, why: String) {
        if !std::mem::replace(&mut self.cost_ledger_reported, true) {
            self.warn(format!(
                "a usage row could not be written to the cost ledger ({why}); later rows \
                 are still recorded, and further failures are not reported"
            ))
            .await;
        }
    }

    /// Session start, after the first event: spill cleanup in the background, the cost
    /// ledger, config pins, AGENTS.md (once), and the background build of the repo map and
    /// memory index.
    pub async fn init(&mut self, config_pins: &[String]) {
        self.init_with(config_pins, false).await;
    }

    /// [`init`](Self::init); with `pins_logged` (a resumed session that never sent a
    /// request, whose log has its `StartPins` already) the pins are the logged ones: no
    /// configuration or AGENTS.md pin is added, nor logged again, and AGENTS.md is read
    /// for its text only (no request logged it yet).
    pub async fn init_with(&mut self, config_pins: &[String], pins_logged: bool) {
        self.start_housekeeping();
        if !pins_logged {
            for pin in config_pins {
                pinned::add(
                    &mut self.context.pinned,
                    Pinned::new(pin, PinSource::Config),
                );
            }
        }
        let sb = Arc::clone(&self.sandbox);
        let mut found = tokio::task::spawn_blocking(move || agents_md::load(sb.as_ref()))
            .await
            .unwrap_or_default();
        if pins_logged {
            found.pins.clear();
        }
        for w in &found.warnings {
            self.warn(w.clone()).await;
        }
        self.context.set_agents_md(found);
        if !pins_logged && !self.context.pinned.is_empty() {
            let pins = self.context.pinned.iter().map(LoggedPin::from).collect();
            self.log(SessionRecord::StartPins { pins }).await;
        }
        self.startup = Some(self.spawn_startup());
    }

    /// Spill cleanup in the background and the cost ledger's writer (every session start,
    /// a resumed one included).
    pub fn start_housekeeping(&self) {
        let root = self.sandbox.root().to_path_buf();
        tokio::task::spawn_blocking(move || cleanup_spills(&root, SPILL_MAX_AGE, SPILL_MAX_BYTES));
        // Off the start path: another process may hold the file for seconds.
        self.cost_ledger.start(self.events.clone());
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
            // Staleness is checked once the index is ready, before the first index block.
            let checked = match opened.memory.clone() {
                Some(store) => tokio::task::spawn_blocking(move || {
                    let mut store = store.write().unwrap_or_else(PoisonError::into_inner);
                    store.refresh_staleness();
                    store.staleness_error().map(str::to_string)
                })
                .await
                .ok(),
                None => None,
            };
            if let Some(Some(e)) = &checked {
                events
                    .warn(format!("memory staleness could not be checked: {e}"))
                    .await;
            }
            let _ = tx.send(Prefix {
                repo_map: map,
                memory_live: checked.is_some(),
            });
        });
        Startup {
            prefix: Some(rx),
            tasks: vec![warn_task, prefix_task],
        }
    }

    /// Before the first request: take the repo map and memory index if they are ready
    /// within [`STARTUP_WAIT`], else run the session without them. A cancel (spec 8) stops
    /// the wait and leaves them to the next turn's first request.
    pub async fn apply_startup(&mut self, cancel: &CancellationToken) {
        let Some(startup) = self.startup.as_mut() else {
            return;
        };
        let Some(ready) = take_prefix(&mut startup.prefix, STARTUP_WAIT, cancel).await else {
            return;
        };
        match ready {
            Some(prefix) => {
                self.context.repo_map = prefix.repo_map;
                if prefix.memory_live {
                    self.memory.live = true;
                    if let Some(text) = self.first_memory_index().await {
                        self.context.memory_index = Some(text.clone());
                        self.log(SessionRecord::MemoryIndexApplied { text }).await;
                    }
                }
            }
            None => {
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

    /// A command that changes the context or memory, applied between steps.
    pub async fn context_command(&mut self, cmd: Command) {
        match cmd {
            Command::Pin { text } => self.user_pin(text).await,
            Command::Unpin { text } => self.unpin(&text).await,
            Command::ApproveMemory { proposal_id } => {
                self.memory_decision(&proposal_id, true).await
            }
            Command::RejectMemory { proposal_id } => {
                self.memory_decision(&proposal_id, false).await
            }
            _ => {}
        }
    }

    pub fn root(&self) -> &Path {
        self.sandbox.root()
    }
}

/// Wait up to `wait` for the start-up prefix in `slot`: `Some(Some(prefix))` when it came,
/// `Some(None)` when it did not in time (or its task ended), and `None` when `cancel`
/// fired first, with the receiver put back in `slot` for the next turn. Nothing to wait
/// for (`slot` empty): `None`.
async fn take_prefix(
    slot: &mut Option<oneshot::Receiver<Prefix>>,
    wait: Duration,
    cancel: &CancellationToken,
) -> Option<Option<Prefix>> {
    let mut rx = slot.take()?;
    tokio::select! {
        r = tokio::time::timeout(wait, &mut rx) => Some(r.ok().and_then(Result::ok)),
        () = cancel.cancelled() => {
            *slot = Some(rx);
            None
        }
    }
}

/// The stable-prefix inputs last logged, to log `PrefixApplied` again only when they
/// change (spec 6.5 H5). A record counts once [`logged`](Self::logged): one whose write
/// failed is due again.
#[derive(Clone, Default)]
pub(crate) struct PrefixLog {
    last: Option<SessionRecord>,
    /// The next request is compared with `last` (a resumed session's first request, H5):
    /// a system prompt or tools that differ make the provider's cache cold.
    check: bool,
}

impl PrefixLog {
    /// When the inputs of `req` (built with this AGENTS.md text and repo map) differ from
    /// the last logged: the `PrefixApplied` record to log and the texts its blobs name.
    pub fn due(
        &self,
        agents_md: Option<&str>,
        repo_map: Option<&str>,
        req: &CanonicalRequest,
    ) -> Option<(SessionRecord, Vec<String>)> {
        let system = match req.system.first() {
            Some(Block::Text(t)) => t.as_str(),
            _ => "",
        };
        let tools = serde_json::to_string(&req.tools).unwrap_or_default();
        let tools_ref = blob_ref(tools.as_bytes());
        let rec = SessionRecord::PrefixApplied {
            agents_md: agents_md.map(|t| blob_ref(t.as_bytes())),
            repo_map: repo_map.map(|t| blob_ref(t.as_bytes())),
            system_prompt_hash: hex_hash(system.as_bytes()),
            tools_hash: tools_ref.as_str().to_string(),
            tools: Some(tools_ref),
        };
        if self.last.as_ref() == Some(&rec) {
            return None;
        }
        let mut texts: Vec<String> = [agents_md, repo_map]
            .into_iter()
            .flatten()
            .map(str::to_string)
            .collect();
        texts.push(tools);
        Some((rec, texts))
    }

    /// `rec` (from [`due`](Self::due)) is in the log.
    pub fn logged(&mut self, rec: SessionRecord) {
        self.last = Some(rec);
    }

    /// Compare the next request's prefix with the last logged one ([`changed`]).
    ///
    /// [`changed`]: Self::changed
    pub fn check_next(&mut self) {
        self.check = true;
    }

    /// What of the last logged prefix differs in `due` (a record [`due`](Self::due) gave
    /// for the next request), if a check is armed; disarms it.
    fn changed(&mut self, due: Option<&SessionRecord>) -> Option<&'static str> {
        if !std::mem::take(&mut self.check) {
            return None;
        }
        let (
            Some(SessionRecord::PrefixApplied {
                system_prompt_hash: system,
                tools_hash: tools,
                ..
            }),
            Some(SessionRecord::PrefixApplied {
                system_prompt_hash: logged_system,
                tools_hash: logged_tools,
                ..
            }),
        ) = (due, &self.last)
        else {
            return None;
        };
        match (system != logged_system, tools != logged_tools) {
            (true, true) => Some("system prompt and tools differ"),
            (true, false) => Some("system prompt differs"),
            (false, true) => Some("tools differ"),
            (false, false) => None,
        }
    }
}

/// The hash of each part of `req` (H6). `pinned` (the rendered pinned block) and
/// `history` are the context's that `req` was assembled from: in the request the pinned
/// block shares the first message.
fn block_hashes(pinned: &Option<String>, history: &[Turn], req: &CanonicalRequest) -> BlockHashes {
    let tail = req.messages.last().filter(|_| req.volatile_tail);
    BlockHashes {
        stable: json_hash(&(&req.system, &req.tools)),
        pinned: json_hash(pinned),
        history: json_hash(history),
        tail: json_hash(&tail),
    }
}

fn hex_hash(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

/// blake3 hex of the JSON serialization (streamed into the hasher): canonical for these
/// types (structs and vectors in a fixed order, maps sorted).
fn json_hash<T: serde::Serialize + ?Sized>(value: &T) -> String {
    let mut hasher = blake3::Hasher::new();
    let _ = serde_json::to_writer(&mut hasher, value);
    hasher.finalize().to_hex().to_string()
}

/// The blob reference `bytes` would have: blake3 hex.
fn blob_ref(bytes: &[u8]) -> BlobRef {
    BlobRef::parse(blake3::hash(bytes).to_hex().as_str())
        .expect("a blake3 hex digest is a blob reference")
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

#[cfg(test)]
mod tests {
    use super::*;

    fn request(system: &str, tool: &str) -> CanonicalRequest {
        CanonicalRequest {
            model: ModelId::from("mock/m"),
            system: vec![Block::Text(system.into())],
            tools: vec![ToolSpec {
                name: tool.into(),
                description: String::new(),
                schema: serde_json::json!({}),
            }],
            messages: vec![],
            max_output: 1,
            reasoning: None,
            cache_breakpoints: vec![],
            volatile_tail: false,
        }
    }

    /// What `log_request` does when the write succeeds: take the due record and mark it.
    fn log_if_due(
        log: &mut PrefixLog,
        agents: Option<&str>,
        map: Option<&str>,
        req: &CanonicalRequest,
    ) -> Option<SessionRecord> {
        let (rec, _) = log.due(agents, map, req)?;
        log.logged(rec.clone());
        Some(rec)
    }

    #[test]
    fn prefix_applied_is_due_only_when_an_input_changes() {
        let mut log = PrefixLog::default();
        let req = request("sys", "read");
        let (first, texts) = log.due(Some("agents"), None, &req).expect("first");
        let tools_json = serde_json::to_string(&req.tools).unwrap();
        assert_eq!(texts, ["agents".to_string(), tools_json.clone()]);
        let SessionRecord::PrefixApplied {
            agents_md,
            repo_map,
            system_prompt_hash,
            tools_hash,
            tools,
        } = &first
        else {
            panic!("{first:?}")
        };
        assert_eq!(agents_md.as_ref(), Some(&blob_ref(b"agents")));
        assert_eq!(*repo_map, None);
        assert_eq!(*system_prompt_hash, blake3::hash(b"sys").to_hex().as_str());
        assert_eq!(*tools_hash, json_hash(&req.tools));
        assert_eq!(tools.as_ref(), Some(&blob_ref(tools_json.as_bytes())));
        log.logged(first.clone());
        assert!(log.due(Some("agents"), None, &req).is_none());
        // Each input on its own makes it due again.
        let sys2 = request("sys 2", "read");
        assert!(log_if_due(&mut log, Some("agents"), None, &sys2).is_some());
        assert!(log_if_due(&mut log, Some("other"), None, &req).is_some());
        assert!(log_if_due(&mut log, Some("other"), Some("map"), &req).is_some());
        let write = request("sys", "write");
        assert!(log_if_due(&mut log, Some("other"), Some("map"), &write).is_some());
        assert!(log_if_due(&mut log, Some("other"), Some("map"), &write).is_none());
        // Back to the first inputs: logged again, identical to the first record.
        assert_eq!(
            log_if_due(&mut log, Some("agents"), None, &req),
            Some(first)
        );
    }

    /// Re-review N4: a cancel stops the start-up wait at once and keeps the receiver, so
    /// the next turn's first request still takes the repo map.
    #[tokio::test]
    async fn a_cancelled_startup_wait_returns_at_once_and_keeps_the_prefix_for_later() {
        let (tx, rx) = oneshot::channel();
        let mut slot = Some(rx);
        let cancel = CancellationToken::new();
        cancel.cancel();
        let t0 = std::time::Instant::now();
        assert!(
            take_prefix(&mut slot, Duration::from_secs(30), &cancel)
                .await
                .is_none()
        );
        assert!(
            t0.elapsed() < Duration::from_millis(20),
            "{:?}",
            t0.elapsed()
        );
        assert!(slot.is_some(), "the receiver is kept");
        tx.send(Prefix {
            repo_map: Some("MAP".into()),
            memory_live: false,
        })
        .map_err(drop)
        .unwrap();
        let got = take_prefix(
            &mut slot,
            Duration::from_secs(30),
            &CancellationToken::new(),
        )
        .await
        .expect("not cancelled")
        .expect("ready");
        assert_eq!(got.repo_map.as_deref(), Some("MAP"));
        assert!(slot.is_none());
    }

    #[tokio::test]
    async fn a_startup_wait_that_runs_out_says_so() {
        let (_tx, rx) = oneshot::channel::<Prefix>();
        let mut slot = Some(rx);
        let never = CancellationToken::new();
        let got = take_prefix(&mut slot, Duration::from_millis(10), &never).await;
        assert!(matches!(got, Some(None)));
        assert!(slot.is_none());
    }

    #[test]
    fn a_prefix_record_whose_write_failed_is_due_again() {
        let mut log = PrefixLog::default();
        let req = request("sys", "read");
        // The write failed: `logged` was never called.
        let (rec, _) = log.due(None, None, &req).unwrap();
        assert_eq!(log.due(None, None, &req).map(|(r, _)| r), Some(rec.clone()));
        log.logged(rec);
        assert!(log.due(None, None, &req).is_none());
    }
}
