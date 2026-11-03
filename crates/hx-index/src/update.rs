//! Incremental refresh: walk, compare size/mtime, re-parse what changed.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use futures::{StreamExt, stream};
use hx_sandbox::{FileMeta, SandboxError};
use rayon::prelude::*;

use crate::extract::extract;
use crate::lang::Lang;
use crate::store::{Index, IndexError, Inner, ParsedFile, RefreshStats};

/// Files larger than this are not indexed.
const MAX_FILE_BYTES: u64 = 1024 * 1024;
/// Files per transaction (and per read/parse batch) during a refresh.
const BATCH_FILES: usize = 500;
/// Concurrent sandbox reads.
const IO_CONCURRENCY: usize = 64;
/// Longest stored reference context, in characters.
const MAX_CONTEXT_CHARS: usize = 200;

struct Candidate {
    key: String,
    lang: Lang,
    mtime: i64,
    size: i64,
}

/// What to do with one re-read file.
enum Outcome {
    /// Content unchanged since it was indexed; only refresh mtime and size.
    Touch(String, i64, i64),
    Parsed(Box<ParsedFile>),
    /// Not UTF-8 (or grew past the size cap): not indexed.
    Skip(String),
}

impl Index {
    /// Bring the index up to date with the sandbox root: parse files whose size or mtime
    /// changed (skipping those whose content hash did not), drop files that are gone,
    /// ignored, over 1 MB or not UTF-8. `unchanged` counts files left as they were.
    pub async fn refresh(&self) -> Result<RefreshStats, IndexError> {
        let inner = &self.inner;
        let current = scan(inner).await?;
        let known = Arc::new(inner.known_files()?);

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
            inner.apply(&[], &[], chunk)?;
        }

        let (stale, fresh): (Vec<Candidate>, Vec<Candidate>) = current.into_iter().partition(|c| {
            known
                .get(&c.key)
                .is_none_or(|k| (k.mtime, k.size) != (c.mtime, c.size))
        });
        stats.unchanged = fresh.len() as u32;

        let mut stale = stale.into_iter().peekable();
        while stale.peek().is_some() {
            let batch: Vec<Candidate> = stale.by_ref().take(BATCH_FILES).collect();
            let read = read_all(inner, batch).await;
            let hashes: Vec<Option<String>> = read
                .iter()
                .map(|(c, _)| known.get(&c.key).map(|k| k.hash.clone()))
                .collect();
            let outcomes = tokio::task::spawn_blocking(move || {
                read.into_par_iter()
                    .zip(hashes)
                    .map(|((c, bytes), old)| process(c, &bytes, old.as_deref()))
                    .collect::<Vec<_>>()
            })
            .await
            .map_err(|e| IndexError::Task(e.to_string()))?;

            let (parsed, touched, dropped) = split(outcomes);
            stats.parsed += parsed.len() as u32;
            stats.unchanged += touched.len() as u32;
            stats.removed += dropped.iter().filter(|k| known.contains_key(*k)).count() as u32;
            inner.apply(&parsed, &touched, &dropped)?;
        }
        inner.mark_ready();
        Ok(stats)
    }

    /// Re-index one file now (the post-edit hook). A file that no longer exists or no
    /// longer qualifies is dropped from the index; an unsupported extension is ignored.
    pub async fn reindex_file(&self, rel: &Path) -> Result<(), IndexError> {
        let inner = &self.inner;
        let Some(lang) = Lang::from_path(rel) else {
            return Ok(());
        };
        let key = inner.key(rel);
        let meta = match inner.sb.metadata(Path::new(&key)).await {
            Ok(m) => m,
            Err(SandboxError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                return inner.apply(&[], &[], &[key]);
            }
            Err(e) => return Err(e.into()),
        };
        if meta.len > MAX_FILE_BYTES {
            return inner.apply(&[], &[], &[key]);
        }
        let bytes = inner.sb.read(Path::new(&key)).await?;
        let old = inner.known_file(&key)?.map(|k| k.hash);
        let cand = candidate(key, lang, &meta);
        let outcome = tokio::task::spawn_blocking(move || process(cand, &bytes, old.as_deref()))
            .await
            .map_err(|e| IndexError::Task(e.to_string()))?;
        let (parsed, touched, dropped) = split(vec![outcome]);
        inner.apply(&parsed, &touched, &dropped)
    }
}

fn candidate(key: String, lang: Lang, meta: &FileMeta) -> Candidate {
    let mtime = meta
        .mtime
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as i64);
    Candidate {
        key,
        lang,
        mtime,
        size: meta.len as i64,
    }
}

/// Walk the root (honoring ignore files, even outside a git checkout) and stat every
/// file of a known language that is within the size cap.
async fn scan(inner: &Inner) -> Result<Vec<Candidate>, IndexError> {
    let mut builder = inner.sb.walker(Path::new("."))?;
    builder.require_git(false);
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
            if let Some(lang) = Lang::from_path(rel) {
                out.push((rel.to_path_buf(), lang));
            }
        }
        out
    })
    .await
    .map_err(|e| IndexError::Task(e.to_string()))?;

    let sb = &inner.sb;
    let stats: Vec<Option<Candidate>> = stream::iter(paths)
        .map(|(rel, lang)| async move {
            let meta = sb.metadata(&rel).await.ok()?;
            if meta.len > MAX_FILE_BYTES {
                return None;
            }
            Some(candidate(inner.key(&rel), lang, &meta))
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

type Split = (Vec<ParsedFile>, Vec<(String, i64, i64)>, Vec<String>);

fn split(outcomes: Vec<Outcome>) -> Split {
    let mut out: Split = (Vec::new(), Vec::new(), Vec::new());
    for o in outcomes {
        match o {
            Outcome::Parsed(p) => out.0.push(*p),
            Outcome::Touch(k, m, s) => out.1.push((k, m, s)),
            Outcome::Skip(k) => out.2.push(k),
        }
    }
    out
}

/// Hash, decode and (when the content changed) parse one file.
fn process(c: Candidate, bytes: &[u8], old_hash: Option<&str>) -> Outcome {
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Outcome::Skip(c.key);
    }
    let hash = blake3::hash(bytes).to_hex().to_string();
    if old_hash == Some(hash.as_str()) {
        return Outcome::Touch(c.key, c.mtime, c.size);
    }
    let Ok(src) = std::str::from_utf8(bytes) else {
        return Outcome::Skip(c.key);
    };
    let extracted = if c.lang.has_symbols() {
        extract(c.lang, src)
    } else {
        Default::default()
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
        symbols,
        refs: extracted.refs,
        lines,
    }))
}
