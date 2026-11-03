//! File-system watcher: keeps the index and tree current with edits made outside Tautly.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::runtime::Handle;

use crate::fresh::{cookie_id, cookie_path};
use crate::store::{Index, IndexError, Inner};
use crate::update::{Admission, admission};

/// Quiet time after the last event before a batch is processed.
const DEBOUNCE: Duration = Duration::from_millis(50);
/// A steady stream of events cannot postpone a batch longer than this.
const MAX_DEBOUNCE: Duration = Duration::from_millis(500);
/// How often an idle watcher thread checks whether it was asked to stop.
const IDLE_POLL: Duration = Duration::from_millis(100);

/// Keeps the watcher running; dropping it stops it (and [`Index::watching`] turns false).
pub struct WatchHandle {
    stop: Arc<AtomicBool>,
    id: u64,
    inner: Arc<Inner>,
    // Held for its Drop: closing the OS watch disconnects the event channel.
    _watcher: RecommendedWatcher,
}

impl Drop for WatchHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.inner.fresh.watcher_stopped(self.id);
    }
}

impl Index {
    /// Watch the workspace root recursively. Changes are debounced by 50 ms. Only paths
    /// that can matter are handled: admitted ones, and ones the tree still knows (gone or
    /// newly ignored); events under ignored directories are dropped without any work.
    /// Before anything is re-read, the changed files lose their postings and become
    /// candidates for every search, so a failure while re-reading (recorded in
    /// [`Index::last_error`]) never leaves stale postings vouching for them. Each path is
    /// then brought in line with the disk ([`Index::reindex_file`] for files, a walk for
    /// directories, row removal for paths that are gone or not admitted). A change to an
    /// ignore file in an admitted directory (or to `.git/info/exclude`) triggers a full [`Index::refresh`], and so
    /// does an overflow or error from the OS watcher, and so does any relevant change
    /// while the workspace contains symlinks (the OS reports the target's path, not its
    /// aliases). Must be called inside a Tokio runtime, which the watcher thread borrows
    /// for the index's async work.
    ///
    /// [`Index::watching`] is true from here until the handle is dropped, the watcher
    /// thread ends, or the OS watcher reports an error (it may have missed changes).
    /// Cookies written by [`Index::refresh_stale_in_background`] are answered here.
    pub fn start_watcher(self: Arc<Self>) -> Result<WatchHandle, IndexError> {
        let runtime = Handle::try_current()
            .map_err(|_| IndexError::Task("start_watcher needs a Tokio runtime".into()))?;
        let root = self.inner.sb.root().to_path_buf();
        let (tx, rx) = channel();
        let mut watcher = notify::recommended_watcher(tx)?;
        watcher.watch(&root, RecursiveMode::Recursive)?;
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let id = self.inner.fresh.watcher_started();
        let inner = Arc::clone(&self.inner);
        let spawned = std::thread::Builder::new()
            .name("tautly-index-watch".into())
            .spawn(move || {
                run(&self, &root, &rx, &flag, &runtime, id);
                self.inner.fresh.watcher_stopped(id);
            });
        if let Err(e) = spawned {
            inner.fresh.watcher_stopped(id);
            return Err(IndexError::Io(e));
        }
        Ok(WatchHandle {
            stop,
            id,
            inner,
            _watcher: watcher,
        })
    }
}

/// Changes collected during one debounce window.
#[derive(Default)]
struct Batch {
    paths: BTreeSet<PathBuf>,
    /// The OS watcher lost track of something: resync whatever the paths are.
    refresh: bool,
    /// The OS watcher reported an error: it can no longer vouch for having seen every
    /// change.
    failed: bool,
    /// Cookies seen (see [`Index::refresh_stale_in_background`]).
    cookies: Vec<u64>,
}

