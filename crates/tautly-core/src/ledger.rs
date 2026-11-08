//! The cost ledger (spec 4.6): one SQLite row per provider response, across sessions and
//! processes, at `<data home>/ledger.sqlite`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, params};
use serde::Serialize;
use tautly_provider::{ModelId, RawUsage};

use crate::protocol::Event;
use crate::receipt::Savings;
use crate::session::EventSink;

/// The ledger's file name in the data home.
pub const LEDGER_FILE: &str = "ledger.sqlite";

/// How long a write waits for another writer (another session or process).
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// `PRAGMA user_version` of the schema below. Changes are additive only (new columns with
/// defaults, new indexes); a build still refuses a file whose version is newer than it
/// knows ([`LedgerError::Newer`]), so it never writes rows a newer schema might misread.
const SCHEMA_VERSION: i64 = 4;

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
CREATE INDEX IF NOT EXISTS usage_session ON usage(session);
";

/// Version 1 to 2: whether the engine estimated the row.
const V2: &str = "ALTER TABLE usage ADD COLUMN estimated INTEGER NOT NULL DEFAULT 0;";
/// Version 2 to 3: a session's totals (every resume, every row of a session list) without
/// a scan of the whole table.
const V3: &str = "CREATE INDEX IF NOT EXISTS usage_session ON usage(session);";
/// Version 3 to 4 (and part of a new ledger): one row per turn receipt (spec 7.11.1).
/// `at` is unix milliseconds, `root` the root's bytes; the `saved_*` columns are the
/// receipt's token estimates. `cost_partial`: some usage had no known price, so
/// `cost_usd` covers the rest (added to schema 4 before its release; [`V4_COST_PARTIAL`]
/// adds it to a file an earlier build of schema 4 made).
const V4: &str = "
CREATE TABLE IF NOT EXISTS receipts (
    id INTEGER PRIMARY KEY,
    session TEXT NOT NULL,
    project TEXT NOT NULL,
    root BLOB NOT NULL,
    turn INTEGER NOT NULL,
    model TEXT NOT NULL,
    at INTEGER NOT NULL,
    tokens_in INTEGER NOT NULL,
    tokens_out INTEGER NOT NULL,
    cache_read INTEGER NOT NULL,
    cache_write INTEGER NOT NULL,
    side_tokens INTEGER NOT NULL,
    cost_usd REAL,
    wall_ms INTEGER NOT NULL,
    steps INTEGER NOT NULL,
    tool_calls INTEGER NOT NULL,
    saved_index INTEGER NOT NULL,
    saved_diff_reads INTEGER NOT NULL,
    saved_context_gc INTEGER NOT NULL,
    saved_filters INTEGER NOT NULL,
    cache_usd REAL,
    total_usd REAL,
    estimated INTEGER NOT NULL DEFAULT 0,
    cost_partial INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS receipts_session ON receipts(session);
CREATE INDEX IF NOT EXISTS receipts_at ON receipts(at);
";

/// A schema-4 file made before `receipts.cost_partial` existed gets it (cp5 review C M1).
const V4_COST_PARTIAL: &str =
    "ALTER TABLE receipts ADD COLUMN cost_partial INTEGER NOT NULL DEFAULT 0;";

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
    #[error("cost ledger: only a turn receipt can be stored as one")]
    NotAReceipt,
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

/// The [`GroupBy::Project`] key of usage rows whose session left no receipt.
pub const NO_PROJECT: &str = "-";

/// How [`Ledger::stats`] groups its rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupBy {
    /// The UTC day, `YYYY-MM-DD`.
    Day,
    /// The model id (usage rows: the model billed; receipt rows: the turn's main model).
    Model,
    /// The project id (spec 6.5); usage rows find theirs through their session's receipts.
    Project,
}

