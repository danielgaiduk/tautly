//! The cost ledger (spec 4.6): one SQLite row per provider response, across sessions and
//! processes, at `<data home>/ledger.sqlite`.

use std::path::Path;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, params};
use tautly_provider::{ModelId, RawUsage};

/// The ledger's file name in the data home.
pub const LEDGER_FILE: &str = "ledger.sqlite";

/// How long a write waits for another writer (another session or process).
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// `PRAGMA user_version` of the schema below. Changes are additive only (new columns with
/// defaults), so an older build can still write a newer file's rows it knows about.
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
    pub fn open(path: &Path) -> Result<Ledger> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path)?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        // Switching to WAL needs the file to itself and does not wait for a busy handler:
        // when another process is creating the ledger at the same moment, retry.
        until_not_busy(|| conn.pragma_update_and_check(None, "journal_mode", "WAL", |_| Ok(())))?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version < SCHEMA_VERSION {
            until_not_busy(|| migrate(&conn))?;
        }
        Ok(Ledger {
            conn: Mutex::new(conn),
        })
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
        let n = |v: i64| u64::try_from(v).unwrap_or(0);
        let totals = self.conn().query_row(
            "SELECT COUNT(*), COALESCE(SUM(input), 0), COALESCE(SUM(output), 0), \
             COALESCE(SUM(cache_read), 0), COALESCE(SUM(cache_write), 0), \
             COALESCE(SUM(cost_usd), 0.0), COUNT(*) - COUNT(cost_usd), \
             COALESCE(SUM(estimated), 0) \
             FROM usage WHERE at_ms >= ?1",
            params![unix_ms(since)],
            |r| {
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
            },
        )?;
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
