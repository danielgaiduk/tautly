//! The cost ledger (spec 4.6): one SQLite row per provider response, across sessions and
//! processes, at `<data home>/ledger.sqlite`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, params};
use tautly_provider::{ModelId, RawUsage};

use crate::protocol::Event;
use crate::session::EventSink;

/// The ledger's file name in the data home.
pub const LEDGER_FILE: &str = "ledger.sqlite";

/// How long a write waits for another writer (another session or process).
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// `PRAGMA user_version` of the schema below. Changes are additive only (new columns with
/// defaults); a build still refuses a file whose version is newer than it knows
/// ([`LedgerError::Newer`]), so it never writes rows a newer schema might misread.
const SCHEMA_VERSION: i64 = 2;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS usage (
    id INTEGER PRIMARY KEY,
    at_ms INTEGER NOT NULL,
    session TEXT NOT NULL,
    model TEXT NOT NULL,
    input INTEGER NOT NULL,
    output INTEGER NOT NULL,
    cache_read INTEGER NOT NULL,
    cache_write INTEGER NOT NULL,
    cost_usd REAL,
    estimated INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS usage_at ON usage(at_ms);
";

/// Version 1 to 2: whether the engine estimated the row.
const V2: &str = "ALTER TABLE usage ADD COLUMN estimated INTEGER NOT NULL DEFAULT 0;";

#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    #[error("cost ledger: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("cost ledger: {0}")]
    Io(#[from] std::io::Error),
    #[error(
        "cost ledger: written by a newer tautly (schema {found}, this one knows {SCHEMA_VERSION}); left as it is"
    )]
    Newer { found: i64 },
}

impl LedgerError {
    /// The file is not a database, or a damaged one.
    fn is_corrupt(&self) -> bool {
        matches!(
            self,
            LedgerError::Db(rusqlite::Error::SqliteFailure(e, _))
                if matches!(
                    e.code,
                    rusqlite::ErrorCode::NotADatabase | rusqlite::ErrorCode::DatabaseCorrupt
                )
        )
    }
}

pub type Result<T> = std::result::Result<T, LedgerError>;

/// Usage summed over ledger rows.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Totals {
    /// Rows (provider responses).
    pub requests: u64,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    /// Cost of the priced rows.
    pub usd: f64,
    /// Rows without a known price (not in `usd`).
    pub unpriced: u64,
    /// Rows the engine estimated (a response that broke off or was cancelled).
    pub estimated: u64,
}

impl Totals {
    /// Every token billed: input, output and both cache kinds.
    pub fn tokens(&self) -> u64 {
        self.input + self.output + self.cache_read + self.cache_write
    }
}

/// A handle on the ledger file. Several handles (threads, processes) may write at once.
pub struct Ledger {
    conn: Mutex<rusqlite::Connection>,
}

impl Ledger {
    /// Open (creating it and its directory if needed) in WAL mode with a 5 s busy timeout.
    /// The file is readable by its owner only. A ledger from a newer schema is refused.
    pub fn open(path: &Path) -> Result<Ledger> {
        Ok(Self::open_noting(path)?.0)
    }

