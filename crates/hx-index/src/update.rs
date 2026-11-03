//! Incremental refresh: walk, compare size/mtime, re-parse what changed.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use futures::{StreamExt, stream};
use hx_sandbox::{FileMeta, Sandbox, SandboxError};
use rayon::prelude::*;
use std::os::unix::fs::MetadataExt;

use crate::extract::extract;
use crate::lang::Lang;
use crate::store::{
    Index, IndexError, Inner, ParsedFile, RefreshStats, TRI_DONE, TRI_NONE, TRI_PENDING,
};
use crate::trigram::trigrams;

/// Files larger than this are tracked by size and mtime only (never read).
const MAX_FILE_BYTES: u64 = 1024 * 1024;
/// Files per transaction (and per read/parse batch) during a refresh.
const BATCH_FILES: usize = 500;
/// Bytes of source held in memory per batch.
const MAX_BATCH_BYTES: u64 = 16 * 1024 * 1024;
/// A refresh of an index that is not ready yet, with at least this many files to
/// (re)write, drops the name indexes for the duration and rebuilds them at the end.
const BULK_REBUILD_FILES: usize = 2000;
/// A refresh or postings build with at least this many files to (re)write gets the large
/// SQLite page cache for the duration; smaller ones keep the idle cache.
const BULK_CACHE_FILES: usize = 2000;
/// Concurrent sandbox reads.
const IO_CONCURRENCY: usize = 64;
/// Longest stored reference context, in characters.
const MAX_CONTEXT_CHARS: usize = 200;

struct Candidate {
    key: String,
    lang: Option<Lang>,
    mtime: i64,
    size: i64,
    /// Reached through a symlink (the file itself or a directory on the way), or hard
    /// linked: its content can change through another name without this one being told,
    /// so the trigram index never vouches for it.
    aliased: bool,
}

/// What to do with one re-read file.
enum Outcome {
    /// Content unchanged since it was indexed; only refresh mtime and size.
    Touch(String, i64, i64),
    Parsed(Box<ParsedFile>),
}

impl Index {
    /// Bring the index up to date with the sandbox root: every admitted file is a row
    /// (and a tree entry); files whose size or mtime changed are re-read (and, if the
    /// content hash changed, re-parsed and re-trigrammed); rows of files that are gone
    /// or no longer admitted are dropped. Files over 1 MB are recorded without being
    /// read; files that are not UTF-8 are recorded by hash only. `unchanged` counts
    /// files left as they were. After a change of extractor version every file is
    /// reparsed. A failure is also recorded in [`Index::last_error`] and leaves
    /// `is_ready` as it was.
    ///
    /// Refreshes of one index run one at a time. A refresh never undoes a write made while
    /// it runs: rows another writer (an edit's re-index, the watcher) wrote or removed after
    /// the refresh started are left as that writer left them.
    pub async fn refresh(&self) -> Result<RefreshStats, IndexError> {
        let _one_at_a_time = self.inner.refresh_lock.lock().await;
        let marks_seen = self.inner.stale_mark_count();
        let mut result = match self.inner.run(|i| i.begin_refresh()).await {
            Ok(since) => self.refresh_inner(since).await,
            Err(e) => Err(e),
        };
        self.inner.end_refresh();
        // A bulk build may have dropped the name indexes; they must be back whatever
        // happened (a no-op otherwise).
        if let Err(e) = self.inner.run(|i| i.ensure_indexes()).await {
            result = result.and(Err(e));
        }
        // Best effort: failing to shrink the cache must not hide the refresh's result.
        let _ = self.inner.run(|i| i.set_bulk_cache(false)).await;
        self.inner.record_refresh(&result);
        if result.is_ok() {
            self.inner.cover_marks(marks_seen);
            self.inner.count_refresh();
        }
        result
    }

