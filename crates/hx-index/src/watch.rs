//! File-system watcher: keeps the index and tree current with edits made outside hx.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::runtime::Handle;

use crate::store::{Index, IndexError};

/// Quiet time after the last event before a batch is processed.
const DEBOUNCE: Duration = Duration::from_millis(50);
/// A steady stream of events cannot postpone a batch longer than this.
const MAX_DEBOUNCE: Duration = Duration::from_millis(500);
/// How often an idle watcher thread checks whether it was asked to stop.
const IDLE_POLL: Duration = Duration::from_millis(100);

/// Keeps the watcher running; dropping it stops it.
pub struct WatchHandle {
    stop: Arc<AtomicBool>,
    // Held for its Drop: closing the OS watch disconnects the event channel.
    _watcher: RecommendedWatcher,
}

impl Drop for WatchHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

impl Index {
    /// Watch the workspace root recursively. Changes are debounced by 50 ms, then every
    /// changed path is brought in line with the disk ([`Index::reindex_file`] for
    /// files, a walk for directories, row removal for paths that are gone or not
    /// admitted); a change to an ignore file triggers a full [`Index::refresh`], and
    /// so does an overflow or error from the OS watcher. Must be called inside a Tokio
    /// runtime, which the watcher thread borrows for the index's async work.
    pub fn start_watcher(self: Arc<Self>) -> Result<WatchHandle, IndexError> {
        let runtime = Handle::try_current()
            .map_err(|_| IndexError::Task("start_watcher needs a Tokio runtime".into()))?;
        let root = self.inner.sb.root().to_path_buf();
        let (tx, rx) = channel();
        let mut watcher = notify::recommended_watcher(tx)?;
        watcher.watch(&root, RecursiveMode::Recursive)?;
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        std::thread::Builder::new()
            .name("hx-index-watch".into())
            .spawn(move || run(&self, &root, &rx, &flag, &runtime))
            .map_err(IndexError::Io)?;
        Ok(WatchHandle {
            stop,
            _watcher: watcher,
        })
    }
}

/// Changes collected during one debounce window.
#[derive(Default)]
struct Batch {
    paths: BTreeSet<PathBuf>,
    refresh: bool,
}

impl Batch {
    fn add(&mut self, root: &Path, event: notify::Result<Event>) {
        let event = match event {
            Ok(e) => e,
            // The watcher lost track of something (queue overflow, limits): resync.
            Err(_) => {
                self.refresh = true;
                return;
            }
        };
        if matches!(event.kind, EventKind::Access(_)) {
            return;
        }
        if event.need_rescan() {
            self.refresh = true;
        }
        for path in event.paths {
            let Ok(rel) = path.strip_prefix(root) else {
                continue;
            };
            if matches!(
                rel.file_name().and_then(|n| n.to_str()),
                Some(".gitignore" | ".ignore")
            ) {
                self.refresh = true;
            }
            if !rel.as_os_str().is_empty() {
                self.paths.insert(rel.to_path_buf());
            }
        }
    }
}

fn run(
    index: &Index,
    root: &Path,
    rx: &Receiver<notify::Result<Event>>,
    stop: &AtomicBool,
    runtime: &Handle,
) {
    loop {
        let first = match rx.recv_timeout(IDLE_POLL) {
            Ok(e) => e,
            Err(RecvTimeoutError::Timeout) if !stop.load(Ordering::Acquire) => continue,
            Err(_) => return,
        };
        let mut batch = Batch::default();
        batch.add(root, first);
        let give_up = Instant::now() + MAX_DEBOUNCE;
        while Instant::now() < give_up {
            match rx.recv_timeout(DEBOUNCE) {
                Ok(e) => batch.add(root, e),
                Err(_) => break,
            }
        }
        for rel in &batch.paths {
            if stop.load(Ordering::Acquire) {
                return;
            }
            // Best effort: a path that cannot be handled now is retried by the next
            // event or the next refresh.
            let _ = runtime.block_on(index.reindex_path(rel));
        }
        if batch.refresh && !stop.load(Ordering::Acquire) {
            let _ = runtime.block_on(index.refresh());
        }
    }
}