    /// [`open`](Self::open) for the engine: a file that is not a database (or is corrupt)
    /// is moved aside as `<name>.corrupt-<unix secs>` and a new one created. Returns the
    /// ledger and what the user should be told (the move, WAL mode unavailable).
    pub fn open_recovering(path: &Path) -> Result<(Ledger, Vec<String>)> {
        match Self::open_noting(path) {
            Err(e) if e.is_corrupt() => {
                let secs = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs());
                let mut aside = path.as_os_str().to_owned();
                aside.push(format!(".corrupt-{secs}"));
                std::fs::rename(path, &aside)?;
                // Journal files of the damaged database would confuse the new one.
                for ext in ["-wal", "-shm"] {
                    let mut f = path.as_os_str().to_owned();
                    f.push(ext);
                    let _ = std::fs::remove_file(&f);
                }
                let (ledger, mut notes) = Self::open_noting(path)?;
                notes.insert(
                    0,
                    format!(
                        "the cost ledger was corrupt ({e}); it was moved to {} and a new one \
                         started",
                        Path::new(&aside).display()
                    ),
                );
                Ok((ledger, notes))
            }
            other => other,
        }
    }

    fn open_noting(path: &Path) -> Result<(Ledger, Vec<String>)> {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        // Owner-only before SQLite creates it; its -wal and -shm files take the same mode.
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(path)?;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        let conn = Connection::open(path)?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version > SCHEMA_VERSION {
            return Err(LedgerError::Newer { found: version });
        }
        let mut notes = Vec::new();
        // Switching to WAL needs the file to itself and does not wait for a busy handler:
        // when another process is creating the ledger at the same moment, retry.
        let mode: String = until_not_busy(|| {
            conn.pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get(0))
        })?;
        if !mode.eq_ignore_ascii_case("wal") {
            notes.push(format!(
                "the cost ledger could not use WAL mode (the file system gave `{mode}`); \
                 sessions writing at once may wait on each other"
            ));
        }
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        if version < SCHEMA_VERSION {
            until_not_busy(|| migrate(&conn))?;
        }
        Ok((
            Ledger {
                conn: Mutex::new(conn),
            },
            notes,
        ))
    }

    pub fn record(
        &self,
        session: &str,
        model: &ModelId,
        usage: &RawUsage,
        cost: Option<f64>,
    ) -> Result<()> {
        self.insert(session, model, usage, cost, false)
    }

    fn insert(
        &self,
        session: &str,
        model: &ModelId,
        usage: &RawUsage,
        cost: Option<f64>,
        estimated: bool,
    ) -> Result<()> {
        let n = |v: u64| i64::try_from(v).unwrap_or(i64::MAX);
        self.conn().execute(
            "INSERT INTO usage (at_ms, session, model, input, output, cache_read, cache_write, \
             cost_usd, estimated) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                unix_ms(SystemTime::now()),
                session,
                model.as_str(),
                n(usage.input),
                n(usage.output),
                n(usage.cache_read),
                n(usage.cache_write),
                cost.filter(|c| c.is_finite()),
                estimated,
            ],
        )?;
        Ok(())
    }

    /// [`record`](Self::record) for usage the engine estimated.
    /// [`record`](Self::record) for usage the engine estimated.
    pub fn record_estimated(
        &self,
        session: &str,
        model: &ModelId,
        usage: &RawUsage,
        cost: Option<f64>,
    ) -> Result<()> {
        self.insert(session, model, usage, cost, true)
    }

    /// Totals of the rows recorded at or after `since`.
    pub fn totals(&self, since: SystemTime) -> Result<Totals> {
        self.sum("at_ms >= ?1", params![unix_ms(since)])
    }

    /// Totals of every row of session `session` (discarded and estimated attempts
    /// included): what a resumed session has spent so far.
    pub fn session_totals(&self, session: &str) -> Result<Totals> {
        self.sum("session = ?1", params![session])
    }

    /// Totals of the rows matching `filter` (an SQL condition on `usage`, with `args`).
    fn sum(&self, filter: &str, args: impl rusqlite::Params) -> Result<Totals> {
        let n = |v: i64| u64::try_from(v).unwrap_or(0);
        let sql = format!(
            "SELECT COUNT(*), COALESCE(SUM(input), 0), COALESCE(SUM(output), 0), \
             COALESCE(SUM(cache_read), 0), COALESCE(SUM(cache_write), 0), \
             COALESCE(SUM(cost_usd), 0.0), COUNT(*) - COUNT(cost_usd), \
             COALESCE(SUM(estimated), 0) \
             FROM usage WHERE {filter}"
        );
        let totals = self.conn().query_row(&sql, args, |r| {
            Ok(Totals {
                requests: n(r.get(0)?),
                input: n(r.get(1)?),
                output: n(r.get(2)?),
                cache_read: n(r.get(3)?),
                cache_write: n(r.get(4)?),
                usd: r.get(5)?,
                unpriced: n(r.get(6)?),
                estimated: n(r.get(7)?),
            })
        })?;
        Ok(totals)
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Bring the schema to [`SCHEMA_VERSION`] in one write transaction (the version is read
/// again inside it, so two processes never migrate twice).
fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let step = || -> rusqlite::Result<()> {
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        match version {
            0 => conn.execute_batch(SCHEMA)?,
            1 => conn.execute_batch(V2)?,
            _ => {}
        }
        if version < SCHEMA_VERSION {
            conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        }
        Ok(())
    };
    match step() {
        Ok(()) => conn.execute_batch("COMMIT"),
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

/// Run `f` again while it fails with `SQLITE_BUSY`, for up to [`BUSY_TIMEOUT`].
fn until_not_busy<T>(mut f: impl FnMut() -> rusqlite::Result<T>) -> rusqlite::Result<T> {
    let end = std::time::Instant::now() + BUSY_TIMEOUT;
    loop {
        match f() {
            Err(rusqlite::Error::SqliteFailure(e, _))
                if e.code == rusqlite::ErrorCode::DatabaseBusy
                    && std::time::Instant::now() < end =>
            {
                std::thread::sleep(Duration::from_millis(5));
            }
            other => return other,
        }
    }
}

fn unix_ms(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// Rows queued for the writer; past this a row is dropped (and said once).
const PENDING_ROWS: usize = 1000;

/// What the writer is sent.
enum Msg {
    Row(Row),
    /// Answer once every row queued before it is written (or failed).
    Flush(tokio::sync::oneshot::Sender<()>),
}

/// One row on its way to the ledger.
pub(crate) struct Row {
    pub session: String,
    pub model: ModelId,
    pub usage: RawUsage,
    pub cost: Option<f64>,
    pub estimated: bool,
}

/// A session's way to the ledger. Rows go to a background writer that opens the ledger
/// (off the start path) and writes them in order, so recording never waits on the file:
/// not at start, not on the cancel path, not while another process holds the lock. A
/// failed write loses only that row; the writer goes on. At shutdown
/// [`flush`](CostLedger::flush) waits a bounded time for the queue to drain.
pub(crate) struct CostLedger {
    tx: std::sync::mpsc::SyncSender<Msg>,
    /// Taken by [`CostLedger::start`].
    pending: Mutex<Option<(std::sync::mpsc::Receiver<Msg>, PathBuf)>>,
    /// Where the writer's warnings go; emptied by [`flush`](CostLedger::flush) (and on
    /// drop), so a writer stuck on the file never holds the event stream open.
    sink: Arc<Mutex<Option<EventSink>>>,
}

impl Drop for CostLedger {
    fn drop(&mut self) {
        lock(&self.sink).take();
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl CostLedger {
    pub fn new(path: PathBuf) -> Self {
        let (tx, rx) = std::sync::mpsc::sync_channel(PENDING_ROWS);
        CostLedger {
            tx,
            pending: Mutex::new(Some((rx, path))),
            sink: Arc::default(),
        }
    }

    /// Start the writer (once; inside a Tokio runtime). What the user should be told (the
    /// ledger moved aside, unavailable, rows lost) goes to `events`.
    pub fn start(&self, events: EventSink) {
        let Some((rx, path)) = lock(&self.pending).take() else {
            return;
        };
        *lock(&self.sink) = Some(events);
        let sink = Arc::clone(&self.sink);
        tokio::task::spawn_blocking(move || {
            let warn = |message: String| {
                if let Some(events) = lock(&sink).as_ref() {
                    events.try_emit(Event::Warning { message });
                }
            };
            let ledger = match Ledger::open_recovering(&path) {
                Ok((ledger, notes)) => {
                    notes.into_iter().for_each(warn);
                    ledger
                }
                Err(e) => {
                    warn(format!(
                        "the cost ledger is unavailable ({e}); this session's usage is not \
                         recorded"
                    ));
                    return;
                }
            };
            let mut reported = false;
            // Ends when the session (the sender) is gone and every queued row is written.
            while let Ok(msg) = rx.recv() {
                let row = match msg {
                    Msg::Row(row) => row,
                    Msg::Flush(done) => {
                        let _ = done.send(());
                        continue;
                    }
                };
                if let Err(e) = write(&ledger, &row)
                    && !std::mem::replace(&mut reported, true)
                {
                    warn(format!(
                        "a usage row could not be written to the cost ledger ({e}); later \
                         rows are still recorded, and further failures are not reported"
                    ));
                }
            }
        });
    }

    /// Queue one row; never waits. An error says it was dropped (the writer is too far
    /// behind). After the ledger turned out unavailable, rows are dropped silently.
    pub fn record(&self, row: Row) -> std::result::Result<(), String> {
        match self.tx.try_send(Msg::Row(row)) {
            Ok(()) | Err(std::sync::mpsc::TrySendError::Disconnected(_)) => Ok(()),
            Err(std::sync::mpsc::TrySendError::Full(_)) => Err(format!(
                "{PENDING_ROWS} rows are waiting for the cost ledger, so this one was dropped"
            )),
        }
    }
}

impl CostLedger {
    /// Wait up to `within` for the rows queued so far to be written; then the writer's
    /// warnings stop (it goes on writing in the background if it is still stuck).
    pub async fn flush(&self, within: Duration) {
        let started = lock(&self.pending).is_none();
        if started {
            let deadline = tokio::time::Instant::now() + within;
            let (done, written) = tokio::sync::oneshot::channel();
            let mut msg = Msg::Flush(done);
            // Behind the queued rows; a full queue is retried until the deadline.
            let queued = loop {
                match self.tx.try_send(msg) {
                    Ok(()) => break true,
                    Err(std::sync::mpsc::TrySendError::Full(m))
                        if tokio::time::Instant::now() < deadline =>
                    {
                        msg = m;
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    Err(_) => break false,
                }
            };
            if queued {
                let _ = tokio::time::timeout_at(deadline, written).await;
            }
        }
        lock(&self.sink).take();
    }
}

fn write(ledger: &Ledger, r: &Row) -> Result<()> {
    if r.estimated {
        ledger.record_estimated(&r.session, &r.model, &r.usage, r.cost)
    } else {
        ledger.record(&r.session, &r.model, &r.usage, r.cost)
    }
}