    /// Phase 2 of building the index: read every text file whose trigram postings are
    /// missing (all of them after a first [`Index::refresh`] or an extractor-version
    /// change; the changed files after a later one) and store them. Extraction runs on
    /// the rayon pool and the next batch is read while the previous one is written.
    /// Returns how many files got postings; afterwards [`Index::trigrams_ready`] is
    /// true and [`Index::grep_candidates`] narrows searches. Files edited since the
    /// refresh are skipped (they stay candidates until the next refresh re-reads them).
    /// Edits made through [`Index::reindex_file`] never wait for this phase.
    pub async fn build_trigrams(&self) -> Result<u32, IndexError> {
        let result = self.build_trigrams_inner().await;
        let _ = self.inner.run(|i| i.set_bulk_cache(false)).await;
        if result.is_ok() {
            self.inner.mark_trigrams_ready();
        }
        result
    }

    async fn build_trigrams_inner(&self) -> Result<u32, IndexError> {
        let inner = &self.inner;
        let pending = inner.run(|i| i.pending_trigram_files()).await?;
        // A large cache only pays for a large build; an idle index stays small.
        if pending.len() >= BULK_CACHE_FILES {
            inner.run(|i| i.set_bulk_cache(true)).await?;
        }
        let sizes: Vec<u64> = pending.iter().map(|p| p.size as u64).collect();
        let mut pending = pending.into_iter();
        let mut pos = 0;
        let mut write: Option<Write<Vec<String>>> = None;
        let mut done = 0u32;
        while pos < sizes.len() {
            let n = batch_end(&sizes[pos..], BATCH_FILES, MAX_BATCH_BYTES);
            pos += n;
            let batch: Vec<_> = pending.by_ref().take(n).collect();
            let candidates = batch
                .iter()
                .map(|p| Candidate {
                    key: p.key.clone(),
                    lang: None,
                    mtime: 0,
                    size: p.size,
                    aliased: false,
                })
                .collect();
            let read = read_all(inner, candidates).await;
            let expect: HashMap<String, (i64, String)> =
                batch.into_iter().map(|p| (p.key, (p.id, p.hash))).collect();
            let items = tokio::task::spawn_blocking(move || {
                read.into_par_iter()
                    .filter_map(|(c, bytes)| {
                        let (id, hash) = expect.get(&c.key)?;
                        // Only content that is still what the row describes.
                        if blake3::hash(&bytes).to_hex().as_str() != hash.as_str() {
                            return None;
                        }
                        Some((*id, c.key, hash.clone(), trigrams(&bytes)))
                    })
                    .collect::<Vec<_>>()
            })
            .await
            .map_err(|e| IndexError::Task(e.to_string()))?;
            if let Some(prev) = write.take() {
                done += prev.finish().await?.len() as u32;
            }
            write = Some(Write::start(inner, move |i| i.apply_trigrams(&items)));
        }
        if let Some(prev) = write.take() {
            done += prev.finish().await?.len() as u32;
        }
        Ok(done)
    }

