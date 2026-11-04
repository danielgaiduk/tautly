//! The per-root hub that sessions share (spec 3.4): the code index, its file watcher and
//! the project memory store. Sessions on one root hold the same `Arc<Workspace>`; the
//! watcher stops when the last one drops it.
//!
//! Opening is lazy and off the start-up path: [`Workspace::start`] (called by each engine
//! after its first event) opens the index on a blocking task, starts its background
//! refresh and the watcher, then opens the memory store. Until then, and when opening
//! fails, tools run without an index (`ToolCx` gets none) and memory tools report that
//! memory is unavailable.
//!
//! [`Workspaces`] hands every session a process starts on one root the same workspace.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock, Weak};
use std::time::Duration;

use tautly_index::{Index, WatchHandle};
use tautly_memory::{MemoryStore, sanitize_title};
use tautly_sandbox::{LocalSandbox, Sandbox};
use tokio::sync::watch;

use crate::engine::{EngineError, session_root};

/// Why the index database could not be locked exclusively.
#[derive(Debug, thiserror::Error)]
pub enum IndexLockError {
    #[error("a running tautly is using this work tree's code index; stop it first")]
    Busy,
    #[error("cannot lock the code index: {0}")]
    Io(#[from] std::io::Error),
}

/// `<db>.lock`, next to the index database.
fn index_lock_path(db: &Path) -> PathBuf {
    let mut name = db.as_os_str().to_os_string();
    name.push(".lock");
    PathBuf::from(name)
}

/// The lock file, created (owner-only) with its directory when it is missing.
fn open_index_lock(path: &Path) -> std::io::Result<File> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
}

/// Lock shared; waits while a rebuild holds the lock (it holds it only to delete files).
fn lock_index_shared(path: &Path) -> std::io::Result<File> {
    let file = open_index_lock(path)?;
    file.lock_shared()?;
    Ok(file)
}

/// What opening produced. Each part may be missing; `warnings` says why.
pub(crate) struct Opened {
    pub index: Option<Arc<Index>>,
    pub memory: Option<Arc<RwLock<MemoryStore>>>,
    pub warnings: Vec<String>,
    /// Keeps the file watcher running for as long as the workspace lives.
    _watch: Mutex<Option<WatchHandle>>,
    /// The shared lock on the index database's lock file, held for the workspace's life:
    /// `tautly index rebuild` takes it exclusively, so it cannot delete the database under
    /// a running session.
    _index_lock: Option<File>,
}

pub struct Workspace {
    sandbox: Arc<dyn Sandbox>,
    data_home: PathBuf,
    started: AtomicBool,
    opened: watch::Sender<Option<Arc<Opened>>>,
}

impl Workspace {
    /// A workspace for the sandbox's root, with the index database and memory episodes
    /// under `data_home`. Nothing is opened until [`start`](Self::start).
    pub fn new(sandbox: Arc<dyn Sandbox>, data_home: &Path) -> Arc<Workspace> {
        Arc::new(Workspace {
            sandbox,
            data_home: data_home.to_path_buf(),
            started: AtomicBool::new(false),
            opened: watch::Sender::new(None),
        })
    }

