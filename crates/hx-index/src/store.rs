//! SQLite-backed storage and queries (WAL mode, shared by several handles).

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use hx_sandbox::Sandbox;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use tokio::task::JoinHandle;

use crate::lang::Lang;

/// Version written by this build; bump and add a `migrate` arm when the schema changes.
const SCHEMA_VERSION: i64 = 1;
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolKind {
    Function,
    Method,
    Type,
    Constant,
    Module,
}

impl SymbolKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SymbolKind::Function => "function",
            SymbolKind::Method => "method",
            SymbolKind::Type => "type",
            SymbolKind::Constant => "constant",
            SymbolKind::Module => "module",
        }
    }

    fn from_db(s: &str) -> SymbolKind {
        match s {
            "method" => SymbolKind::Method,
            "type" => SymbolKind::Type,
            "constant" => SymbolKind::Constant,
            "module" => SymbolKind::Module,
            _ => SymbolKind::Function,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolRow {
    pub path: PathBuf,
    pub name: String,
    pub kind: SymbolKind,
    pub parent: Option<String>,
    pub start_line: u32,
    pub end_line: u32,
    pub signature: String,
    pub hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefRow {
    pub path: PathBuf,
    pub line: u32,
    /// The trimmed source line, at most 200 characters.
    pub context: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefreshStats {
    pub parsed: u32,
    pub removed: u32,
    pub unchanged: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexStats {
    pub files: u64,
    pub symbols: u64,
    pub refs: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("sandbox error: {0}")]
    Sandbox(#[from] hx_sandbox::SandboxError),
    #[error("index database has schema version {found}, this build understands {SCHEMA_VERSION}")]
    SchemaTooNew { found: i64 },
    #[error(
        "index database has schema version {found}, older than the oldest supported ({SCHEMA_VERSION}); delete it to rebuild"
    )]
    SchemaTooOld { found: i64 },
    #[error("background task failed: {0}")]
    Task(String),
}

/// What the index remembers about a file, to decide whether it changed.
pub(crate) struct KnownFile {
    pub mtime: i64,
    pub size: i64,
    pub hash: String,
}

/// One file ready to be written: its row, symbols, references and the source lines
/// the references sit on.
pub(crate) struct ParsedFile {
    pub path: String,
    pub hash: String,
    pub mtime: i64,
    pub size: i64,
    pub lang: Lang,
    pub symbols: Vec<SymbolRow>,
    pub refs: Vec<(u32, String)>,
    pub lines: Vec<(u32, String)>,
}

pub(crate) struct Inner {
    conn: Mutex<Connection>,
    pub sb: Arc<dyn Sandbox>,
    ready: AtomicBool,
    last_error: Mutex<Option<String>>,
}

/// Incremental symbol and reference index of the sandbox root, persisted in SQLite.
///
/// Several handles may open the same database file (WAL mode); queries are cheap
/// synchronous reads, [`Index::refresh`] and [`Index::reindex_file`] do the work.
pub struct Index {
    pub(crate) inner: Arc<Inner>,
}

impl Index {
    /// Open (creating if needed) the database at `db`, with `sb` as the only way the
    /// index ever reads source files.
    pub fn open(db: &Path, sb: Arc<dyn Sandbox>) -> Result<Index, IndexError> {
        if let Some(dir) = db.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        let mut conn = Connection::open(db)?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        conn.pragma_update(None, "foreign_keys", true)?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update_and_check(None, "journal_mode", "WAL", |_| Ok(()))?;
        migrate(&mut conn)?;
        Ok(Index {
            inner: Arc::new(Inner {
                conn: Mutex::new(conn),
                sb,
                ready: AtomicBool::new(false),
                last_error: Mutex::new(None),
            }),
        })
    }

    /// Run [`Index::refresh`] in the background; never blocks the caller. A failed
    /// refresh leaves the index not ready (callers fall back to on-demand lookups) and
    /// is recorded in [`Index::last_error`]; the next call retries.
    pub fn spawn_background_refresh(self: Arc<Self>) -> JoinHandle<()> {
        tokio::spawn(async move {
            let _ = self.refresh().await;
        })
    }

    /// True once a refresh has completed since this handle was opened.
    pub fn is_ready(&self) -> bool {
        self.inner.ready.load(Ordering::Acquire)
    }

    /// The error of the most recent refresh, if it failed.
    pub fn last_error(&self) -> Option<String> {
        self.inner
            .last_error
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Symbols of one file in source order. Empty for unknown files.
    pub fn outline(&self, rel: &Path) -> Result<Vec<SymbolRow>, IndexError> {
        let key = self.inner.key(rel);
        let conn = self.inner.conn();
        let mut stmt = conn.prepare_cached(&format!(
            "{SYMBOL_SELECT} WHERE file = ?1 ORDER BY start_line, id"
        ))?;
        let rows = stmt.query_map([key], symbol_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Definitions named exactly `name`, optionally of one kind, ordered by file and line.
    pub fn find_symbol(
        &self,
        name: &str,
        kind: Option<SymbolKind>,
    ) -> Result<Vec<SymbolRow>, IndexError> {
        let conn = self.inner.conn();
        let mut stmt = conn.prepare_cached(&format!(
            "{SYMBOL_SELECT} WHERE name = ?1 AND (?2 IS NULL OR kind = ?2) \
             ORDER BY file, start_line, id"
        ))?;
        let rows = stmt.query_map(params![name, kind.map(SymbolKind::as_str)], symbol_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Name-based references (identifier occurrences that are not definitions), at most
    /// `limit`, ordered by file and line.
    pub fn refs(&self, name: &str, limit: usize) -> Result<Vec<RefRow>, IndexError> {
        self.refs_in(name, None, limit)
    }

    /// [`Index::refs`] restricted to files at or under `path_prefix` (a file or directory;
    /// `./`, absolute-under-root and trailing `/` spellings are normalized).
    pub fn refs_in(
        &self,
        name: &str,
        path_prefix: Option<&Path>,
        limit: usize,
    ) -> Result<Vec<RefRow>, IndexError> {
        // The root itself (empty key) filters nothing.
        let prefix = path_prefix
            .map(|p| self.inner.key(p))
            .filter(|k| !k.is_empty());
        let conn = self.inner.conn();
        // Whole path components: `a` matches `a` and `a/x.rs`, not `ab.rs`. No LIKE, so
        // `%` and `_` in directory names need no escaping.
        let mut stmt = conn.prepare_cached(
            "SELECT r.file, r.line, l.text FROM refs r \
             JOIN ref_lines l ON l.file = r.file AND l.line = r.line \
             WHERE r.name = ?1 \
               AND (?2 IS NULL OR r.file = ?2 OR substr(r.file, 1, length(?2) + 1) = ?2 || '/') \
             ORDER BY r.file, r.line LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![name, prefix, limit as i64], |r| {
            Ok(RefRow {
                path: PathBuf::from(r.get::<_, String>(0)?),
                line: r.get(1)?,
                context: r.get(2)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// blake3 hex of the file content as last indexed.
    pub fn file_hash(&self, rel: &Path) -> Option<String> {
        let key = self.inner.key(rel);
        self.inner
            .conn()
            .query_row("SELECT hash FROM files WHERE path = ?1", [key], |r| {
                r.get(0)
            })
            .optional()
            .ok()
            .flatten()
    }

    /// blake3 hex of the first definition named `name` in the file.
    pub fn symbol_hash(&self, rel: &Path, name: &str) -> Option<String> {
        let key = self.inner.key(rel);
        self.inner
            .conn()
            .query_row(
                "SELECT hash FROM symbols WHERE file = ?1 AND name = ?2 \
                 ORDER BY start_line, id LIMIT 1",
                params![key, name],
                |r| r.get(0),
            )
            .optional()
            .ok()
            .flatten()
    }

    pub fn stats(&self) -> IndexStats {
        let conn = self.inner.conn();
        let count = |table: &str| -> u64 {
            conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
                .unwrap_or(0)
        };
        IndexStats {
            files: count("files"),
            symbols: count("symbols"),
            refs: count("refs"),
        }
    }
}

const SYMBOL_SELECT: &str =
    "SELECT file, name, kind, parent, start_line, end_line, signature, hash FROM symbols";

fn symbol_row(r: &rusqlite::Row) -> rusqlite::Result<SymbolRow> {
    Ok(SymbolRow {
        path: PathBuf::from(r.get::<_, String>(0)?),
        name: r.get(1)?,
        kind: SymbolKind::from_db(&r.get::<_, String>(2)?),
        parent: r.get(3)?,
        start_line: r.get(4)?,
        end_line: r.get(5)?,
        signature: r.get(6)?,
        hash: r.get(7)?,
    })
}

impl Inner {
    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn mark_ready(&self) {
        self.ready.store(true, Ordering::Release);
    }

    /// Record the outcome of a refresh: success clears the last error.
    pub fn record_refresh<T>(&self, result: &Result<T, IndexError>) {
        let mut slot = self
            .last_error
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *slot = result.as_ref().err().map(ToString::to_string);
    }

    /// Run database work on the blocking pool, so a long transaction holding the
    /// connection mutex never parks an async worker.
    pub async fn run<T, F>(self: &Arc<Self>, f: F) -> Result<T, IndexError>
    where
        T: Send + 'static,
        F: FnOnce(&Inner) -> Result<T, IndexError> + Send + 'static,
    {
        let inner = Arc::clone(self);
        tokio::task::spawn_blocking(move || f(&inner))
            .await
            .map_err(|e| IndexError::Task(e.to_string()))?
    }

    /// The database key for a path: root-relative, `/`-separated, no `.` components.
    pub fn key(&self, p: &Path) -> String {
        let resolved;
        let mut p = p;
        if p.is_absolute() {
            // The caller's root may be spelled through a symlink (`/var` vs
            // `/private/var`); the sandbox knows the canonical form.
            if let Ok(r) = self.sb.resolve(p) {
                resolved = r;
                p = &resolved;
            }
            p = p.strip_prefix(self.sb.root()).unwrap_or(p);
        }
        p.components()
            .filter_map(|c| match c {
                Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
                Component::CurDir => None,
                other => Some(other.as_os_str().to_string_lossy().into_owned()),
            })
            .collect::<Vec<_>>()
            .join("/")
    }

    pub fn known_files(&self) -> Result<HashMap<String, KnownFile>, IndexError> {
        let conn = self.conn();
        let mut stmt = conn.prepare_cached("SELECT path, mtime, size, hash FROM files")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                KnownFile {
                    mtime: r.get(1)?,
                    size: r.get(2)?,
                    hash: r.get(3)?,
                },
            ))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn known_file(&self, key: &str) -> Result<Option<KnownFile>, IndexError> {
        Ok(self
            .conn()
            .query_row(
                "SELECT mtime, size, hash FROM files WHERE path = ?1",
                [key],
                |r| {
                    Ok(KnownFile {
                        mtime: r.get(0)?,
                        size: r.get(1)?,
                        hash: r.get(2)?,
                    })
                },
            )
            .optional()?)
    }

    /// Apply one batch of changes in a single transaction.
    pub fn apply(
        &self,
        parsed: &[ParsedFile],
        touched: &[(String, i64, i64)],
        removed: &[String],
    ) -> Result<(), IndexError> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        {
            let mut delete = tx.prepare_cached("DELETE FROM files WHERE path = ?1")?;
            for path in removed {
                delete.execute([path])?;
            }
            let mut touch =
                tx.prepare_cached("UPDATE files SET mtime = ?2, size = ?3 WHERE path = ?1")?;
            for (path, mtime, size) in touched {
                touch.execute(params![path, mtime, size])?;
            }
            let mut insert_file = tx.prepare_cached(
                "INSERT INTO files (path, hash, mtime, size, lang) VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            let mut insert_symbol = tx.prepare_cached(
                "INSERT INTO symbols \
                 (file, name, kind, parent, start_line, end_line, signature, hash) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )?;
            let mut insert_ref =
                tx.prepare_cached("INSERT INTO refs (file, line, name) VALUES (?1, ?2, ?3)")?;
            let mut insert_line =
                tx.prepare_cached("INSERT INTO ref_lines (file, line, text) VALUES (?1, ?2, ?3)")?;
            for f in parsed {
                delete.execute([&f.path])?;
                insert_file.execute(params![f.path, f.hash, f.mtime, f.size, f.lang.as_str()])?;
                for s in &f.symbols {
                    insert_symbol.execute(params![
                        f.path,
                        s.name,
                        s.kind.as_str(),
                        s.parent,
                        s.start_line,
                        s.end_line,
                        s.signature,
                        s.hash
                    ])?;
                }
                for (line, name) in &f.refs {
                    insert_ref.execute(params![f.path, line, name])?;
                }
                for (line, text) in &f.lines {
                    insert_line.execute(params![f.path, line, text])?;
                }
            }
        }
        tx.commit()?;
        Ok(())
    }
}

fn migrate(conn: &mut Connection) -> Result<(), IndexError> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute_batch("CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL)")?;
    let found: Option<i64> = tx
        .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
        .optional()?;
    match found {
        None => {
            tx.execute_batch(SCHEMA_V1)?;
            tx.execute(
                "INSERT INTO schema_version (version) VALUES (?1)",
                [SCHEMA_VERSION],
            )?;
        }
        Some(found) if found > SCHEMA_VERSION => return Err(IndexError::SchemaTooNew { found }),
        Some(found) => upgrade(&tx, found)?,
    }
    tx.commit()?;
    Ok(())
}

/// Migration scripts as `(from_version, sql)`; each upgrades `from_version` to
/// `from_version + 1`. Empty while v1 is the only schema; a schema change appends a
/// step here and bumps [`SCHEMA_VERSION`].
const MIGRATIONS: &[(i64, &str)] = &[];

/// Step an existing database from `from` up to [`SCHEMA_VERSION`].
fn upgrade(tx: &rusqlite::Transaction, from: i64) -> Result<(), IndexError> {
    let mut version = from;
    while version < SCHEMA_VERSION {
        let Some((_, sql)) = MIGRATIONS.iter().find(|(v, _)| *v == version) else {
            return Err(IndexError::SchemaTooOld { found: from });
        };
        tx.execute_batch(sql)?;
        version += 1;
    }
    if version != from {
        tx.execute("UPDATE schema_version SET version = ?1", [version])?;
    }
    Ok(())
}

/// `refs` holds only (file, line, name) as in the spec; the source line a reference
/// sits on is stored once per line in `ref_lines`.
const SCHEMA_V1: &str = "
CREATE TABLE files (
    path  TEXT PRIMARY KEY,
    hash  TEXT NOT NULL,
    mtime INTEGER NOT NULL,
    size  INTEGER NOT NULL,
    lang  TEXT NOT NULL
);
CREATE TABLE symbols (
    id         INTEGER PRIMARY KEY,
    file       TEXT NOT NULL REFERENCES files(path) ON DELETE CASCADE,
    name       TEXT NOT NULL,
    kind       TEXT NOT NULL,
    parent     TEXT,
    start_line INTEGER NOT NULL,
    end_line   INTEGER NOT NULL,
    signature  TEXT NOT NULL,
    hash       TEXT NOT NULL
);
CREATE INDEX symbols_file ON symbols(file);
CREATE INDEX symbols_name ON symbols(name);
CREATE TABLE refs (
    file TEXT NOT NULL REFERENCES files(path) ON DELETE CASCADE,
    line INTEGER NOT NULL,
    name TEXT NOT NULL
);
CREATE INDEX refs_file ON refs(file);
CREATE INDEX refs_name ON refs(name);
CREATE TABLE ref_lines (
    file TEXT NOT NULL REFERENCES files(path) ON DELETE CASCADE,
    line INTEGER NOT NULL,
    text TEXT NOT NULL,
    PRIMARY KEY (file, line)
) WITHOUT ROWID;
";
