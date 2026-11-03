//! Incremental refresh: walk, compare size/mtime, re-parse what changed.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use futures::{StreamExt, stream};
use hx_sandbox::{FileMeta, Sandbox, SandboxError};
use rayon::prelude::*;

use crate::extract::extract;
use crate::lang::Lang;
use crate::store::{Index, IndexError, Inner, ParsedFile, RefreshStats};
use crate::trigram::trigrams;

/// Files larger than this are tracked by size and mtime only (never read).
const MAX_FILE_BYTES: u64 = 1024 * 1024;
/// Files per transaction (and per read/parse batch) during a refresh.
const BATCH_FILES: usize = 500;
/// Bytes of source held in memory per batch.
const MAX_BATCH_BYTES: u64 = 16 * 1024 * 1024;
/// Concurrent sandbox reads.
const IO_CONCURRENCY: usize = 64;
/// Longest stored reference context, in characters.
const MAX_CONTEXT_CHARS: usize = 200;

struct Candidate {
    key: String,
    lang: Option<Lang>,
    mtime: i64,
    size: i64,
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
    pub async fn refresh(&self) -> Result<RefreshStats, IndexError> {
        self.inner.run(|i| i.set_bulk_cache(true)).await?;
        let result = self.refresh_inner().await;
        // Best effort: failing to shrink the cache must not hide the refresh's result.
        let _ = self.inner.run(|i| i.set_bulk_cache(false)).await;
        self.inner.record_refresh(&result);
        result
    }

    async fn refresh_inner(&self) -> Result<RefreshStats, IndexError> {
        let inner = &self.inner;
        let current = scan(inner).await?;
        let known = Arc::new(inner.run(|i| i.known_files()).await?);
        let force = inner.run(|i| i.extractor_changed()).await?;

        let present: HashSet<&str> = current.iter().map(|c| c.key.as_str()).collect();
        let gone: Vec<String> = known
            .keys()
            .filter(|k| !present.contains(k.as_str()))
            .cloned()
            .collect();
        let mut stats = RefreshStats {
            parsed: 0,
            removed: gone.len() as u32,
            unchanged: 0,
        };
        for chunk in gone.chunks(BATCH_FILES) {
            let chunk = chunk.to_vec();
            inner.run(move |i| i.apply(&[], &[], &chunk)).await?;
        }

        let (stale, fresh): (Vec<Candidate>, Vec<Candidate>) = current.into_iter().partition(|c| {
            force
                || known
                    .get(&c.key)
                    .is_none_or(|k| (k.mtime, k.size) != (c.mtime, c.size))
        });
        stats.unchanged = fresh.len() as u32;

        // Files over the cap are never read, so they cost no batch budget.
        let sizes: Vec<u64> = stale
            .iter()
            .map(|c| {
                if is_oversize(c.size) {
                    0
                } else {
                    c.size as u64
                }
            })
            .collect();
        let mut stale = stale.into_iter();
        let mut pos = 0;
        while pos < sizes.len() {
            let n = batch_end(&sizes[pos..], BATCH_FILES, MAX_BATCH_BYTES);
            pos += n;
            let (big, batch): (Vec<Candidate>, Vec<Candidate>) =
                stale.by_ref().take(n).partition(|c| is_oversize(c.size));
            let read = read_all(inner, batch).await;
            let hashes: Vec<Option<String>> = read
                .iter()
                .map(|(c, _)| known.get(&c.key).filter(|_| !force).map(|k| k.hash.clone()))
                .collect();
            let outcomes = tokio::task::spawn_blocking(move || {
                read.into_par_iter()
                    .zip(hashes)
                    .map(|((c, bytes), old)| process(c, &bytes, old.as_deref()))
                    .collect::<Vec<_>>()
            })
            .await
            .map_err(|e| IndexError::Task(e.to_string()))?;

            let (mut parsed, touched) = split(outcomes);
            parsed.extend(big.into_iter().map(oversize_file));
            stats.parsed += parsed.len() as u32;
            stats.unchanged += touched.len() as u32;
            inner.run(move |i| i.apply(&parsed, &touched, &[])).await?;
        }
        if force {
            inner.run(|i| i.record_extractor_version()).await?;
        }
        inner.mark_ready();
        Ok(stats)
    }

