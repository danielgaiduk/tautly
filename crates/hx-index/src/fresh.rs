//! When the index can be trusted: readiness, staleness after shell commands, the
//! post-bash freshness check (a cookie barrier while the watcher runs, a background
//! refresh otherwise), and whether a watcher is alive (spec 7.10).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::store::Index;

/// How long a cookie may take to come back through the watcher before the check falls
/// back to a full refresh.
const COOKIE_TIMEOUT: Duration = Duration::from_secs(2);
/// Where cookies are written, root-relative.
const COOKIE_DIR: &str = ".hx/tmp";
const COOKIE_PREFIX: &str = "cookie-";

pub(crate) struct Freshness {
    /// Set once a refresh has completed since open; the channel wakes [`Index::wait_ready`].
    ready: watch::Sender<bool>,
    ready_flag: AtomicBool,
    trigrams_ready: AtomicBool,
    /// How many times [`Index::mark_stale`] was called.
    stale_marks: AtomicU64,
    /// The highest mark count known to be caught up with (by a refresh or a cookie).
    covered: watch::Sender<u64>,
    /// A background refresh loop is running or about to.
    bg_refresh: AtomicBool,
    /// The id of the running watcher (0: none); see [`Index::watching`].
    watcher: AtomicU64,
    next_watcher: AtomicU64,
    /// Cookies written and not yet seen by the watcher: id to the mark count they cover.
    cookies: Mutex<HashMap<u64, u64>>,
    next_cookie: AtomicU64,
    /// `.hx/tmp/.gitignore` is known to exist.
    cookie_dir_ready: AtomicBool,
}

impl Default for Freshness {
    fn default() -> Self {
        Freshness {
            ready: watch::Sender::new(false),
            ready_flag: AtomicBool::new(false),
            trigrams_ready: AtomicBool::new(false),
            stale_marks: AtomicU64::new(0),
            covered: watch::Sender::new(0),
            bg_refresh: AtomicBool::new(false),
            watcher: AtomicU64::new(0),
            next_watcher: AtomicU64::new(1),
            cookies: Mutex::new(HashMap::new()),
            next_cookie: AtomicU64::new(1),
            cookie_dir_ready: AtomicBool::new(false),
        }
    }
}

impl Freshness {
    pub fn mark_ready(&self) {
        self.ready_flag.store(true, Ordering::SeqCst);
        self.ready.send_replace(true);
    }

    pub fn mark_trigrams_ready(&self) {
        self.trigrams_ready.store(true, Ordering::SeqCst);
    }

    pub fn stale_mark_count(&self) -> u64 {
        self.stale_marks.load(Ordering::SeqCst)
    }

    /// Marks up to `seen` (a mark count read when an operation started) are caught up.
    pub fn cover_marks(&self, seen: u64) {
        self.covered.send_if_modified(|c| {
            let moved = seen > *c;
            *c = (*c).max(seen);
            moved
        });
    }

    /// A new watcher runs from now on; returns its id.
    pub fn watcher_started(&self) -> u64 {
        let id = self.next_watcher.fetch_add(1, Ordering::SeqCst);
        self.watcher.store(id, Ordering::SeqCst);
        id
    }

    /// The watcher `id` stopped or failed; a newer watcher is left alone.
    pub fn watcher_stopped(&self, id: u64) {
        let _ = self
            .watcher
            .compare_exchange(id, 0, Ordering::SeqCst, Ordering::SeqCst);
    }

    /// The watcher saw cookie `id`: everything that happened before it was written has
    /// been processed.
    pub fn cookie_seen(&self, id: u64) {
        let marks = self
            .cookies
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&id);
        if let Some(marks) = marks {
            self.cover_marks(marks);
        }
    }
}

/// The root-relative path of cookie `id`.
pub(crate) fn cookie_path(id: u64) -> PathBuf {
    Path::new(COOKIE_DIR).join(format!("{COOKIE_PREFIX}{id}"))
}

/// The cookie id a root-relative path names, if it is a cookie.
pub(crate) fn cookie_id(rel: &Path) -> Option<u64> {
    if rel.parent() != Some(Path::new(COOKIE_DIR)) {
        return None;
    }
    rel.file_name()?
        .to_str()?
        .strip_prefix(COOKIE_PREFIX)?
        .parse()
        .ok()
}

impl Index {
    /// True once a refresh has completed since this handle was opened: symbols,
    /// references and the workspace tree are current.
    pub fn is_ready(&self) -> bool {
        self.inner.fresh.ready_flag.load(Ordering::SeqCst)
    }

    /// Wait until [`Index::is_ready`] or `timeout` passes; returns whether it is ready.
    pub async fn wait_ready(&self, timeout: Duration) -> bool {
        let mut rx = self.inner.fresh.ready.subscribe();
        matches!(
            tokio::time::timeout(timeout, rx.wait_for(|ready| *ready)).await,
            Ok(Ok(_))
        )
    }

    /// True once [`Index::build_trigrams`] has completed since this handle was opened;
    /// until then [`Index::grep_candidates`] answers [`crate::Candidates::FullScan`].
    pub fn trigrams_ready(&self) -> bool {
        self.inner.fresh.trigrams_ready.load(Ordering::SeqCst)
    }

    /// True while a file watcher started by [`Index::start_watcher`] is running and has
    /// reported no error: only then do changes made outside hx reach the index.
    pub fn watching(&self) -> bool {
        self.inner.fresh.watcher.load(Ordering::SeqCst) != 0
    }