    async fn refresh_inner(&self, since: u32) -> Result<RefreshStats, IndexError> {
        let inner = &self.inner;
        let current = scan(inner).await?;
        inner.scan_hold_point().await;
        let force = inner.run(|i| i.extractor_changed()).await?;

        // Compare with the in-memory tree (which has every row's size and mtime) instead
        // of reading the whole table.
        let Diff {
            stale,
            unchanged,
            gone,
        } = diff(inner, current, force);
        let mut stats = RefreshStats {
            parsed: 0,
            removed: gone.len() as u32,
            unchanged,
        };
        for chunk in gone.chunks(BATCH_FILES) {
            let chunk = chunk.to_vec();
            inner
                .run(move |i| i.apply_guarded(&[], &[], &chunk, Some(since)))
                .await?;
        }
        if stale.len() >= BULK_CACHE_FILES {
            inner.run(|i| i.set_bulk_cache(true)).await?;
        }
        if !self.is_ready() && stale.len() >= BULK_REBUILD_FILES {
            inner.run(|i| i.drop_name_indexes()).await?;
        }

        // Files over the cap are never read, so they cost no batch budget.
        let sizes: Vec<u64> = stale
            .iter()
            .map(|(c, _)| {
                if is_oversize(c.size) {
                    0
                } else {
                    c.size as u64
                }
            })
            .collect();
        let mut stale = stale.into_iter();
        let mut pos = 0;
        let mut write: Option<Write<()>> = None;
        while pos < sizes.len() {
            let n = batch_end(&sizes[pos..], BATCH_FILES, MAX_BATCH_BYTES);
            pos += n;
            let (big, batch): (Vec<Known>, Vec<Known>) = stale
                .by_ref()
                .take(n)
                .partition(|(c, _)| is_oversize(c.size));
            let known: HashSet<String> = batch
                .iter()
                .filter(|(_, known)| *known)
                .map(|(c, _)| c.key.clone())
                .collect();
            let wanted: Vec<String> = batch.iter().map(|(c, _)| c.key.clone()).collect();
            let read = read_all(inner, batch.into_iter().map(|(c, _)| c).collect()).await;
            // A known file that changed but could not be read: its old postings no
            // longer describe it, so it must become a candidate.
            let readable: HashSet<&str> = read.iter().map(|(c, _)| c.key.as_str()).collect();
            let unreadable: Vec<String> = wanted
                .iter()
                .filter(|k| !readable.contains(k.as_str()) && known.contains(k.as_str()))
                .cloned()
                .collect();
            // The stored hash and trigram state of the known files that were read, which
            // decide whether a re-read file needs parsing.
            let lookup: Vec<String> = read
                .iter()
                .map(|(c, _)| c.key.clone())
                .filter(|k| !force && known.contains(k))
                .collect();
            let mut stored: HashMap<String, (String, u8)> = HashMap::new();
            if !lookup.is_empty() {
                let rows = inner
                    .run(move |i| Ok((i.known_files_for(&lookup)?, lookup)))
                    .await?;
                for (row, key) in rows.0.into_iter().zip(rows.1) {
                    if let Some(k) = row {
                        stored.insert(key, (k.hash, k.tri_state));
                    }
                }
            }
            let olds: Vec<Option<(String, u8)>> =
                read.iter().map(|(c, _)| stored.remove(&c.key)).collect();
            let outcomes = tokio::task::spawn_blocking(move || {
                read.into_par_iter()
                    .zip(olds)
                    .map(|((c, bytes), old)| {
                        let old = old.as_ref().map(|(h, s)| (h.as_str(), *s));
                        process(c, &bytes, old, false)
                    })
                    .collect::<Vec<_>>()
            })
            .await
            .map_err(|e| IndexError::Task(e.to_string()))?;

            let (mut parsed, touched) = split(outcomes);
            parsed.extend(big.into_iter().map(|(c, _)| oversize_file(c)));
            stats.parsed += parsed.len() as u32;
            stats.unchanged += touched.len() as u32;
            // Write this batch while the next one is read and parsed.
            if let Some(prev) = write.take() {
                prev.finish().await?;
            }
            write = Some(Write::start(inner, move |i| {
                i.apply_guarded(&parsed, &touched, &[], Some(since))?;
                i.mark_pending(&unreadable)
            }));
        }
        if let Some(prev) = write.take() {
            prev.finish().await?;
        }
        if force {
            inner.run(|i| i.record_extractor_version()).await?;
        }
        inner.mark_ready();
        Ok(stats)
    }

    /// Re-index one file now (the post-edit hook). A file that no longer exists or is
    /// no longer admitted (ignored, hidden, outside the root) is dropped from the
    /// index; a directory is ignored. When the path is another name for a file (a
    /// symlink, admitted or not, or a differently-cased spelling), the canonical in-root
    /// path of that file is re-indexed too: it is the only row that can hold postings
    /// for it, and nobody told it about the change. Whenever the file cannot be re-read
    /// (metadata, walk or read fails) its old postings stop vouching for it and the
    /// error is returned.
    ///
    /// Known limitation: a hard link created after the file was indexed, then edited in
    /// place (same inode) with no watcher running, keeps stale postings until the next
    /// [`Index::refresh`] sees the new link count. hx's own edits are atomic renames,
    /// which break hard links, so they never hit this.
    pub async fn reindex_file(&self, rel: &Path) -> Result<(), IndexError> {
        let key = self.inner.key(rel);
        if let Some(canonical) = self.reindex_one(key).await? {
            self.reindex_one(canonical).await?;
        }
        Ok(())
    }

