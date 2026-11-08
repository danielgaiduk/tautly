//! SQLite-backed storage and queries (WAL mode, shared by several handles).

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock};
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use tautly_sandbox::Sandbox;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::fresh::Freshness;
use crate::lang::Lang;
use crate::tree::{SharedTree, Slot, TreeData};

/// Version written by this build; bump and append a [`MIGRATIONS`] step when the schema
/// changes.
pub(crate) const SCHEMA_VERSION: i64 = 2;
/// The oldest version a migration path exists from.
const OLDEST_MIGRATABLE: i64 = 1;
/// Version of everything derived from file content (symbols, references, trigrams).
/// Bump it when an extractor changes: a refresh that finds a different value in the
/// database reparses every file instead of trusting content hashes.
const EXTRACTOR_VERSION: &str = "2";
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
/// `files.trigram_state`: the trigram index cannot vouch for this file (binary, over the
/// size cap).
pub(crate) const TRI_NONE: u8 = 0;
/// The file's postings are in `trigrams` and match its stored hash.
pub(crate) const TRI_DONE: u8 = 1;
/// UTF-8 text whose postings have not been built (yet): found by a refresh, picked up by
/// [`Index::build_trigrams`].
pub(crate) const TRI_PENDING: u8 = 2;
/// SQLite's default page cache, and the one used while refreshing, in KiB.
const DEFAULT_CACHE_KIB: i64 = 2000;
const BULK_CACHE_KIB: i64 = 64 * 1024;
/// The page cache while the tree is loaded at open, in KiB.
const LOAD_CACHE_KIB: i64 = 128;

/// What kind of definition a symbol is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolKind {
    /// A free function.
    Function,
    /// A function inside a type or impl block.
    Method,
    /// A struct, enum, class, interface, trait or type alias.
    Type,
    /// A constant or static.
    Constant,
    /// A module or namespace.
    Module,
}

impl SymbolKind {
    /// The name stored in the database and shown to the model.
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

/// One definition in the index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolRow {
    /// Root-relative path of the file.
    pub path: PathBuf,
    /// The symbol's name.
    pub name: String,
    /// What kind of definition it is.
    pub kind: SymbolKind,
    /// The enclosing type, for a method.
    pub parent: Option<String>,
    /// First line, 1-based.
    pub start_line: u32,
    /// Last line, 1-based.
    pub end_line: u32,
    /// The declaration line(s), shortened.
    pub signature: String,
    /// blake3 hex of the definition's text.
    pub hash: String,
}

/// One reference to a symbol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefRow {
    /// Root-relative path of the file.
    pub path: PathBuf,
    /// The line, 1-based.
    pub line: u32,
    /// The trimmed source line, at most 200 characters.
    pub context: String,
}

/// What a refresh did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefreshStats {
    /// Files parsed again.
    pub parsed: u32,
    /// Files dropped from the index.
    pub removed: u32,
    /// Files left as they were.
    pub unchanged: u32,
}

/// The size of the index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexStats {
    /// Files indexed.
    pub files: u64,
    /// Definitions.
    pub symbols: u64,
    /// References.
    pub refs: u64,
}

/// Errors from the code index.
#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    /// The SQLite database failed.
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),
    /// A file could not be read.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// The sandbox refused or failed a read.
    #[error("sandbox error: {0}")]
    Sandbox(#[from] tautly_sandbox::SandboxError),
    /// The database was written by a newer version.
    #[error("index database has schema version {found}, this build understands {SCHEMA_VERSION}")]
    SchemaTooNew {
        /// The database's schema version.
        found: i64,
    },
    /// The database is too old to migrate (delete it to rebuild).
    #[error(
        "index database has schema version {found}, older than the oldest migratable version ({OLDEST_MIGRATABLE}); delete it to rebuild"
    )]
    SchemaTooOld {
        /// The database's schema version.
        found: i64,
    },
    /// The file watcher failed.
    #[error("file watcher error: {0}")]
    Watch(#[from] notify::Error),
    /// A background task panicked or was cancelled.
    #[error("background task failed: {0}")]
    Task(String),
}