    /// Record that files may have changed behind the index's back (a shell command ran).
    /// Until the changes are known to be caught up, [`Index::is_stale`] is true and callers
    /// should not trust the tree or the trigram candidates for files that may have just
    /// appeared. Catching up is a refresh that started after this call, or (see
    /// [`Index::refresh_stale_in_background`]) a cookie written after it coming back
    /// through the watcher.
    pub fn mark_stale(&self) {
        self.inner.fresh.stale_marks.fetch_add(1, Ordering::SeqCst);
    }

    /// True between [`Index::mark_stale`] and the moment its changes are caught up.
    pub fn is_stale(&self) -> bool {
        self.inner.fresh.stale_mark_count() > *self.inner.fresh.covered.borrow()
    }

    /// Catch up with changes made behind the index's back, in the background (the
    /// post-bash freshness check). While a watcher runs, this writes a cookie file
    /// (`.hx/tmp/cookie-<n>`) and clears the staleness once the watcher reports it: the OS
    /// delivers events in order, so every change made before the cookie has been processed
    /// by then. Without a watcher, or when the cookie does not come back in time, it
    /// refreshes until the index is no longer stale and then builds the postings of files
    /// the refresh changed. Refreshes are coalesced: if a loop is already running, that
    /// loop does one more pass when a mark arrived after its refresh started. A failed
    /// refresh stops the loop (see [`Index::last_error`]); the next call retries. None
    /// outside a Tokio runtime, or when the refresh loop is already running.
    pub fn refresh_stale_in_background(self: &Arc<Self>) -> Option<JoinHandle<()>> {
        let runtime = tokio::runtime::Handle::try_current().ok()?;
        if !self.watching() {
            return self.spawn_refresh_loop(&runtime);
        }
        let this = Arc::clone(self);
        Some(runtime.spawn(async move {
            if this.cookie_barrier().await {
                return;
            }
            let runtime = tokio::runtime::Handle::current();
            if let Some(refresh) = this.spawn_refresh_loop(&runtime) {
                let _ = refresh.await;
            }
        }))
    }

    /// Write a cookie and wait until the watcher has seen it; false when that cannot be
    /// done or takes too long.
    async fn cookie_barrier(&self) -> bool {
        let fresh = &self.inner.fresh;
        let marks = fresh.stale_mark_count();
        let id = fresh.next_cookie.fetch_add(1, Ordering::SeqCst);
        fresh
            .cookies
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id, marks);
        let mut covered = fresh.covered.subscribe();
        let path = self.cookie_path(id).await;
        let written = match path.clone() {
            Some(path) => tokio::task::spawn_blocking(move || std::fs::write(path, b""))
                .await
                .is_ok_and(|r| r.is_ok()),
            None => false,
        };
        if !written {
            // Perhaps `.hx/tmp` went away: set it up again next time.
            fresh.cookie_dir_ready.store(false, Ordering::SeqCst);
        }
        let seen = written
            && matches!(
                tokio::time::timeout(COOKIE_TIMEOUT, covered.wait_for(|c| *c >= marks)).await,
                Ok(Ok(_))
            );
        fresh
            .cookies
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&id);
        if written && !seen {
            // Nobody else will remove it.
            if let Some(path) = path {
                let _ = tokio::task::spawn_blocking(move || std::fs::remove_file(path)).await;
            }
        }
        seen
    }

    /// The absolute path of cookie `id`, with `.hx/tmp` (and its `.gitignore`) in place;
    /// None when it cannot be written there directly (no symlink may redirect it).
    async fn cookie_path(&self, id: u64) -> Option<PathBuf> {
        let sb = &self.inner.sb;
        let fresh = &self.inner.fresh;
        if !fresh.cookie_dir_ready.load(Ordering::SeqCst) {
            let ignore = Path::new(COOKIE_DIR).join(".gitignore");
            let exists = sb.resolve(&ignore).ok().is_some_and(|p| p.exists());
            if !exists {
                sb.write_atomic(&ignore, b"*", None).await.ok()?;
            }
            fresh.cookie_dir_ready.store(true, Ordering::SeqCst);
        }
        let rel = cookie_path(id);
        let path = sb.resolve(&rel).ok()?;
        (path == sb.root().join(&rel)).then_some(path)
    }

    /// The coalesced refresh loop of [`Index::refresh_stale_in_background`].
    fn spawn_refresh_loop(
        self: &Arc<Self>,
        runtime: &tokio::runtime::Handle,
    ) -> Option<JoinHandle<()>> {
        let fresh = &self.inner.fresh;
        if fresh.bg_refresh.swap(true, Ordering::SeqCst) {
            return None;
        }
        let this = Arc::clone(self);
        Some(runtime.spawn(async move {
            let fresh = &this.inner.fresh;
            loop {
                let mut ok = true;
                while this.is_stale() {
                    if this.refresh().await.is_err() {
                        ok = false;
                        break;
                    }
                }
                // Files the refresh found changed are pending; give them postings so they
                // stop being candidates for every search.
                if ok && matches!(this.inner.run(|i| i.has_pending_trigrams()).await, Ok(true)) {
                    let _ = this.build_trigrams().await;
                }
                fresh.bg_refresh.store(false, Ordering::SeqCst);
                // A mark that landed after the last check but before the flag cleared
                // saw the flag set and started nothing: take the loop back.
                if !ok || !this.is_stale() || fresh.bg_refresh.swap(true, Ordering::SeqCst) {
                    break;
                }
            }
        }))
    }
}