    /// [`Index::reindex_file`] for one path; returns the canonical path to follow when
    /// this one is another name for it.
    async fn reindex_one(&self, key: String) -> Result<Option<String>, IndexError> {
        let inner = &self.inner;
        if key.is_empty() {
            return Ok(None);
        }
        // Whatever happens to this name, the file it leads to may have changed. This
        // does not depend on this name being admitted (hidden or ignored links, other
        // spellings) or even existing (a delete through a link).
        let canonical = canonical_key(inner, &key).filter(|c| *c != key);
        if let Some(c) = &canonical {
            distrust(inner, vec![c.clone()]).await;
        }
        let meta = match inner.sb.metadata(Path::new(&key)).await {
            Ok(m) => m,
            Err(SandboxError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                return forget(inner, key).await.map(|()| canonical);
            }
            Err(SandboxError::OutsideRoot(_)) => {
                return forget(inner, key).await.map(|()| canonical);
            }
            Err(e) => {
                distrust(inner, vec![key]).await;
                return Err(e.into());
            }
        };
        let aliased = match admission(inner, &key).await {
            Ok(Admission::File { aliased }) => aliased,
            Ok(Admission::Dir) => return Ok(None),
            Ok(Admission::No) => return forget(inner, key).await.map(|()| canonical),
            Err(e) => {
                distrust(inner, vec![key]).await;
                return Err(e);
            }
        };
        let cand = candidate(key.clone(), &meta, aliased);
        if is_oversize(cand.size) {
            let file = oversize_file(cand);
            inner.run(move |i| i.apply(&[file], &[], &[])).await?;
            return Ok(canonical);
        }
        let bytes = match inner.sb.read(Path::new(&key)).await {
            Ok(b) => b,
            Err(e) => {
                distrust(inner, vec![key]).await;
                return Err(e.into());
            }
        };
        let old = {
            let key = key.clone();
            inner.run(move |i| i.known_file(&key)).await?
        };
        let outcome = tokio::task::spawn_blocking(move || {
            let old = old.as_ref().map(|k| (k.hash.as_str(), k.tri_state));
            process(cand, &bytes, old, true)
        })
        .await
        .map_err(|e| IndexError::Task(e.to_string()))?;
        let (parsed, touched) = split(vec![outcome]);
        inner.run(move |i| i.apply(&parsed, &touched, &[])).await?;
        Ok(canonical)
    }

    /// Bring whatever is at `rel` in line with the disk: a file is re-indexed, a
    /// directory is walked (new files indexed, vanished ones dropped), and a path that
    /// is gone or not admitted loses its row and everything under it. This is what the
    /// watcher runs for each changed path.
    pub(crate) async fn reindex_path(&self, rel: &Path) -> Result<(), IndexError> {
        let inner = &self.inner;
        let key = inner.key(rel);
        if key.is_empty() {
            return Ok(());
        }
        match admission(inner, &key).await? {
            Admission::File { .. } => self.reindex_file(Path::new(&key)).await,
            Admission::Dir => {
                let found = files_under(inner, &key).await?;
                let live: HashSet<&str> = found.iter().map(String::as_str).collect();
                let gone: Vec<String> = tree_keys_under(inner, &key)
                    .into_iter()
                    .filter(|k| !live.contains(k.as_str()))
                    .collect();
                if !gone.is_empty() {
                    inner.run(move |i| i.apply(&[], &[], &gone)).await?;
                }
                // One unreadable file must not hide the rest of the directory; the first
                // failure is the one reported.
                let mut first_error = None;
                for file in &found {
                    if let Err(e) = self.reindex_file(Path::new(file)).await {
                        first_error.get_or_insert(e);
                    }
                }
                first_error.map_or(Ok(()), Err)
            }
            Admission::No => {
                let mut gone = tree_keys_under(inner, &key);
                let known = {
                    let key = key.clone();
                    inner.run(move |i| i.known_file(&key)).await?.is_some()
                };
                if known {
                    gone.push(key);
                }
                if gone.is_empty() {
                    return Ok(());
                }
                inner.run(move |i| i.apply(&[], &[], &gone)).await
            }
        }
    }
}