/// What the index remembers about a file's content, to decide whether it changed.
pub(crate) struct KnownFile {
    pub hash: String,
    /// One of the `TRI_*` states.
    pub tri_state: u8,
}

/// One file ready to be written: its row, symbols, references and the source lines
/// the references sit on.
pub(crate) struct ParsedFile {
    pub path: String,
    /// Empty for a file over the size cap, which is never read.
    pub hash: String,
    pub mtime: i64,
    pub size: i64,
    /// None for extensions without a grammar.
    pub lang: Option<Lang>,
    /// One of the `TRI_*` states.
    pub tri_state: u8,
    pub symbols: Vec<SymbolRow>,
    pub refs: Vec<(u32, String)>,
    pub lines: Vec<(u32, String)>,
    /// Distinct trigrams of the content (see [`crate::trigram::trigrams`]); only for
    /// `TRI_DONE` files.
    pub trigrams: Vec<u32>,
}

pub(crate) struct Inner {
    conn: Mutex<Connection>,
    /// The database file, and a read-only connection to it opened on first use (see
    /// [`Index::top_referenced`]).
    db: PathBuf,
    reader: Mutex<Option<Connection>>,
    pub sb: Arc<dyn Sandbox>,
    pub fresh: Freshness,
    /// The generation the next write gets (see [`Inner::apply`]).
    next_generation: AtomicU32,
    /// While a refresh runs: keys other writers removed since it started, which it must
    /// not bring back from its scan.
    tombstones: Mutex<Option<HashSet<String>>>,
    /// Held for the whole of a refresh: refreshes of one index never overlap.
    pub refresh_lock: tokio::sync::Mutex<()>,
    /// SQLite's `data_version` when the tree last mirrored the database: it moves when
    /// another handle commits.
    data_version: AtomicI64,
    /// The last scan met symlinks: a change under one path may alias another.
    has_links: AtomicBool,
    last_error: Mutex<Option<String>>,
    tree: SharedTree,
    /// Completed refreshes (diagnostics and tests).
    refreshes: AtomicU64,
    /// Test hook: the next refresh pauses after its scan (see
    /// [`Index::hold_next_refresh_after_scan`]).
    scan_hold: Mutex<Option<(oneshot::Sender<()>, oneshot::Receiver<()>)>>,
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
        // A bulk refresh that was interrupted may have left these dropped.
        ensure_indexes(&conn)?;
        // Loading the tree reads every row once; a small cache keeps those pages from
        // staying resident.
        conn.pragma_update(None, "cache_size", -LOAD_CACHE_KIB)?;
        let tree = load_tree(&conn)?;
        conn.pragma_update(None, "cache_size", -DEFAULT_CACHE_KIB)?;
        let data_version = data_version(&conn)?;
        Ok(Index {
            inner: Arc::new(Inner {
                conn: Mutex::new(conn),
                db: db.to_path_buf(),
                reader: Mutex::new(None),
                sb,
                fresh: Freshness::default(),
                next_generation: AtomicU32::new(1),
                tombstones: Mutex::new(None),
                refresh_lock: tokio::sync::Mutex::new(()),
                data_version: AtomicI64::new(data_version),
                has_links: AtomicBool::new(false),
                last_error: Mutex::new(None),
                tree: RwLock::new(tree),
                refreshes: AtomicU64::new(0),
                scan_hold: Mutex::new(None),
            }),
        })
    }

    /// Run [`Index::refresh`] and then [`Index::build_trigrams`] in the background;
    /// never blocks the caller. A failed refresh leaves the index not ready (callers
    /// fall back to on-demand lookups) and is recorded in [`Index::last_error`]; the
    /// next call retries.
    pub fn spawn_background_refresh(self: Arc<Self>) -> JoinHandle<()> {
        tokio::spawn(async move {
            if self.refresh().await.is_ok() {
                let _ = self.build_trigrams().await;
            }
        })
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

    /// The most-referenced defined symbols as `(name, reference count)`, at most `limit`,
    /// most referenced first (ties by name). Only names the index has a non-method
    /// definition for count: identifiers like `self` or library calls are not symbols of
    /// this repository, and method names (`new`, `len`) are shared noise. The cap is
    /// applied in SQL; a database error is returned, not folded into an empty list.
    ///
    /// It reads on a read-only connection of its own: on a large index the query takes a
    /// while (150 ms on 20,000 files), and in WAL mode it then never holds up a write such
    /// as an edit's reindex. It sees what was committed when it started.
    pub fn top_referenced(&self, limit: usize) -> Result<Vec<(String, u64)>, IndexError> {
        let mut reader = self
            .inner
            .reader
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if reader.is_none() {
            let conn = Connection::open_with_flags(
                &self.inner.db,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                    | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?;
            conn.busy_timeout(BUSY_TIMEOUT)?;
            *reader = Some(conn);
        }
        let conn = reader.as_ref().expect("opened above");
        let mut stmt = conn.prepare_cached(
            "SELECT r.name, COUNT(*) AS n FROM refs r \
             WHERE r.name IN (SELECT name FROM symbols WHERE kind != 'method') \
             GROUP BY r.name ORDER BY n DESC, r.name ASC LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit as i64], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// The workspace root the index reads from.
    pub fn root(&self) -> &Path {
        self.inner.sb.root()
    }

    /// blake3 hex of the file content as last indexed.
    pub fn file_hash(&self, rel: &Path) -> Option<String> {
        self.try_file_hash(rel).ok().flatten()
    }

    /// [`file_hash`](Self::file_hash) that reports a database error instead of folding it
    /// into `None` (not indexed).
    pub fn try_file_hash(&self, rel: &Path) -> Result<Option<String>, IndexError> {
        let key = self.inner.key(rel);
        Ok(self
            .inner
            .conn()
            .query_row("SELECT hash FROM files WHERE path = ?1", [key], |r| {
                r.get(0)
            })
            .optional()?)
    }

    /// blake3 hex of the first definition (in source order) named `name` in the file,
    /// narrowed to the definitions whose parent is `parent` and whose kind is `kind` when
    /// those are given (None: any).
    pub fn symbol_hash_in(
        &self,
        rel: &Path,
        name: &str,
        parent: Option<&str>,
        kind: Option<SymbolKind>,
    ) -> Option<String> {
        self.try_symbol_hash_in(rel, name, parent, kind)
            .ok()
            .flatten()
    }

    /// [`symbol_hash_in`](Self::symbol_hash_in) that reports a database error instead of
    /// folding it into `None` (no such definition).
    pub fn try_symbol_hash_in(
        &self,
        rel: &Path,
        name: &str,
        parent: Option<&str>,
        kind: Option<SymbolKind>,
    ) -> Result<Option<String>, IndexError> {
        let key = self.inner.key(rel);
        Ok(self
            .inner
            .conn()
            .query_row(
                "SELECT hash FROM symbols WHERE file = ?1 AND name = ?2 \
                 AND (?3 IS NULL OR parent = ?3) AND (?4 IS NULL OR kind = ?4) \
                 ORDER BY start_line, id LIMIT 1",
                params![key, name, parent, kind.map(SymbolKind::as_str)],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// How many refreshes have completed on this handle.
    #[doc(hidden)]
    pub fn refresh_count(&self) -> u64 {
        self.inner.refreshes.load(Ordering::SeqCst)
    }

    /// Test hook: the next refresh pauses right after its scan until the returned hold is
    /// released, so a test can act between the scan and the writes.
    #[doc(hidden)]
    pub fn hold_next_refresh_after_scan(&self) -> RefreshHold {
        let (reached_tx, reached) = oneshot::channel();
        let (release, release_rx) = oneshot::channel();
        *self
            .inner
            .scan_hold
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some((reached_tx, release_rx));
        RefreshHold { reached, release }
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

    /// How many files, definitions and references the index holds.
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

/// See [`Index::hold_next_refresh_after_scan`].
#[doc(hidden)]
pub struct RefreshHold {
    reached: oneshot::Receiver<()>,
    release: oneshot::Sender<()>,
}

impl RefreshHold {
    /// Wait until the refresh has finished its scan.
    pub async fn reached(&mut self) {
        let _ = (&mut self.reached).await;
    }

    /// Let the refresh go on.
    pub fn release(self) {
        let _ = self.release.send(());
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

    /// Run `f` with the connection locked.
    pub fn with_conn<R>(&self, f: impl FnOnce(&Connection) -> R) -> R {
        f(&self.conn())
    }

    pub fn tree(&self) -> &SharedTree {
        &self.tree
    }

    /// Whether the stored extractor version differs from this build's, so that no
    /// content hash can be trusted to mean "already extracted".
    pub fn extractor_changed(&self) -> Result<bool, IndexError> {
        let stored: Option<String> = self
            .conn()
            .query_row(
                "SELECT value FROM meta WHERE key = 'extractor_version'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        Ok(stored.as_deref() != Some(EXTRACTOR_VERSION))
    }

    pub fn record_extractor_version(&self) -> Result<(), IndexError> {
        self.conn().execute(
            "INSERT INTO meta (key, value) VALUES ('extractor_version', ?1) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [EXTRACTOR_VERSION],
        )?;
        Ok(())
    }

    /// Give SQLite a large page cache for the duration of a bulk refresh (the postings
    /// b-tree is far bigger than the default 2 MB cache, and cache misses dominate the
    /// build), then hand the memory back so an idle index stays small.
    pub fn set_bulk_cache(&self, on: bool) -> Result<(), IndexError> {
        let conn = self.conn();
        let kib: i64 = if on {
            -BULK_CACHE_KIB
        } else {
            -DEFAULT_CACHE_KIB
        };
        conn.pragma_update(None, "cache_size", kib)?;
        // Let the WAL grow during a bulk build instead of checkpointing every 4 MB.
        conn.pragma_update(None, "wal_autocheckpoint", if on { 16384 } else { 1000 })?;
        if !on {
            conn.execute_batch("PRAGMA shrink_memory")?;
        }
        Ok(())
    }

    /// Drop the two name indexes before a bulk build: with them in place every inserted
    /// symbol and reference is a random b-tree insert, which is half the build time;
    /// [`ensure_indexes`] rebuilds them sorted afterwards.
    pub fn drop_name_indexes(&self) -> Result<(), IndexError> {
        self.conn().execute_batch(
            "DROP INDEX IF EXISTS symbols_name; DROP INDEX IF EXISTS refs_name_file_line;",
        )?;
        Ok(())
    }

    pub fn ensure_indexes(&self) -> Result<(), IndexError> {
        ensure_indexes(&self.conn())
    }

    /// Marks up to `seen` (the mark count read when an operation started) as covered.
    pub fn cover_marks(&self, seen: u64) {
        self.fresh.cover_marks(seen);
    }

    pub fn stale_mark_count(&self) -> u64 {
        self.fresh.stale_mark_count()
    }

    pub fn mark_ready(&self) {
        self.fresh.mark_ready();
    }

    pub fn mark_trigrams_ready(&self) {
        self.fresh.mark_trigrams_ready();
    }

    /// Start a refresh's snapshot: rows written from now on carry a generation of at least
    /// the one returned, and keys removed from now on are remembered, so the refresh can
    /// leave both alone ([`Inner::apply_guarded`]). Taken with the connection held, so no
    /// write is half done.
    ///
    /// When another handle has written to the database since, the tree is reloaded first,
    /// so the refresh compares the disk with what the database holds.
    pub fn begin_refresh(&self) -> Result<u32, IndexError> {
        let conn = self.conn();
        let version = data_version(&conn)?;
        if version != self.data_version.load(Ordering::SeqCst) {
            let tree = load_tree(&conn)?;
            *self.tree.write().unwrap_or_else(PoisonError::into_inner) = tree;
            self.data_version.store(version, Ordering::SeqCst);
        }
        *self
            .tombstones
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(HashSet::new());
        Ok(self.next_generation.load(Ordering::SeqCst))
    }

    pub fn end_refresh(&self) {
        *self
            .tombstones
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = None;
    }

    /// Whether any file waits for its postings.
    pub fn has_pending_trigrams(&self) -> Result<bool, IndexError> {
        Ok(self.conn().query_row(
            &format!("SELECT EXISTS (SELECT 1 FROM files WHERE trigram_state = {TRI_PENDING})"),
            [],
            |r| r.get(0),
        )?)
    }

    /// The stored content hash of each key (empty when it has none).
    pub fn stored_hashes(&self, keys: &[&str]) -> Vec<String> {
        let conn = self.conn();
        let Ok(mut stmt) = conn.prepare_cached("SELECT hash FROM files WHERE path = ?1") else {
            return vec![String::new(); keys.len()];
        };
        keys.iter()
            .map(|k| {
                stmt.query_row([k], |r| r.get(0))
                    .optional()
                    .ok()
                    .flatten()
                    .unwrap_or_default()
            })
            .collect()
    }

    /// What the index remembers about each key, if anything.
    pub fn known_files_for(&self, keys: &[String]) -> Result<Vec<Option<KnownFile>>, IndexError> {
        let conn = self.conn();
        let mut stmt =
            conn.prepare_cached("SELECT hash, trigram_state FROM files WHERE path = ?1")?;
        let mut out = Vec::with_capacity(keys.len());
        for key in keys {
            out.push(
                stmt.query_row([key], |r| {
                    Ok(KnownFile {
                        hash: r.get(0)?,
                        tri_state: r.get(1)?,
                    })
                })
                .optional()?,
            );
        }
        Ok(out)
    }

    pub fn set_has_links(&self, v: bool) {
        self.has_links.store(v, Ordering::Release);
    }

    pub fn has_links(&self) -> bool {
        self.has_links.load(Ordering::Acquire)
    }

    /// Record the outcome of a refresh: success clears the last error.
    /// Remember a failure outside a refresh (the watcher); the next successful refresh
    /// clears it.
    pub fn record_error(&self, message: String) {
        *self
            .last_error
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(message);
    }

    /// The postings of these files may no longer describe them: forget the postings and
    /// make the files candidates for every search until they are re-read. Only files
    /// that have postings are affected (the others are candidates already).
    pub fn mark_pending(&self, paths: &[String]) -> Result<(), IndexError> {
        self.mark_pending_where("path = ?1", paths)
    }

    /// [`Inner::mark_pending`] for every file under the directories `dirs`.
    pub fn mark_pending_under(&self, dirs: &[String]) -> Result<(), IndexError> {
        // `0` is the character after `/`, so this is the range of `dir/...`.
        self.mark_pending_where("(path >= ?1 || '/' AND path < ?1 || '0')", dirs)
    }

    fn mark_pending_where(&self, condition: &str, keys: &[String]) -> Result<(), IndexError> {
        if keys.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        {
            let rows = format!("{condition} AND trigram_state = {TRI_DONE}");
            let mut delete = tx.prepare_cached(&format!(
                "DELETE FROM trigrams WHERE file_id IN (SELECT id FROM files WHERE {rows})"
            ))?;
            let mut mark = tx.prepare_cached(&format!(
                "UPDATE files SET trigram_state = {TRI_PENDING} WHERE {rows}"
            ))?;
            for key in keys {
                delete.execute([key])?;
                mark.execute([key])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Pause at the test hook if one is armed (see [`Index::hold_next_refresh_after_scan`]).
    pub async fn scan_hold_point(&self) {
        let hold = self
            .scan_hold
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some((reached, release)) = hold {
            let _ = reached.send(());
            let _ = release.await;
        }
    }

    pub fn count_refresh(&self) {
        self.refreshes.fetch_add(1, Ordering::SeqCst);
    }

    pub fn record_refresh<T>(&self, result: &Result<T, IndexError>) {
        let mut slot = self
            .last_error
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *slot = result.as_ref().err().map(ToString::to_string);
        drop(slot);
        self.fresh.refreshed.send_replace(Some(result.is_ok()));
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

    pub fn known_file(&self, key: &str) -> Result<Option<KnownFile>, IndexError> {
        Ok(self
            .conn()
            .query_row(
                "SELECT hash, trigram_state FROM files WHERE path = ?1",
                [key],
                |r| {
                    Ok(KnownFile {
                        hash: r.get(0)?,
                        tri_state: r.get(1)?,
                    })
                },
            )
            .optional()?)
    }

    /// Apply one batch of changes in a single transaction, then mirror it in the tree.
    /// Every row written gets a new generation.
    pub fn apply(
        &self,
        parsed: &[ParsedFile],
        touched: &[(String, i64, i64)],
        removed: &[String],
    ) -> Result<(), IndexError> {
        self.apply_guarded(parsed, touched, removed, None)
    }

    /// [`Inner::apply`] for a refresh whose snapshot started at generation `since`
    /// ([`Inner::begin_refresh`]): a row another writer wrote since then (generation at
    /// least `since`), or a key it removed since then, is left alone. The check and the
    /// write happen with the connection held, which every writer holds for its whole write.
    pub fn apply_guarded(
        &self,
        parsed: &[ParsedFile],
        touched: &[(String, i64, i64)],
        removed: &[String],
        since: Option<u32>,
    ) -> Result<(), IndexError> {
        let mut conn = self.conn();
        let (parsed, touched, removed): (Vec<&ParsedFile>, Vec<_>, Vec<&String>) = match since {
            None => (
                parsed.iter().collect(),
                touched.iter().collect(),
                removed.iter().collect(),
            ),
            Some(since) => {
                let tree = self.tree.read().unwrap_or_else(PoisonError::into_inner);
                let tombstones = self
                    .tombstones
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                let untouched = |key: &str| match tree.get(key) {
                    Some(slot) => slot.generation < since,
                    None => !tombstones.as_ref().is_some_and(|t| t.contains(key)),
                };
                (
                    parsed.iter().filter(|f| untouched(&f.path)).collect(),
                    touched.iter().filter(|t| untouched(&t.0)).collect(),
                    removed
                        .iter()
                        .filter(|k| tree.get(k).is_some_and(|s| s.generation < since))
                        .collect(),
                )
            }
        };
        if parsed.is_empty() && touched.is_empty() && removed.is_empty() {
            return Ok(());
        }
        let generation = self.next_generation.fetch_add(1, Ordering::SeqCst);
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        {
            // Postings carry no foreign key (they point at `files.id`), so they go first.
            let mut delete_tris = tx.prepare_cached(
                "DELETE FROM trigrams WHERE file_id = (SELECT id FROM files WHERE path = ?1)",
            )?;
            let mut delete = tx.prepare_cached("DELETE FROM files WHERE path = ?1")?;
            for path in &removed {
                delete_tris.execute([path])?;
                delete.execute([path])?;
            }
            let mut touch =
                tx.prepare_cached("UPDATE files SET mtime = ?2, size = ?3 WHERE path = ?1")?;
            for (path, mtime, size) in &touched {
                touch.execute(params![path, mtime, size])?;
            }
            let mut insert_file = tx.prepare_cached(
                "INSERT INTO files (path, hash, mtime, size, lang, trigram_state) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
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
            let mut insert_tri =
                tx.prepare_cached("INSERT INTO trigrams (tri, file_id) VALUES (?1, ?2)")?;
            let mut postings: Vec<u64> = Vec::new();
            for f in &parsed {
                delete_tris.execute([&f.path])?;
                delete.execute([&f.path])?;
                insert_file.execute(params![
                    f.path,
                    f.hash,
                    f.mtime,
                    f.size,
                    f.lang.map_or("other", Lang::as_str),
                    f.tri_state
                ])?;
                let file_id = tx.last_insert_rowid();
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
                postings.extend(f.trigrams.iter().map(|t| pack_posting(*t, file_id)));
            }
            // In key order, so the postings b-tree is appended to rather than hit at
            // random.
            postings.sort_unstable();
            for p in postings {
                insert_tri.execute(params![p >> POSTING_ID_BITS, p & POSTING_ID_MASK])?;
            }
        }
        tx.commit()?;
        if since.is_none()
            && let Some(t) = self
                .tombstones
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .as_mut()
        {
            t.extend(removed.iter().map(|k| k.to_string()));
        }
        let mut tree = self.tree.write().unwrap_or_else(PoisonError::into_inner);
        for path in removed {
            tree.remove(path);
        }
        for (path, mtime, size) in touched {
            if let Some(slot) = tree.get_mut(path) {
                *slot = Slot::new(path, *size, *mtime, generation);
            }
        }
        for f in parsed {
            tree.insert(&f.path, Slot::new(&f.path, f.size, f.mtime, generation));
        }
        Ok(())
    }
}

/// A file whose postings are waiting to be built.
pub(crate) struct PendingFile {
    pub id: i64,
    pub key: String,
    pub size: i64,
    pub hash: String,
}

impl Inner {
    pub fn pending_trigram_files(&self) -> Result<Vec<PendingFile>, IndexError> {
        let conn = self.conn();
        let mut stmt = conn.prepare_cached(&format!(
            "SELECT id, path, size, hash FROM files WHERE trigram_state = {TRI_PENDING} ORDER BY id"
        ))?;
        let rows = stmt.query_map([], |r| {
            Ok(PendingFile {
                id: r.get(0)?,
                key: r.get(1)?,
                size: r.get(2)?,
                hash: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Store postings for pending files, one transaction. A file is only marked done if
    /// it is still the pending row with the hash the postings were computed from (a
    /// concurrent reindex replaces the row); returns the paths that were.
    pub fn apply_trigrams(
        &self,
        items: &[(i64, String, String, Vec<u32>)],
    ) -> Result<Vec<String>, IndexError> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut done = Vec::new();
        {
            let mut mark = tx.prepare_cached(&format!(
                "UPDATE files SET trigram_state = {TRI_DONE} \
                 WHERE id = ?1 AND hash = ?2 AND trigram_state = {TRI_PENDING}"
            ))?;
            let mut postings: Vec<u64> = Vec::new();
            for (id, path, hash, tris) in items {
                if mark.execute(params![id, hash])? == 1 {
                    postings.extend(tris.iter().map(|t| pack_posting(*t, *id)));
                    done.push(path.clone());
                }
            }
            postings.sort_unstable();
            let mut insert =
                tx.prepare_cached("INSERT INTO trigrams (tri, file_id) VALUES (?1, ?2)")?;
            for p in postings {
                insert.execute(params![p >> POSTING_ID_BITS, p & POSTING_ID_MASK])?;
            }
        }
        tx.commit()?;
        Ok(done)
    }
}

fn ensure_indexes(conn: &Connection) -> Result<(), IndexError> {
    conn.execute_batch(&format!(
        "CREATE INDEX IF NOT EXISTS symbols_name ON symbols(name); \
         CREATE INDEX IF NOT EXISTS refs_name_file_line ON refs(name, file, line); \
         CREATE INDEX IF NOT EXISTS files_pending ON files(id) WHERE trigram_state = {TRI_PENDING}; \
         CREATE INDEX IF NOT EXISTS files_opaque ON files(path) WHERE trigram_state != {TRI_DONE};"
    ))?;
    Ok(())
}

const POSTING_ID_BITS: u32 = 40;
const POSTING_ID_MASK: u64 = (1 << POSTING_ID_BITS) - 1;

/// A posting as one sortable integer: trigram in the high bits, file id below.
fn pack_posting(tri: u32, file_id: i64) -> u64 {
    (u64::from(tri) << POSTING_ID_BITS) | (file_id as u64 & POSTING_ID_MASK)
}

/// Moves whenever another connection commits to the database.
fn data_version(conn: &Connection) -> Result<i64, IndexError> {
    Ok(conn.pragma_query_value(None, "data_version", |r| r.get(0))?)
}

/// The tree as the database remembers it. In path order (the unique index on `path`),
/// so every file is appended to its directory's list.
fn load_tree(conn: &Connection) -> Result<TreeData, IndexError> {
    let mut tree = TreeData::default();
    let mut stmt = conn.prepare("SELECT path, size, mtime FROM files ORDER BY path")?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let path: String = row.get(0)?;
        tree.insert(&path, Slot::new(&path, row.get(1)?, row.get(2)?, 0));
    }
    tree.shrink();
    Ok(tree)
}

fn migrate(conn: &mut Connection) -> Result<(), IndexError> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute_batch("CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL)")?;
    let found: Option<i64> = tx
        .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
        .optional()?;
    match found {
        None => {
            // A new database is created at v1 and migrated like any other.
            tx.execute_batch(SCHEMA_V1)?;
            tx.execute("INSERT INTO schema_version (version) VALUES (1)", [])?;
            upgrade(&tx, 1)?;
        }
        Some(found) if found > SCHEMA_VERSION => return Err(IndexError::SchemaTooNew { found }),
        Some(found) => upgrade(&tx, found)?,
    }
    tx.commit()?;
    Ok(())
}

/// Migration scripts as `(from_version, sql)`; each upgrades `from_version` to
/// `from_version + 1`. A schema change appends a step here and bumps
/// [`SCHEMA_VERSION`].
const MIGRATIONS: &[(i64, &str)] = &[(1, MIGRATE_V1_TO_V2)];

/// The v2 schema. Everything in a v1 database is derived from the sources, and the
/// extractor-version change would reparse every file anyway, so the migration drops the
/// v1 tables and creates v2 ones rather than rebuilding `files` in place.
///
/// What v2 adds: `files.id`, an explicit `INTEGER PRIMARY KEY` that trigram postings
/// point at (an implicit rowid would not survive `VACUUM`); `files.trigram_state` (see
/// the `TRI_*` constants); `trigrams` (the `trigrams_file` index serves deleting one
/// file's postings); `meta`; and a covering index for reference lookups.
const MIGRATE_V1_TO_V2: &str = "
DROP TABLE ref_lines;
DROP TABLE refs;
DROP TABLE symbols;
DROP TABLE files;
CREATE TABLE files (
    id            INTEGER PRIMARY KEY,
    path          TEXT NOT NULL UNIQUE,
    hash          TEXT NOT NULL,
    mtime         INTEGER NOT NULL,
    size          INTEGER NOT NULL,
    lang          TEXT NOT NULL,
    trigram_state INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX files_pending ON files(id) WHERE trigram_state = 2;
CREATE INDEX files_opaque ON files(path) WHERE trigram_state != 1;
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
CREATE INDEX refs_name_file_line ON refs(name, file, line);
CREATE TABLE ref_lines (
    file TEXT NOT NULL REFERENCES files(path) ON DELETE CASCADE,
    line INTEGER NOT NULL,
    text TEXT NOT NULL,
    PRIMARY KEY (file, line)
) WITHOUT ROWID;
CREATE TABLE trigrams (
    tri     INTEGER NOT NULL,
    file_id INTEGER NOT NULL,
    PRIMARY KEY (tri, file_id)
) WITHOUT ROWID;
CREATE INDEX trigrams_file ON trigrams(file_id);
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT) WITHOUT ROWID;
";

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
