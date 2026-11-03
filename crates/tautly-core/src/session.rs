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
use tautly_provider::{Block, CanonicalRequest, ModelId, ModelInfo, Provider, ToolSpec};
use tautly_sandbox::{Policy, Sandbox, cleanup_spills};
use tautly_session::{BlobRef, SessionWriter};
use tautly_tools::{ProcessGroups, ReadTracker, Tool};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::attempts::AttemptLedger;
use crate::budget::Budgets;
use crate::ledger::Ledger;
use crate::protocol::{Command, ErrorKind, Event};
use crate::record::{BlockHashes, SessionRecord};
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
    /// The cost ledger file; opened at [`Session::init`].
    pub cost_ledger_path: PathBuf,
    /// None when it could not be opened (said once).
    pub cost_ledger: Option<Arc<Ledger>>,
    /// A ledger problem was reported; later ones are not.
    pub cost_ledger_reported: bool,
    /// The process groups this session's commands started (killed at shutdown).
    pub groups: ProcessGroups,
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
    /// goes first when the stable-prefix inputs differ from the last one logged (H5).
    pub async fn log_request(&mut self, req: &CanonicalRequest, step: u32) {
        let bytes = serde_json::to_vec(req).unwrap_or_default();
        let prefix = self.prefix_log.changed(
            self.context.agents_md.as_deref(),
            self.context.repo_map.as_deref(),
            req,
        );
        let rec = SessionRecord::Request {
            hash: blob_ref(&bytes),
            turn: self.user_turn,
            step,
            model: req.model.clone(),
            blocks: block_hashes(&self.context, req),
        };
        let path = self.last_request_path.clone();
        self.with_log(move |l| {
            if let Some((prefix, texts)) = prefix {
                for t in &texts {
                    l.blob(t.as_bytes())?;
                }
                l.append(&prefix)?;
            }
            write_private(&path, &bytes);
            l.append(&rec)
        })
        .await;
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

/// The stable-prefix inputs last logged, to log `PrefixApplied` again only when they
/// change (spec 6.5 H5).
#[derive(Default)]
pub(crate) struct PrefixLog {
    last: Option<SessionRecord>,
}

impl PrefixLog {
    /// When the inputs of `req` (built with this AGENTS.md text and repo map) differ from
    /// the last logged: the `PrefixApplied` record to log and the texts its blobs name.
    pub fn changed(
        &mut self,
        agents_md: Option<&str>,
        repo_map: Option<&str>,
        req: &CanonicalRequest,
    ) -> Option<(SessionRecord, Vec<String>)> {
        let system = match req.system.first() {
            Some(Block::Text(t)) => t.as_str(),
            _ => "",
        };
        let rec = SessionRecord::PrefixApplied {
            agents_md: agents_md.map(|t| blob_ref(t.as_bytes())),
            repo_map: repo_map.map(|t| blob_ref(t.as_bytes())),
            system_prompt_hash: hex_hash(system.as_bytes()),
            tools_hash: json_hash(&req.tools),
        };
        if self.last.as_ref() == Some(&rec) {
            return None;
        }
        self.last = Some(rec.clone());
        let texts = [agents_md, repo_map].into_iter().flatten();
        Some((rec, texts.map(str::to_string).collect()))
    }
}

/// The hash of each part of `req` (H6). `pinned` and `history` come from `context`, which
/// `req` was assembled from: in the request the pinned block shares the first message.
fn block_hashes(context: &Context, req: &CanonicalRequest) -> BlockHashes {
    let tail = req.messages.last().filter(|_| req.volatile_tail);
    BlockHashes {
        stable: json_hash(&(&req.system, &req.tools)),
        pinned: json_hash(&pinned::render(&context.pinned)),
        history: json_hash(&context.history),
        tail: json_hash(&tail),
    }
}

fn hex_hash(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

/// blake3 hex of the JSON serialization: canonical for these types (structs and vectors
/// in a fixed order, maps sorted).
fn json_hash<T: serde::Serialize + ?Sized>(value: &T) -> String {
    hex_hash(&serde_json::to_vec(value).unwrap_or_default())
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

    #[test]
    fn prefix_applied_is_due_only_when_an_input_changes() {
        let mut log = PrefixLog::default();
        let req = request("sys", "read");
        let (first, texts) = log.changed(Some("agents"), None, &req).expect("first");
        assert_eq!(texts, ["agents"]);
        let SessionRecord::PrefixApplied {
            agents_md,
            repo_map,
            system_prompt_hash,
            tools_hash,
        } = &first
        else {
            panic!("{first:?}")
        };
        assert_eq!(agents_md.as_ref(), Some(&blob_ref(b"agents")));
        assert_eq!(*repo_map, None);
        assert_eq!(*system_prompt_hash, blake3::hash(b"sys").to_hex().as_str());
        assert_eq!(tools_hash.len(), 64);
        assert!(log.changed(Some("agents"), None, &req).is_none());
        // Each input on its own makes it due again.
        assert!(
            log.changed(Some("agents"), None, &request("sys 2", "read"))
                .is_some()
        );
        assert!(log.changed(Some("other"), None, &req).is_some());
        assert!(log.changed(Some("other"), Some("map"), &req).is_some());
        assert!(
            log.changed(Some("other"), Some("map"), &request("sys", "write"))
                .is_some()
        );
        assert!(
            log.changed(Some("other"), Some("map"), &request("sys", "write"))
                .is_none()
        );
        // Back to the first inputs: logged again, identical to the first record.
        let (again, _) = log.changed(Some("agents"), None, &req).unwrap();
        assert_eq!(again, first);
    }
}