/// Make these files candidates for every search (best effort: the caller is already
/// handling a failure).
async fn distrust(inner: &Arc<Inner>, keys: Vec<String>) {
    let _ = inner.run(move |i| i.mark_pending(&keys)).await;
}

/// The root-relative path of `key` with every symlink resolved.
fn canonical_key(inner: &Inner, key: &str) -> Option<String> {
    let real = inner.sb.resolve(Path::new(key)).ok()?;
    let rel = real.strip_prefix(inner.sb.root()).ok()?;
    Some(
        rel.components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/"),
    )
}

/// Drop the rows of `key` if it has any; a path that was never indexed costs a lookup,
/// not a write.
async fn forget(inner: &Arc<Inner>, key: String) -> Result<(), IndexError> {
    inner
        .run(move |i| {
            if i.known_file(&key)?.is_none() {
                return Ok(());
            }
            i.apply(&[], &[], &[key])
        })
        .await
}

fn tree_keys_under(inner: &Inner, dir: &str) -> Vec<String> {
    inner
        .tree()
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .keys_under(dir)
}

fn is_oversize(size: i64) -> bool {
    size as u64 > MAX_FILE_BYTES
}

/// The row for a file over the size cap: size and mtime only, no hash, never read.
fn oversize_file(c: Candidate) -> ParsedFile {
    ParsedFile {
        path: c.key,
        hash: String::new(),
        mtime: c.mtime,
        size: c.size,
        lang: c.lang,
        tri_state: TRI_NONE,
        symbols: Vec::new(),
        refs: Vec::new(),
        lines: Vec::new(),
        trigrams: Vec::new(),
    }
}

/// A database write running in the background, so the next batch can be read and parsed
/// meanwhile.
struct Write<T>(tokio::task::JoinHandle<Result<T, IndexError>>);

impl<T: Send + 'static> Write<T> {
    fn start(
        inner: &Arc<Inner>,
        f: impl FnOnce(&Inner) -> Result<T, IndexError> + Send + 'static,
    ) -> Write<T> {
        let inner = Arc::clone(inner);
        Write(tokio::spawn(async move { inner.run(f).await }))
    }

    async fn finish(self) -> Result<T, IndexError> {
        self.0.await.map_err(|e| IndexError::Task(e.to_string()))?
    }
}

fn candidate(key: String, meta: &FileMeta, aliased: bool) -> Candidate {
    let mtime = meta
        .mtime
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as i64);
    Candidate {
        lang: Lang::from_path(Path::new(&key)),
        // Hard links: the content is reachable under names this index is never told about.
        aliased: aliased || meta.nlink > 1,
        key,
        mtime,
        size: meta.len as i64,
    }
}

/// A walker over `dir` (root-relative, empty for the root), configured the same way for
/// every use so a refresh, a single-file reindex and the watcher agree on which files
/// exist. It is the sandbox's own walker, so the sandbox's entry filter stays in force.
fn walker_at(sb: &dyn Sandbox, dir: &str) -> Result<ignore::WalkBuilder, IndexError> {
    let mut builder = sb.walker(Path::new(if dir.is_empty() { "." } else { dir }))?;
    // Same link handling as the grep and glob tools, so the tree and the trigram index
    // are never a subset of what they see. Links leaving the root are dropped by the
    // sandbox's own entry filter; the walker detects link cycles.
    builder.require_git(false).follow_links(true);
    Ok(builder)
}