    /// Re-index one file now (the post-edit hook). A file that no longer exists or is
    /// no longer admitted (ignored, hidden, outside the root) is dropped from the
    /// index; a directory is ignored.
    pub async fn reindex_file(&self, rel: &Path) -> Result<(), IndexError> {
        let inner = &self.inner;
        let key = inner.key(rel);
        if key.is_empty() {
            return Ok(());
        }
        let meta = match inner.sb.metadata(Path::new(&key)).await {
            Ok(m) => m,
            Err(SandboxError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                return forget(inner, key).await;
            }
            Err(SandboxError::OutsideRoot(_)) => return forget(inner, key).await,
            Err(e) => return Err(e.into()),
        };
        match admission(inner, &key).await? {
            Admission::File => {}
            Admission::Dir => return Ok(()),
            Admission::No => return forget(inner, key).await,
        }
        let cand = candidate(key.clone(), &meta);
        if is_oversize(cand.size) {
            let file = oversize_file(cand);
            return inner.run(move |i| i.apply(&[file], &[], &[])).await;
        }
        let bytes = inner.sb.read(Path::new(&key)).await?;
        let old = inner
            .run(move |i| i.known_file(&key))
            .await?
            .map(|k| k.hash);
        let outcome = tokio::task::spawn_blocking(move || process(cand, &bytes, old.as_deref()))
            .await
            .map_err(|e| IndexError::Task(e.to_string()))?;
        let (parsed, touched) = split(vec![outcome]);
        inner.run(move |i| i.apply(&parsed, &touched, &[])).await
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
            Admission::File => self.reindex_file(Path::new(&key)).await,
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
                for file in &found {
                    self.reindex_file(Path::new(file)).await?;
                }
                Ok(())
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
        text: false,
        symbols: Vec::new(),
        refs: Vec::new(),
        lines: Vec::new(),
        trigrams: Vec::new(),
    }
}

fn candidate(key: String, meta: &FileMeta) -> Candidate {
    let mtime = meta
        .mtime
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as i64);
    Candidate {
        lang: Lang::from_path(Path::new(&key)),
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
    builder.require_git(false);
    Ok(builder)
}

enum Admission {
    /// A regular file the walker visits.
    File,
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
async fn admission(inner: &Inner, key: &str) -> Result<Admission, IndexError> {
    let sb = Arc::clone(&inner.sb);
    let key = key.to_string();
    tokio::task::spawn_blocking(move || {
        let parts: Vec<&str> = key.split('/').collect();
        let mut dir = String::new();
        for (i, part) in parts.iter().enumerate() {
            let mut builder = walker_at(sb.as_ref(), &dir)?;
            builder.max_depth(Some(1));
            let entry = builder
                .build()
                .flatten()
                .find(|e| e.depth() == 1 && e.file_name() == *part);
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
                Some(t) if t.is_file() && last => return Ok(Admission::File),
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
/// file.
async fn scan(inner: &Inner) -> Result<Vec<Candidate>, IndexError> {
    let builder = walker_at(inner.sb.as_ref(), "")?;
    let root = inner.sb.root().to_path_buf();
    let paths = tokio::task::spawn_blocking(move || {
        let mut out = Vec::new();
        for entry in builder.build().flatten() {
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            let Ok(rel) = entry.path().strip_prefix(&root) else {
                continue;
            };
            out.push(rel.to_path_buf());
        }
        out
    })
    .await
    .map_err(|e| IndexError::Task(e.to_string()))?;

    let sb = &inner.sb;
    let stats: Vec<Option<Candidate>> = stream::iter(paths)
        .map(|rel| async move {
            let meta = sb.metadata(&rel).await.ok()?;
            Some(candidate(inner.key(&rel), &meta))
        })
        .buffer_unordered(IO_CONCURRENCY)
        .collect()
        .await;
    Ok(stats.into_iter().flatten().collect())
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

/// Hash, decode and (when the content changed) parse and trigram one file. Content that
/// is over the cap (it grew since the stat) or not UTF-8 is recorded by hash only.
fn process(c: Candidate, bytes: &[u8], old_hash: Option<&str>) -> Outcome {
    let hash = blake3::hash(bytes).to_hex().to_string();
    if old_hash == Some(hash.as_str()) {
        return Outcome::Touch(c.key, c.mtime, c.size);
    }
    let src = if bytes.len() as u64 > MAX_FILE_BYTES {
        None
    } else {
        std::str::from_utf8(bytes).ok()
    };
    let Some(src) = src else {
        let mut file = oversize_file(c);
        file.hash = hash;
        return Outcome::Parsed(Box::new(file));
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
        text: true,
        symbols,
        refs: extracted.refs,
        lines,
        trigrams: trigrams(bytes),
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