impl Batch {
    fn add(&mut self, root: &Path, event: notify::Result<Event>) {
        let event = match event {
            Ok(e) => e,
            // The watcher lost track of something (queue overflow, limits): resync.
            Err(_) => {
                self.refresh = true;
                self.failed = true;
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
            if let Some(id) = cookie_id(rel) {
                self.cookies.push(id);
            } else if !rel.as_os_str().is_empty() {
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
    id: u64,
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
        // A cookie comes after everything it vouches for: no need to wait for more.
        while Instant::now() < give_up && batch.cookies.is_empty() {
            match rx.recv_timeout(DEBOUNCE) {
                Ok(e) => batch.add(root, e),
                Err(_) => break,
            }
        }
        if stop.load(Ordering::Acquire) {
            return;
        }
        if batch.failed {
            index.inner.fresh.watcher_stopped(id);
        }
        runtime.block_on(process(index, batch, stop));
    }
}

fn is_ignore_file(key: &str) -> bool {
    matches!(key.rsplit('/').next(), Some(".gitignore" | ".ignore"))
}

/// The repository directory whose `.git/info/exclude` is `key`, if it is one.
fn git_exclude_repo(key: &str) -> Option<&str> {
    if key == ".git/info/exclude" {
        return Some("");
    }
    key.strip_suffix("/.git/info/exclude")
}

/// Whether directory `dir` is one the walker descends into; memoized for the batch.
/// Ancestors are decided first, shallowest first, so everything under one ignored
/// directory costs a single walk.
async fn dir_admitted(inner: &Inner, cache: &mut HashMap<String, bool>, dir: &str) -> bool {
    let mut prefix = String::new();
    for part in dir.split('/').filter(|p| !p.is_empty()) {
        if !prefix.is_empty() {
            prefix.push('/');
        }
        prefix.push_str(part);
        let admitted = match cache.get(&prefix) {
            Some(&known) => known,
            None => {
                // When in doubt, process the path.
                let admitted = !matches!(
                    admission(inner, &prefix).await,
                    Ok(Admission::No | Admission::File { .. })
                );
                cache.insert(prefix.clone(), admitted);
                admitted
            }
        };
        if !admitted {
            return false;
        }
    }
    true
}

/// Handle one debounced batch. Only paths that can matter are processed: admitted ones,
/// and ones the tree still knows (deleted or newly ignored). Everything under an ignored
/// directory (`target/`, `.git/`) is dropped without touching the database.
async fn process(index: &Index, batch: Batch, stop: &AtomicBool) {
    let inner = &index.inner;
    let mut dirs: HashMap<String, bool> = HashMap::new();
    let mut resync = batch.refresh;
    let mut work: Vec<String> = Vec::new();
    for rel in &batch.paths {
        let key = inner.key(rel);
        if key.is_empty() {
            continue;
        }
        let parent = key.rsplit_once('/').map_or("", |(p, _)| p);
        let parent_ok = dir_admitted(inner, &mut dirs, parent).await;
        if parent_ok && is_ignore_file(&key) {
            resync = true;
        }
        // `.git` itself is never admitted; the exclude file counts if its repository is.
        if let Some(repo) = git_exclude_repo(&key) {
            if dir_admitted(inner, &mut dirs, repo).await {
                resync = true;
            }
        }
        let known = tree_has(inner, &key);
        if !parent_ok && !known {
            continue;
        }
        if !known && matches!(admission(inner, &key).await, Ok(Admission::No)) {
            continue;
        }
        work.push(key);
    }

    // Before anything is re-read, the changed files stop being vouched for by their
    // postings: they are candidates whatever happens next.
    if !work.is_empty() {
        let keys = work.clone();
        let marked = inner
            .run(move |i| {
                i.mark_pending(&keys)?;
                i.mark_pending_under(&keys)
            })
            .await;
        if let Err(e) = marked {
            inner.record_error(format!("watcher: {e}"));
        }
    }
    for key in &work {
        if stop.load(Ordering::Acquire) {
            return;
        }
        // A failure leaves the file pending (still a candidate); it is reported, and the
        // next event or refresh retries.
        if let Err(e) = index.reindex_path(Path::new(key)).await {
            inner.record_error(format!("watcher: {key}: {e}"));
        }
    }
    // With symlinks in the tree one change can alias several paths, and a changed ignore
    // file changes which files are admitted: reconcile with a full refresh.
    resync |= inner.has_links() && !work.is_empty();
    if resync && !stop.load(Ordering::Acquire) && index.refresh().await.is_ok() {
        let _ = index.build_trigrams().await;
    }
    // Everything that happened before these cookies were written is processed now.
    for id in batch.cookies {
        inner.fresh.cookie_seen(id);
        let rel = cookie_path(id);
        let _ = inner.sb.remove_file(&rel).await;
    }
}

fn tree_has(inner: &Inner, key: &str) -> bool {
    inner
        .tree()
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .contains_or_under(key)
}