pub(crate) enum Admission {
    /// A regular file the walker visits; `aliased` when it, or a directory on the way,
    /// is a symlink.
    File { aliased: bool },
    /// A directory the walker descends into.
    Dir,
    /// Missing, ignored, hidden, filtered by the sandbox, or not a regular file.
    No,
}

/// Whether the walker used by `refresh` would visit `key`, and as what. Walks down to it
/// one directory at a time, each level with a walker from the sandbox (so its filter is
/// never replaced) and ignore rules that still see the parents' ignore files, which
/// makes a hidden or ignored directory on the way hide everything below it, exactly as
/// the full walk does.
pub(crate) async fn admission(inner: &Inner, key: &str) -> Result<Admission, IndexError> {
    let sb = Arc::clone(&inner.sb);
    let key = key.to_string();
    tokio::task::spawn_blocking(move || {
        let parts: Vec<&str> = key.split('/').collect();
        let mut dir = String::new();
        let mut aliased = false;
        for (i, part) in parts.iter().enumerate() {
            let mut builder = walker_at(sb.as_ref(), &dir)?;
            builder.max_depth(Some(1));
            let entry = builder
                .build()
                .flatten()
                .find(|e| e.depth() == 1 && e.file_name() == *part);
            aliased |= entry.as_ref().is_some_and(|e| e.path_is_symlink());
            let kind = entry.as_ref().and_then(|e| e.file_type());
            let last = i + 1 == parts.len();
            match kind {
                Some(t) if t.is_dir() && !last => {
                    if !dir.is_empty() {
                        dir.push('/');
                    }
                    dir.push_str(part);
                }
                Some(t) if t.is_dir() => return Ok(Admission::Dir),
                Some(t) if t.is_file() && last => return Ok(Admission::File { aliased }),
                _ => return Ok(Admission::No),
            }
        }
        Ok(Admission::No)
    })
    .await
    .map_err(|e| IndexError::Task(e.to_string()))?
}

/// Root-relative keys of the admitted files under the (admitted) directory `dir`.
async fn files_under(inner: &Inner, dir: &str) -> Result<Vec<String>, IndexError> {
    let builder = walker_at(inner.sb.as_ref(), dir)?;
    let root = inner.sb.root().to_path_buf();
    tokio::task::spawn_blocking(move || {
        builder
            .build()
            .flatten()
            .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
            .filter_map(|e| {
                let rel = e.path().strip_prefix(&root).ok()?;
                Some(
                    rel.components()
                        .map(|c| c.as_os_str().to_string_lossy())
                        .collect::<Vec<_>>()
                        .join("/"),
                )
            })
            .collect()
    })
    .await
    .map_err(|e| IndexError::Task(e.to_string()))
}

/// Walk the root (honoring ignore files, even outside a git checkout) and stat every
/// file. The stat data comes from the walk itself (the sandbox's walker, which already
/// enforces confinement), so no per-file sandbox call is made.
async fn scan(inner: &Inner) -> Result<Vec<Candidate>, IndexError> {
    let builder = walker_at(inner.sb.as_ref(), "")?;
    let root = inner.sb.root().to_path_buf();
    let (found, links) = tokio::task::spawn_blocking(move || {
        let mut out = Vec::new();
        // Symlinked directories seen so far (the walk is depth-first, so a directory
        // comes before its contents).
        let mut link_dirs: HashSet<std::path::PathBuf> = HashSet::new();
        let mut any_alias = false;
        for entry in builder.build().flatten() {
            let is_link = entry.path_is_symlink();
            let Ok(rel) = entry.path().strip_prefix(&root) else {
                continue;
            };
            if is_link && entry.file_type().is_some_and(|t| t.is_dir()) {
                link_dirs.insert(rel.to_path_buf());
            }
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            let aliased = is_link || rel.ancestors().skip(1).any(|a| link_dirs.contains(a));
            any_alias |= aliased;
            // Follows the link when the walker does (it is configured to).
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            let Ok(mtime) = meta.modified() else {
                continue;
            };
            let meta = FileMeta {
                mtime,
                len: meta.len(),
                mode: meta.mode(),
                nlink: meta.nlink(),
            };
            out.push((rel_key(rel), meta, aliased));
        }
        let links = !link_dirs.is_empty() || any_alias;
        (out, links)
    })
    .await
    .map_err(|e| IndexError::Task(e.to_string()))?;
    inner.set_has_links(links);
    Ok(found
        .into_iter()
        .map(|(key, meta, aliased)| candidate(key, &meta, aliased))
        .collect())
}

