//! A strictly read-only look at an index database, for diagnostics (`tautly doctor`): no
//! file or directory is created, no schema is migrated, no index is built and no write
//! lock is taken, so it never gets in the way of a session refreshing the same index.

use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, OptionalExtension};

use crate::store::IndexError;

/// How long a peek waits for a lock (a reader of a WAL database normally waits for none).
const BUSY_TIMEOUT: Duration = Duration::from_millis(250);

/// What an index database holds, as far as a reader can tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexPeek {
    /// The database's schema version (0 when it records none).
    pub schema_version: i64,
    /// Indexed files.
    pub files: u64,
}

impl IndexPeek {
    /// Whether this build uses the database as it is (else a session migrates it first,
    /// or refuses a newer one).
    pub fn is_current(&self) -> bool {
        self.schema_version == crate::store::SCHEMA_VERSION
    }
}

/// The schema version and file count of the index database at `db`; `None` when there is
/// no such file. The database is opened read-only.
pub fn peek(db: &Path) -> Result<Option<IndexPeek>, IndexError> {
    if !db.is_file() {
        return Ok(None);
    }
    // No CREATE flag: a file removed meanwhile is an error, never an empty new database.
    let conn = Connection::open_with_flags(
        db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    let schema_version = conn
        .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
        .optional()
        .or_else(|e| match e {
            // An index from before versioning, or not an index at all: version 0.
            rusqlite::Error::SqliteFailure(_, Some(ref m)) if m.contains("no such table") => {
                Ok(None)
            }
            e => Err(e),
        })?
        .unwrap_or(0);
    let files: i64 = conn.query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))?;
    Ok(Some(IndexPeek {
        schema_version,
        files: u64::try_from(files).unwrap_or(0),
    }))
}