    /// Start opening the index, watcher and memory store in the background; later calls
    /// do nothing. Needs a Tokio runtime.
    pub fn start(self: &Arc<Self>) {
        if self.started.swap(true, Ordering::SeqCst) {
            return;
        }
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let opened = this.open().await;
            this.opened.send_replace(Some(Arc::new(opened)));
        });
    }

    async fn open(&self) -> Opened {
        let mut warnings = Vec::new();
        let root = self.sandbox.root().to_path_buf();
        let db = self.index_db();
        let lock_file = index_lock_path(&db);
        let locked = tokio::task::spawn_blocking(move || lock_index_shared(&lock_file)).await;
        let index_lock = match locked {
            Ok(Ok(f)) => Some(f),
            Ok(Err(e)) => {
                warnings.push(format!(
                    "code index lock unavailable ({e}); `tautly index rebuild` would not \
                     notice this session"
                ));
                None
            }
            Err(_) => None,
        };
        let sb = Arc::clone(&self.sandbox);
        let index = match tokio::task::spawn_blocking(move || Index::open(&db, sb)).await {
            Ok(Ok(index)) => Some(Arc::new(index)),
            Ok(Err(e)) => {
                warnings.push(format!("code index unavailable: {e}"));
                None
            }
            Err(e) => {
                warnings.push(format!("code index unavailable: {e}"));
                None
            }
        };
        let mut handle = None;
        if let Some(index) = &index {
            Arc::clone(index).spawn_background_refresh();
            // A recursive watch can walk the whole tree (inotify): off the async workers.
            let watched = Arc::clone(index);
            match tokio::task::spawn_blocking(move || watched.start_watcher()).await {
                Ok(Ok(h)) => handle = Some(h),
                Ok(Err(e)) => warnings.push(format!(
                    "file watcher not running ({e}); the code index is treated as stale"
                )),
                Err(e) => warnings.push(format!(
                    "file watcher not running ({e}); the code index is treated as stale"
                )),
            }
        }
        let memory = match &index {
            Some(index) => {
                let (index, home) = (Arc::clone(index), self.data_home.clone());
                let opened =
                    tokio::task::spawn_blocking(move || MemoryStore::open_in(&root, &home, index))
                        .await;
                match opened {
                    Ok(Ok(store)) => {
                        let skipped = store.skipped_count();
                        if let (true, Some((path, _))) = (skipped > 0, store.skipped().first()) {
                            // The file's name only: its text and the reason (which may
                            // quote it) are the repository's, and the path is local.
                            // (`sanitize_title` also clips it to 120 characters.)
                            let name = path
                                .file_name()
                                .map(|n| sanitize_title(&n.to_string_lossy()))
                                .unwrap_or_default();
                            warnings.push(format!(
                                "{skipped} memory entr{} could not be loaded (first: {name})",
                                if skipped == 1 { "y" } else { "ies" },
                            ));
                        }
                        Some(Arc::new(RwLock::new(store)))
                    }
                    Ok(Err(e)) => {
                        warnings.push(format!("project memory unavailable: {e}"));
                        None
                    }
                    Err(e) => {
                        warnings.push(format!("project memory unavailable: {e}"));
                        None
                    }
                }
            }
            None => None,
        };
        Opened {
            index,
            memory,
            warnings,
            _watch: Mutex::new(handle),
            _index_lock: index_lock,
        }
    }

    pub fn root(&self) -> &Path {
        self.sandbox.root()
    }

    /// The code index's database file (`tautly index rebuild` removes it).
    pub fn index_db(&self) -> PathBuf {
        self.data_home.join("index").join(format!(
            "{}.db",
            tautly_session::paths::project_hash(self.sandbox.root())
        ))
    }

    /// Take the index database's lock exclusively, without waiting: `tautly index rebuild`
    /// holds the returned file while it deletes the database, so no running session (each
    /// holds the lock shared from [`start`](Self::start) on) has it open. Dropping the
    /// file releases the lock.
    pub fn try_lock_index_exclusive(&self) -> Result<File, IndexLockError> {
        let file = open_index_lock(&index_lock_path(&self.index_db()))?;
        match file.try_lock() {
            Ok(()) => Ok(file),
            Err(std::fs::TryLockError::WouldBlock) => Err(IndexLockError::Busy),
            Err(std::fs::TryLockError::Error(e)) => Err(IndexLockError::Io(e)),
        }
    }

    pub fn sandbox(&self) -> &Arc<dyn Sandbox> {
        &self.sandbox
    }

    /// The code index, once opened.
    pub fn index(&self) -> Option<Arc<Index>> {
        self.opened.borrow().as_ref().and_then(|o| o.index.clone())
    }

    /// The project memory store, once opened.
    pub fn memory(&self) -> Option<Arc<RwLock<MemoryStore>>> {
        self.opened.borrow().as_ref().and_then(|o| o.memory.clone())
    }

    /// Wait until opening has finished (successfully or not).
    pub(crate) async fn opened(&self) -> Arc<Opened> {
        let mut rx = self.opened.subscribe();
        let opened = rx
            .wait_for(Option::is_some)
            .await
            .expect("the workspace holds its sender");
        Arc::clone(opened.as_ref().expect("waited for Some"))
    }

    /// Wait up to `timeout` until the index is open and has finished its first refresh.
    pub async fn wait_ready(&self, timeout: Duration) -> bool {
        tokio::time::timeout(timeout, async {
            match self.opened().await.index.clone() {
                Some(index) => index.wait_ready(timeout).await,
                None => false,
            }
        })
        .await
        .unwrap_or(false)
    }
}

/// The workspaces of a process's sessions (spec 3.4): one per root, opened by the first
/// session on it and closed when the last one ends. Held weakly, so a root no session
/// uses has nothing open. Clones share the registry.
#[derive(Clone)]
pub struct Workspaces {
    data_home: PathBuf,
    open: Arc<Mutex<HashMap<PathBuf, Weak<Workspace>>>>,
}

impl Workspaces {
    /// Workspaces whose index databases and memory episodes live under `data_home` (the
    /// sessions' `EngineBuilder::data_home`).
    pub fn new(data_home: impl Into<PathBuf>) -> Workspaces {
        Workspaces {
            data_home: data_home.into(),
            open: Arc::default(),
        }
    }

    pub fn data_home(&self) -> &Path {
        &self.data_home
    }

    /// The workspace of the root a session started in `start` runs on
    /// ([`session_root`]): the one a live session already uses, else a new one on a
    /// `LocalSandbox` (nothing is opened until a session starts it).
    pub fn get(&self, start: &Path) -> Result<Arc<Workspace>, EngineError> {
        let root = session_root(start)?;
        let mut open = self.open.lock().unwrap_or_else(PoisonError::into_inner);
        open.retain(|_, ws| ws.strong_count() > 0);
        if let Some(ws) = open.get(&root).and_then(Weak::upgrade) {
            return Ok(ws);
        }
        let sandbox: Arc<dyn Sandbox> = Arc::new(LocalSandbox::new(root.clone())?);
        let ws = Workspace::new(sandbox, &self.data_home);
        open.insert(root, Arc::downgrade(&ws));
        Ok(ws)
    }
}