/// The `/`-separated key of a root-relative path, as [`Inner::key`] spells it.
fn rel_key(rel: &Path) -> String {
    rel.components()
        .filter_map(|c| match c {
            std::path::Component::CurDir => None,
            c => Some(c.as_os_str().to_string_lossy()),
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// A file to re-read, with whether the tree knew it.
type Known = (Candidate, bool);

/// What a refresh has to do, from comparing a scan with the tree.
struct Diff {
    stale: Vec<Known>,
    unchanged: u32,
    /// Rows of files that are gone.
    gone: Vec<String>,
}

/// Compare a scan with the in-memory tree. `force` (a new extractor) makes every file
/// stale.
fn diff(inner: &Inner, current: Vec<Candidate>, force: bool) -> Diff {
    let tree = inner
        .tree()
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut gone = Vec::new();
    {
        let present: HashSet<&str> = current.iter().map(|c| c.key.as_str()).collect();
        let mut key = String::new();
        tree.for_each(|dir, name, _| {
            key.clear();
            if !dir.is_empty() {
                key.push_str(dir);
                key.push('/');
            }
            key.push_str(name);
            if !present.contains(key.as_str()) {
                gone.push(key.clone());
            }
        });
    }
    let mut stale = Vec::new();
    let mut unchanged = 0;
    for c in current {
        match tree.get(&c.key) {
            Some(slot) if !force && slot.same_stat(c.size, c.mtime) => unchanged += 1,
            slot => stale.push((c, slot.is_some())),
        }
    }
    Diff {
        stale,
        unchanged,
        gone,
    }
}

/// Read a batch of files; ones that vanished or cannot be read are left out.
async fn read_all(inner: &Inner, batch: Vec<Candidate>) -> Vec<(Candidate, Vec<u8>)> {
    let sb = &inner.sb;
    stream::iter(batch)
        .map(|c| async move {
            let bytes = sb.read(Path::new(&c.key)).await.ok()?;
            Some((c, bytes))
        })
        .buffer_unordered(IO_CONCURRENCY)
        .filter_map(|r| async move { r })
        .collect()
        .await
}

type Split = (Vec<ParsedFile>, Vec<(String, i64, i64)>);

fn split(outcomes: Vec<Outcome>) -> Split {
    let mut out: Split = (Vec::new(), Vec::new());
    for o in outcomes {
        match o {
            Outcome::Parsed(p) => out.0.push(*p),
            Outcome::Touch(k, m, s) => out.1.push((k, m, s)),
        }
    }
    out
}

/// Hash, decode and (when the content changed) parse one file; with `with_trigrams` its
/// postings are computed too, otherwise the file is left pending for
/// [`Index::build_trigrams`]. Content that is over the cap (it grew since the stat) or
/// not UTF-8 is recorded by hash only, and a file reached through a symlink never gets
/// postings (see [`Candidate::aliased`]). `old` is the stored (hash, trigram state); a
/// row is only left alone if the content is unchanged and the row is already settled
/// (a pending row, e.g. one the watcher marked before re-reading, is rebuilt).
fn process(c: Candidate, bytes: &[u8], old: Option<(&str, u8)>, with_trigrams: bool) -> Outcome {
    let hash = blake3::hash(bytes).to_hex().to_string();
    let src = if bytes.len() as u64 > MAX_FILE_BYTES {
        None
    } else {
        std::str::from_utf8(bytes).ok()
    };
    let wants_postings = src.is_some() && !c.aliased;
    if let Some((old_hash, old_state)) = old {
        // Unchanged content leaves the row alone only if the row is already what this
        // call would write: postings in place (or, for a refresh, left for phase 2).
        let settled = if wants_postings {
            old_state == TRI_DONE || (!with_trigrams && old_state == TRI_PENDING)
        } else {
            old_state == TRI_NONE
        };
        if old_hash == hash && settled {
            return Outcome::Touch(c.key, c.mtime, c.size);
        }
    }
    let Some(src) = src else {
        let mut file = oversize_file(c);
        file.hash = hash;
        return Outcome::Parsed(Box::new(file));
    };
    let tri_state = if !wants_postings {
        TRI_NONE
    } else if with_trigrams {
        TRI_DONE
    } else {
        TRI_PENDING
    };
    let extracted = match c.lang {
        Some(lang) if lang.has_symbols() => extract(lang, src),
        _ => Default::default(),
    };
    let lines: Vec<&str> = src.lines().collect();
    let mut context: HashMap<u32, String> = HashMap::new();
    for (line, _) in &extracted.refs {
        context.entry(*line).or_insert_with(|| {
            let text = lines.get(*line as usize - 1).copied().unwrap_or("");
            text.trim().chars().take(MAX_CONTEXT_CHARS).collect()
        });
    }
    let mut lines: Vec<(u32, String)> = context.into_iter().collect();
    lines.sort_unstable();
    let mut symbols = extracted.symbols;
    let path = std::path::PathBuf::from(&c.key);
    for s in &mut symbols {
        s.path = path.clone();
    }
    Outcome::Parsed(Box::new(ParsedFile {
        path: c.key,
        hash,
        mtime: c.mtime,
        size: c.size,
        lang: c.lang,
        tri_state,
        symbols,
        refs: extracted.refs,
        lines,
        trigrams: if tri_state == TRI_DONE {
            trigrams(bytes)
        } else {
            Vec::new()
        },
    }))
}

/// How many of the leading files (at least one) form the next refresh batch: at most
/// `max_files` files and, once the batch has one file, at most `max_bytes` in total.
fn batch_end(sizes: &[u64], max_files: usize, max_bytes: u64) -> usize {
    let mut total = 0u64;
    let mut n = 0;
    for &size in sizes.iter().take(max_files) {
        if n > 0 && total.saturating_add(size) > max_bytes {
            break;
        }
        total = total.saturating_add(size);
        n += 1;
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1024 * 1024;

    #[test]
    fn batches_close_at_the_byte_budget() {
        let sizes = [5 * MIB; 6];
        assert_eq!(batch_end(&sizes, 500, 16 * MIB), 3);
        assert_eq!(batch_end(&sizes[3..], 500, 16 * MIB), 3);
    }

    #[test]
    fn batches_close_at_the_file_limit() {
        let sizes = vec![10u64; 1200];
        assert_eq!(batch_end(&sizes, 500, 16 * MIB), 500);
        assert_eq!(batch_end(&sizes[1100..], 500, 16 * MIB), 100);
    }

    #[test]
    fn an_oversized_file_still_forms_a_batch_of_one() {
        assert_eq!(batch_end(&[20 * MIB, 1, 1], 500, 16 * MIB), 1);
        assert_eq!(batch_end(&[MIB, 20 * MIB], 500, 16 * MIB), 1);
    }

    #[test]
    fn no_batch_ever_exceeds_the_budget_or_file_limit() {
        let sizes: Vec<u64> = (0..2000u64)
            .map(|i| (i * 7919) % (1024 * 1024) + 1)
            .collect();
        let mut pos = 0;
        while pos < sizes.len() {
            let n = batch_end(&sizes[pos..], BATCH_FILES, MAX_BATCH_BYTES);
            assert!((1..=BATCH_FILES).contains(&n));
            assert!(sizes[pos..pos + n].iter().sum::<u64>() <= MAX_BATCH_BYTES);
            pos += n;
        }
    }
}