/// One group of [`Ledger::stats`]. Tokens, cost and the cache hit ratio come from usage
/// rows; turns, savings and wall time from receipt rows.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StatsRow {
    pub key: String,
    pub turns: u64,
    /// Every token billed: input, output and both cache kinds.
    pub tokens: u64,
    /// Cost of the priced usage rows; `None` when none was priced.
    pub cost_usd: Option<f64>,
    /// Usage rows without a known price (not in `cost_usd`).
    pub unpriced: u64,
    /// Usage rows the engine estimated.
    pub estimated: u64,
    /// The receipts' savings estimates, in tokens.
    pub saved_tokens: u64,
    /// The receipts' `total_usd`; `None` when none had one.
    pub saved_usd: Option<f64>,
    pub wall_ms: u64,
    /// `cache_read / (input + cache_read)` of the usage rows; `None` when both are 0.
    pub cache_hit_ratio: Option<f64>,
}

/// A receipt as the ledger stores it.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredReceipt {
    pub session: String,
    pub project: String,
    pub root: PathBuf,
    pub turn: u32,
    pub model: String,
    pub at: SystemTime,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub side_tokens: u64,
    /// The priced usage's cost (none: nothing was priced).
    pub cost_usd: Option<f64>,
    /// Some usage had no known price: `cost_usd` covers the rest.
    pub cost_partial: bool,
    pub wall_ms: u64,
    pub steps: u32,
    pub tool_calls: u32,
    pub saved: Savings,
    pub estimated: bool,
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
        if version < SCHEMA_VERSION || !has_cost_partial(&conn)? {
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

    /// Store a [`Event::TurnReceipt`] of session `session`, in project `project` (its id)
    /// at `root`.
    pub fn record_receipt(
        &self,
        session: &str,
        project: &str,
        root: &Path,
        receipt: &Event,
    ) -> Result<()> {
        use std::os::unix::ffi::OsStrExt;
        let Event::TurnReceipt {
            turn,
            model,
            tokens_in,
            tokens_out,
            cache_read,
            cache_write,
            side_tokens,
            cost_usd,
            cost_partial,
            wall_ms,
            steps,
            tool_calls,
            saved,
            estimated,
            ..
        } = receipt
        else {
            return Err(LedgerError::NotAReceipt);
        };
        let n = |v: u64| i64::try_from(v).unwrap_or(i64::MAX);
        let finite = |v: Option<f64>| v.filter(|c| c.is_finite());
        self.conn().execute(
            "INSERT INTO receipts (session, project, root, turn, model, at, tokens_in, \
             tokens_out, cache_read, cache_write, side_tokens, cost_usd, wall_ms, steps, \
             tool_calls, saved_index, saved_diff_reads, saved_context_gc, saved_filters, \
             cache_usd, total_usd, estimated, cost_partial) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, \
             ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23)",
            params![
                session,
                project,
                root.as_os_str().as_bytes(),
                turn,
                model.as_str(),
                unix_ms(SystemTime::now()),
                n(*tokens_in),
                n(*tokens_out),
                n(*cache_read),
                n(*cache_write),
                n(*side_tokens),
                finite(*cost_usd),
                n(*wall_ms),
                steps,
                tool_calls,
                n(saved.index),
                n(saved.diff_reads),
                n(saved.context_gc),
                n(saved.filters),
                finite(saved.cache_usd),
                finite(saved.total_usd),
                estimated,
                cost_partial,
            ],
        )?;
        Ok(())
    }

    /// The rows recorded at or after `since` (all of them: `None`), grouped `by`, in key
    /// order. A group may have usage rows and no receipt (side models, sessions from before
    /// receipts) or, outside `since`, the other way round.
    pub fn stats(&self, since: Option<SystemTime>, by: GroupBy) -> Result<Vec<StatsRow>> {
        let since = since.map_or(i64::MIN, unix_ms);
        let day = |col: &str| format!("strftime('%Y-%m-%d', {col} / 1000, 'unixepoch')");
        let (usage_key, usage_from, receipt_key) = match by {
            GroupBy::Day => (day("u.at_ms"), "usage u".to_string(), day("at")),
            GroupBy::Model => ("u.model".into(), "usage u".into(), "model".into()),
            GroupBy::Project => (
                format!("COALESCE(r.project, '{NO_PROJECT}')"),
                "usage u LEFT JOIN (SELECT session, MIN(project) AS project FROM receipts \
                 GROUP BY session) r ON r.session = u.session"
                    .into(),
                "project".into(),
            ),
        };
        let n = |v: i64| u64::try_from(v).unwrap_or(0);
        let mut rows: std::collections::BTreeMap<String, StatsRow> = Default::default();
        let empty = |key: &str| StatsRow {
            key: key.to_string(),
            turns: 0,
            tokens: 0,
            cost_usd: None,
            unpriced: 0,
            estimated: 0,
            saved_tokens: 0,
            saved_usd: None,
            wall_ms: 0,
            cache_hit_ratio: None,
        };
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {usage_key} AS k, \
             COALESCE(SUM(u.input + u.output + u.cache_read + u.cache_write), 0), \
             SUM(u.cost_usd), COUNT(*) - COUNT(u.cost_usd), COALESCE(SUM(u.estimated), 0), \
             COALESCE(SUM(u.input), 0), COALESCE(SUM(u.cache_read), 0) \
             FROM {usage_from} WHERE u.at_ms >= ?1 GROUP BY k"
        ))?;
        let usage = stmt.query_map(params![since], |r| {
            Ok((
                r.get::<_, String>(0)?,
                n(r.get(1)?),
                r.get::<_, Option<f64>>(2)?,
                n(r.get(3)?),
                n(r.get(4)?),
                n(r.get(5)?),
                n(r.get(6)?),
            ))
        })?;
        for row in usage {
            let (key, tokens, cost, unpriced, estimated, input, cache_read) = row?;
            let seen = input.saturating_add(cache_read);
            let entry = rows.entry(key.clone()).or_insert_with(|| empty(&key));
            entry.tokens = tokens;
            entry.cost_usd = cost;
            entry.unpriced = unpriced;
            entry.estimated = estimated;
            entry.cache_hit_ratio = (seen > 0).then(|| cache_read as f64 / seen as f64);
        }
        let mut stmt = conn.prepare(&format!(
            "SELECT {receipt_key} AS k, COUNT(*), \
             COALESCE(SUM(saved_index + saved_diff_reads + saved_context_gc + saved_filters), 0), \
             SUM(total_usd), COALESCE(SUM(wall_ms), 0) \
             FROM receipts WHERE at >= ?1 GROUP BY k"
        ))?;
        let receipts = stmt.query_map(params![since], |r| {
            Ok((
                r.get::<_, String>(0)?,
                n(r.get(1)?),
                n(r.get(2)?),
                r.get::<_, Option<f64>>(3)?,
                n(r.get(4)?),
            ))
        })?;
        for row in receipts {
            let (key, turns, saved_tokens, saved_usd, wall_ms) = row?;
            let entry = rows.entry(key.clone()).or_insert_with(|| empty(&key));
            entry.turns = turns;
            entry.saved_tokens = saved_tokens;
            entry.saved_usd = saved_usd;
            entry.wall_ms = wall_ms;
        }
        Ok(rows.into_values().collect())
    }

    /// Every receipt of session `session`, in turn order.
    pub fn session_receipts(&self, session: &str) -> Result<Vec<StoredReceipt>> {
        use std::os::unix::ffi::OsStrExt;
        let n = |v: i64| u64::try_from(v).unwrap_or(0);
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT session, project, root, turn, model, at, tokens_in, tokens_out, \
             cache_read, cache_write, side_tokens, cost_usd, wall_ms, steps, tool_calls, \
             saved_index, saved_diff_reads, saved_context_gc, saved_filters, cache_usd, \
             total_usd, estimated, cost_partial FROM receipts WHERE session = ?1 ORDER BY \
             turn, id",
        )?;
        let rows = stmt.query_map(params![session], |r| {
            let cache_read = n(r.get(8)?);
            Ok(StoredReceipt {
                session: r.get(0)?,
                project: r.get(1)?,
                root: PathBuf::from(std::ffi::OsStr::from_bytes(&r.get::<_, Vec<u8>>(2)?)),
                turn: r.get(3)?,
                model: r.get(4)?,
                at: UNIX_EPOCH + Duration::from_millis(n(r.get(5)?)),
                tokens_in: n(r.get(6)?),
                tokens_out: n(r.get(7)?),
                cache_read,
                cache_write: n(r.get(9)?),
                side_tokens: n(r.get(10)?),
                cost_usd: r.get(11)?,
                wall_ms: n(r.get(12)?),
                steps: r.get(13)?,
                tool_calls: r.get(14)?,
                saved: Savings {
                    index: n(r.get(15)?),
                    diff_reads: n(r.get(16)?),
                    context_gc: n(r.get(17)?),
                    filters: n(r.get(18)?),
                    cache_read_tokens: cache_read,
                    cache_usd: r.get(19)?,
                    total_usd: r.get(20)?,
                },
                estimated: r.get(21)?,
                cost_partial: r.get(22)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The receipts table has its `cost_partial` column (an empty or new file: no).
fn has_cost_partial(conn: &Connection) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('receipts') WHERE name = 'cost_partial'",
        [],
        |r| r.get::<_, i64>(0).map(|n| n > 0),
    )
}

/// Bring the schema to [`SCHEMA_VERSION`] in one write transaction (the version is read
/// again inside it, so two processes never migrate twice).
fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let step = || -> rusqlite::Result<()> {
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version == 0 {
            conn.execute_batch(SCHEMA)?;
        } else {
            if version < 2 {
                conn.execute_batch(V2)?;
            }
            if version < 3 {
                conn.execute_batch(V3)?;
            }
        }
        if version < 4 {
            conn.execute_batch(V4)?;
        }
        if !has_cost_partial(conn)? {
            conn.execute_batch(V4_COST_PARTIAL)?;
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
    Receipt(Box<ReceiptRow>),
    /// Answer once every row queued before it is written (or failed).
    Flush(tokio::sync::oneshot::Sender<()>),
}

/// A turn receipt on its way to the ledger; the writer looks up the project id of `root`
/// (it reads git files, so off the async workers).
pub(crate) struct ReceiptRow {
    pub session: String,
    pub root: PathBuf,
    pub receipt: Event,
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
                let written = match msg {
                    Msg::Row(row) => write(&ledger, &row),
                    Msg::Receipt(r) => {
                        let project = tautly_session::paths::project_id(&r.root);
                        ledger.record_receipt(&r.session, &project, &r.root, &r.receipt)
                    }
                    Msg::Flush(done) => {
                        let _ = done.send(());
                        continue;
                    }
                };
                if let Err(e) = written
                    && !std::mem::replace(&mut reported, true)
                {
                    warn(format!(
                        "a row could not be written to the cost ledger ({e}); later \
                         rows are still recorded, and further failures are not reported"
                    ));
                }
            }
        });
    }

    /// Queue one row; never waits. An error says it was dropped (the writer is too far
    /// behind). After the ledger turned out unavailable, rows are dropped silently.
    pub fn record(&self, row: Row) -> std::result::Result<(), String> {
        self.queue(Msg::Row(row))
    }

    /// Queue a turn receipt, like [`record`](Self::record).
    pub fn record_receipt(&self, row: ReceiptRow) -> std::result::Result<(), String> {
        self.queue(Msg::Receipt(Box::new(row)))
    }

    fn queue(&self, msg: Msg) -> std::result::Result<(), String> {
        match self.tx.try_send(msg) {
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
