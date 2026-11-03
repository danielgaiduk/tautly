//! The per-root hub that sessions share (spec 3.4): the code index, its file watcher and
//! the project memory store. Sessions on one root hold the same `Arc<Workspace>`; the
//! watcher stops when the last one drops it.
//!
//! Opening is lazy and off the start-up path: [`Workspace::start`] (called by each engine
//! after its first event) opens the index on a blocking task, starts its background
//! refresh and the watcher, then opens the memory store. Until then, and when opening
//! fails, tools run without an index (`ToolCx` gets none) and memory tools report that
//! memory is unavailable.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use tautly_index::{Index, WatchHandle};
use tautly_memory::{MemoryStore, sanitize_title};
use tautly_sandbox::Sandbox;
use tokio::sync::watch;

/// What opening produced. Each part may be missing; `warnings` says why.
pub(crate) struct Opened {
    pub index: Option<Arc<Index>>,
    pub memory: Option<Arc<RwLock<MemoryStore>>>,
    pub warnings: Vec<String>,
    /// Keeps the file watcher running for as long as the workspace lives.
    _watch: Mutex<Option<WatchHandle>>,
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
        let db = self
            .data_home
            .join("index")
            .join(format!("{}.db", tautly_session::paths::project_hash(&root)));
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
        }
    }

    pub fn root(&self) -> &Path {
        self.sandbox.root()
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
