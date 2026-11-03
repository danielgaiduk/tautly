//! The memory store: loading, applying changes, the index block and search.

use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hx_index::Index;

use crate::entry::{check_id, normalize_rel, sanitize_title, valid_id, valid_session_id};
use crate::episodes::{self, Episode};
use crate::search::{self, Doc, Hit, HitSource};
use crate::stale;
use crate::{MAX_BODY_TOKENS, MemAnchor, MemoryChange, MemoryDraft, MemoryEntry, MemoryError};

/// Entry files larger than this are not loaded.
const MAX_ENTRY_BYTES: u64 = 16 * 1024;
/// At most this many entries are loaded.
const MAX_ENTRIES: usize = 500;
/// The entry files loaded add up to at most this many bytes.
const MAX_TOTAL_BYTES: u64 = 2 * 1024 * 1024;
/// Directory listings are read only up to this many `.md` files.
const MAX_SCAN: usize = MAX_ENTRIES * 4;
/// Most anchors one entry may have.
pub(crate) const MAX_ANCHORS: usize = 16;
/// `skipped()` lists this many files at most; the rest are only counted.
const MAX_SKIPS_LISTED: usize = 50;

pub struct MemoryStore {
    pub(crate) index: Arc<Index>,
    /// `<project>/.hx/memory`.
    pub(crate) dir: PathBuf,
    /// `<data_home>/memory/<project_hash>/episodes`.
    pub(crate) episodes_dir: PathBuf,
    /// Oldest first (ties by id).
    pub(crate) entries: Vec<MemoryEntry>,
    /// Search statistics of `entries`, in the same order.
    pub(crate) docs: Vec<Doc>,
    skipped: Vec<(PathBuf, String)>,
    skipped_total: usize,
    /// `.hx` and `.hx/memory`: checked not to be links before every read and write.
    dir_chain: Vec<PathBuf>,
    /// The hx data home, and below it `memory`, the project's directory and `episodes`.
    data_home: PathBuf,
    episodes_chain: Vec<PathBuf>,
    staleness_error: Option<String>,
    /// Loaded on the first search; dropped when an episode is written.
    episodes: Mutex<Option<Arc<Vec<Episode>>>>,
}

fn io_err(path: &Path) -> impl FnOnce(std::io::Error) -> MemoryError + '_ {
    move |source| MemoryError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// Whether every directory of `chain` (each inside the previous) exists as a real
/// directory: Ok(false) once one is missing, `UnsafeDir` for a symlink or a file. Memory is
/// repo-controlled, so a link must never lead reads or writes out of the project.
pub(crate) fn verify_dirs(chain: &[PathBuf]) -> Result<bool, MemoryError> {
    for dir in chain {
        match fs::symlink_metadata(dir) {
            Ok(m) if m.is_dir() => {}
            Ok(_) => return Err(MemoryError::UnsafeDir(dir.clone())),
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(io_err(dir)(e)),
        }
    }
    Ok(true)
}

/// [`verify_dirs`], creating the missing directories with mode 0700.
fn ensure_dirs(chain: &[PathBuf]) -> Result<(), MemoryError> {
    for dir in chain {
        if let Err(e) = fs::DirBuilder::new().mode(0o700).create(dir)
            && e.kind() != ErrorKind::AlreadyExists
        {
            return Err(io_err(dir)(e));
        }
        // Whether we made it or it was there: it must be a real directory.
        if !fs::symlink_metadata(dir).map_err(io_err(dir))?.is_dir() {
            return Err(MemoryError::UnsafeDir(dir.clone()));
        }
    }
    Ok(())
}

/// Read `path` (UTF-8) without following a symlink or blocking on a pipe. A file over `max`
/// bytes is an error, or cut to `max` when `truncate`.
pub(crate) fn read_nofollow(path: &Path, max: u64, truncate: bool) -> Result<String, String> {
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|e| format!("cannot open (a symlink is refused): {e}"))?;
    let meta = file.metadata().map_err(|e| e.to_string())?;
    if !meta.is_file() {
        return Err("not a regular file".into());
    }
    if meta.len() > max && !truncate {
        return Err(format!("larger than {max} bytes"));
    }
    let mut bytes = Vec::new();
    file.take(max)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    match String::from_utf8(bytes) {
        Ok(t) => Ok(t),
        Err(e) if truncate => {
            let valid = e.utf8_error().valid_up_to();
            Ok(String::from_utf8_lossy(&e.as_bytes()[..valid]).into_owned())
        }
        Err(_) => Err("not UTF-8".into()),
    }
}

/// Create `file` atomically (temp file in the same directory + rename), mode 0600, in the
/// last directory of `chain`, creating the chain with mode 0700. The chain is checked for
/// links first.
pub(crate) fn write_private(
    chain: &[PathBuf],
    file: &str,
    content: &str,
) -> Result<PathBuf, MemoryError> {
    ensure_dirs(chain)?;
    let dir = chain.last().expect("a directory chain is never empty");
    let target = dir.join(file);
    let tmp = dir.join(format!(".{file}.{:08x}.tmp", rand::random::<u32>()));
    let written = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .and_then(|mut f| {
            f.write_all(content.as_bytes())?;
            f.sync_all()
        })
        .and_then(|()| fs::rename(&tmp, &target));
    if let Err(e) = written {
        let _ = fs::remove_file(&tmp);
        return Err(io_err(&target)(e));
    }
    Ok(target)
}

/// Whole seconds since the epoch, so a timestamp survives the file format unchanged.
pub(crate) fn now() -> SystemTime {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    UNIX_EPOCH + Duration::from_secs(secs)
}

fn sort_key(e: &MemoryEntry) -> (SystemTime, &str) {
    (e.created, e.id.as_str())
}

impl MemoryStore {
    /// Open the store of `project_root`, with episodes under the hx data home.
    pub fn open(project_root: &Path, index: Arc<Index>) -> Result<Self, MemoryError> {
        Self::open_in(project_root, &hx_session::paths::data_home(), index)
    }

    /// [`open`](Self::open) with an explicit data home (tests). Entry files that cannot be
    /// read are left out and listed by [`skipped`](Self::skipped); staleness is checked.
    pub fn open_in(
        project_root: &Path,
        data_home: &Path,
        index: Arc<Index>,
    ) -> Result<Self, MemoryError> {
        let hx = project_root.join(".hx");
        let dir = hx.join("memory");
        let dir_chain = vec![hx, dir.clone()];
        let episodes_dir = data_home
            .join("memory")
            .join(hx_session::paths::project_hash(project_root))
            .join("episodes");
        let episodes_chain: Vec<PathBuf> = episodes_dir
            .ancestors()
            .take(3)
            .map(Path::to_path_buf)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        let (entries, skipped, skipped_total) = if verify_dirs(&dir_chain)? {
            load_entries(&dir)?
        } else {
            (vec![], vec![], 0)
        };
        let mut store = MemoryStore {
            index,
            dir,
            episodes_dir,
            entries,
            docs: Vec::new(),
            skipped,
            skipped_total,
            dir_chain,
            data_home: data_home.to_path_buf(),
            episodes_chain,
            staleness_error: None,
            episodes: Mutex::new(None),
        };
        store.rebuild();
        store.refresh_staleness();
        Ok(store)
    }

    pub fn entries(&self) -> &[MemoryEntry] {
        &self.entries
    }

    /// Entry files that could not be loaded, with the reason.
    pub fn skipped(&self) -> &[(PathBuf, String)] {
        &self.skipped
    }

    /// How many entry files were skipped when loading (`skipped` lists only the first few).
    pub fn skipped_count(&self) -> usize {
        self.skipped_total
    }

    /// The error from the last staleness check, if the index could not be queried.
    pub fn staleness_error(&self) -> Option<&str> {
        self.staleness_error.as_deref()
    }

    fn rebuild(&mut self) {
        self.entries.sort_by(|a, b| sort_key(a).cmp(&sort_key(b)));
        self.docs = self
            .entries
            .iter()
            .map(|e| search::doc_of(&e.title, &e.body))
            .collect();
    }

    /// Re-check every anchor against the index; returns how many entries are now stale. An
    /// entry whose anchors cannot be checked because the index fails keeps its flag; the
    /// error is available from [`staleness_error`](Self::staleness_error) until the next
    /// check.
    pub fn refresh_staleness(&mut self) -> usize {
        let index = &self.index;
        let mut error = None;
        for e in &mut self.entries {
            let mut stale = false;
            let mut unknown = false;
            for a in &e.anchors {
                match stale::is_stale(index, a) {
                    Ok(true) => {
                        stale = true;
                        break;
                    }
                    Ok(false) => {}
                    Err(err) => {
                        error.get_or_insert_with(|| err.to_string());
                        unknown = true;
                        break;
                    }
                }
            }
            if !unknown {
                e.stale = stale;
            }
        }
        self.staleness_error = error;
        self.entries.iter().filter(|e| e.stale).count()
    }

    /// One line per entry (`- [kind] title`, plus ` [stale]`), oldest first, within
    /// `budget_tokens`. When the full list is too long the oldest unverified entries are left
    /// out first (they stay on disk); the bool says some were. Titles are cleaned: the text
    /// ends up in the prompt and files can be edited by anyone with the repository.
    pub fn index_block(&self, budget_tokens: u32, est: &dyn Fn(&str) -> u32) -> (String, bool) {
        let lines: Vec<String> = self
            .entries
            .iter()
            .map(|e| {
                let title = sanitize_title(&e.title);
                let title = if title.is_empty() {
                    "(untitled)"
                } else {
                    &title
                };
                let stale = if e.stale { " [stale]" } else { "" };
                format!("- [{}] {title}{stale}", e.kind.as_str())
            })
            .collect();
        let render = |keep: &[bool]| {
            let shown: Vec<&str> = lines
                .iter()
                .zip(keep)
                .filter(|(_, k)| **k)
                .map(|(l, _)| l.as_str())
                .collect();
            shown.join("\n")
        };
        let mut keep = vec![true; lines.len()];
        let mut block = render(&keep);
        if est(&block) <= budget_tokens {
            return (block, false);
        }
        let mut drop_order: Vec<usize> = (0..lines.len()).collect();
        drop_order.sort_by_key(|&i| {
            (
                self.entries[i].verified.is_some(),
                sort_key(&self.entries[i]),
            )
        });
        for i in drop_order {
            keep[i] = false;
            block = render(&keep);
            if est(&block) <= budget_tokens {
                break;
            }
        }
        (block, true)
    }

    /// An id for a new entry: slug of `title` and four hex digits, unused by any entry or
    /// file and not in `taken`.
    pub(crate) fn new_id(&self, title: &str, taken: &[String]) -> String {
        let mut slug = String::new();
        for c in title.chars() {
            if c.is_ascii_alphanumeric() {
                slug.push(c.to_ascii_lowercase());
            } else if !slug.ends_with('-') {
                slug.push('-');
            }
        }
        let slug = slug.trim_matches('-');
        let slug: String = slug.chars().take(40).collect();
        let slug = slug.trim_end_matches('-');
        let slug = if slug.is_empty() { "entry" } else { slug };
        loop {
            let id = format!("{slug}-{:04x}", rand::random::<u16>());
            let used = self.entries.iter().any(|e| e.id == id)
                || taken.contains(&id)
                || self.dir.join(format!("{id}.md")).exists();
            if !used {
                return id;
            }
        }
    }

    /// Validate `d` and fill in the anchor hashes. Writes nothing. A draft naming an existing
    /// `id` updates that entry: it keeps its creation time and counts as verified now.
    pub fn draft_to_change(
        &self,
        d: MemoryDraft,
        est: &dyn Fn(&str) -> u32,
    ) -> Result<MemoryChange, MemoryError> {
        let title = sanitize_title(&d.title);
        if title.is_empty() {
            return Err(MemoryError::EmptyTitle);
        }
        let body = d.body.trim();
        if body.is_empty() {
            return Err(MemoryError::EmptyBody);
        }
        let tokens = est(body);
        if tokens > MAX_BODY_TOKENS {
            return Err(MemoryError::TooLong {
                tokens,
                max: MAX_BODY_TOKENS,
            });
        }
        let existing = match &d.id {
            Some(id) => {
                check_id(id)?;
                Some(
                    self.entries
                        .iter()
                        .find(|e| &e.id == id)
                        .ok_or_else(|| MemoryError::UnknownId(id.clone()))?,
                )
            }
            None => None,
        };
        // An update without anchors keeps the entry's own, hashed again against the code now.
        let wanted: Vec<(PathBuf, Option<String>)> = match existing {
            Some(e) if d.anchors.is_empty() => e
                .anchors
                .iter()
                .map(|a| (a.path.clone(), a.symbol.clone()))
                .collect(),
            _ => d.anchors.clone(),
        };
        if wanted.len() > MAX_ANCHORS {
            return Err(MemoryError::InvalidChange(format!(
                "an entry has at most {MAX_ANCHORS} anchors"
            )));
        }
        let mut anchors = Vec::new();
        for (path, symbol) in &wanted {
            let shown = match symbol {
                Some(s) => format!("{} (symbol `{s}`)", path.display()),
                None => path.display().to_string(),
            };
            let bad = |reason: String| MemoryError::BadAnchor {
                anchor: shown.clone(),
                reason,
            };
            let index_err = |e: hx_index::IndexError| MemoryError::Index(e.to_string());
            let rel = normalize_rel(path).map_err(|why| bad(why.into()))?;
            let symbol = match symbol.as_deref().map(str::trim) {
                Some("") => return Err(bad("the symbol name is empty".into())),
                other => other.map(String::from),
            };
            let hash =
                match stale::probe(&self.index, &rel, symbol.as_deref()).map_err(index_err)? {
                    stale::Probe::Hash(h) => h,
                    stale::Probe::Gone(reason) => return Err(bad(reason)),
                };
            if let Some(sym) = &symbol
                && let Some(why) = stale::ambiguity(&self.index, &rel, sym).map_err(index_err)?
            {
                return Err(bad(why));
            }
            anchors.push(MemAnchor {
                path: rel,
                symbol,
                hash,
            });
        }
        let when = now();
        let entry = MemoryEntry {
            id: match existing {
                Some(e) => e.id.clone(),
                None => self.new_id(&title, &[]),
            },
            title,
            kind: d.kind,
            anchors,
            created: existing.map_or(when, |e| e.created),
            verified: existing.map(|_| when),
            body: body.to_string(),
            stale: false,
        };
        Ok(MemoryChange {
            upsert: Some(entry),
            delete: Vec::new(),
        })
    }

    /// Write the change to disk and update the in-memory entries. The upserted file is
    /// written first, so a failure there changes nothing; a failed delete is reported after
    /// the others were tried.
    pub fn apply(&mut self, c: &MemoryChange) -> Result<(), MemoryError> {
        verify_dirs(&self.dir_chain)?;
        if let Some(e) = &c.upsert {
            check_id(&e.id)?;
            if let Some(a) = e.anchors.iter().find(|a| normalize_rel(&a.path).is_err()) {
                return Err(MemoryError::InvalidChange(format!(
                    "anchor path {} is not a relative path inside the project",
                    a.path.display()
                )));
            }
            if c.delete.contains(&e.id) {
                return Err(MemoryError::InvalidChange(format!(
                    "`{}` is both written and deleted",
                    e.id
                )));
            }
        }
        for id in &c.delete {
            check_id(id)?;
        }
        if let Some(e) = &c.upsert {
            write_private(&self.dir_chain, &format!("{}.md", e.id), &e.to_markdown())?;
            self.entries.retain(|x| x.id != e.id);
            self.entries.push(e.clone());
        }
        let mut failure = None;
        for id in &c.delete {
            let path = self.dir.join(format!("{id}.md"));
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    failure.get_or_insert(io_err(&path)(e));
                    continue;
                }
            }
            self.entries.retain(|x| &x.id != id);
        }
        self.rebuild();
        failure.map_or(Ok(()), Err)
    }

    fn episodes(&self) -> Arc<Vec<Episode>> {
        let mut slot = self.episodes.lock().unwrap_or_else(PoisonError::into_inner);
        slot.get_or_insert_with(|| {
            // A linked episodes directory is treated as having no episodes.
            let found = match verify_dirs(&self.episodes_chain) {
                Ok(true) => episodes::load_all(&self.episodes_dir),
                _ => Vec::new(),
            };
            Arc::new(found)
        })
        .clone()
    }

    /// BM25 (k1 = 1.2, b = 0.75) over project entries and episodes: the best `k` that match,
    /// with their texts cut so that titles and texts together fit `cap_tokens`.
    pub fn search(
        &self,
        query: &str,
        k: usize,
        cap_tokens: u32,
        est: &dyn Fn(&str) -> u32,
    ) -> Vec<Hit> {
        let terms = search::query_terms(query);
        if terms.is_empty() || k == 0 {
            return Vec::new();
        }
        let episodes = self.episodes();
        let docs: Vec<&Doc> = self
            .docs
            .iter()
            .chain(episodes.iter().map(|e| &e.doc))
            .collect();
        let mut hits = Vec::new();
        let mut left = cap_tokens;
        for (i, score) in search::rank(&docs, &terms).into_iter().take(k) {
            let (source, kind, title, text, stale) = match self.entries.get(i) {
                Some(e) => (
                    HitSource::Project(e.id.clone()),
                    Some(e.kind),
                    sanitize_title(&e.title),
                    e.body.as_str(),
                    e.stale,
                ),
                None => {
                    let ep = &episodes[i - self.entries.len()];
                    (
                        HitSource::Episode(ep.session_id.clone()),
                        None,
                        ep.title.clone(),
                        ep.text.as_str(),
                        false,
                    )
                }
            };
            let Some(room) = left.checked_sub(est(&title)) else {
                break;
            };
            let Some(text) = search::truncate_to(text, room, est) else {
                break;
            };
            left = room.saturating_sub(est(&text));
            hits.push(Hit {
                source,
                kind,
                title,
                text,
                stale,
                score,
            });
            if left == 0 {
                break;
            }
        }
        hits
    }

    /// Write the episode of `session_id` (replacing an earlier one); returns its path.
    pub fn write_episode(&self, session_id: &str, markdown: &str) -> Result<PathBuf, MemoryError> {
        if !valid_session_id(session_id) {
            return Err(MemoryError::BadId(session_id.to_string()));
        }
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.data_home)
            .map_err(io_err(&self.data_home))?;
        let path = write_private(&self.episodes_chain, &format!("{session_id}.md"), markdown)?;
        *self.episodes.lock().unwrap_or_else(PoisonError::into_inner) = None;
        Ok(path)
    }
}

/// What loading a directory found: the entries, the first skipped files with the reason,
/// and how many were skipped in all.
type Loaded = (Vec<MemoryEntry>, Vec<(PathBuf, String)>, usize);

/// The entries in `dir` (checked not to be a link), within the load caps. A missing
/// directory is empty.
fn load_entries(dir: &Path) -> Result<Loaded, MemoryError> {
    let read = match fs::read_dir(dir) {
        Ok(r) => r,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok((vec![], vec![], 0)),
        Err(e) => return Err(io_err(dir)(e)),
    };
    let mut paths: Vec<PathBuf> = read
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "md"))
        .take(MAX_SCAN + 1)
        .collect();
    let (mut entries, mut skipped, mut total) = (Vec::new(), Vec::new(), 0usize);
    let mut skip = |path: &Path, why: String| {
        total += 1;
        if skipped.len() < MAX_SKIPS_LISTED {
            skipped.push((path.to_path_buf(), why));
        }
    };
    if paths.len() > MAX_SCAN {
        paths.pop();
        skip(
            dir,
            format!("more than {MAX_SCAN} entry files; the rest were not read"),
        );
    }
    paths.sort();
    let mut bytes = 0u64;
    for path in paths {
        if entries.len() >= MAX_ENTRIES {
            skip(
                &path,
                format!("more than {MAX_ENTRIES} entries; not loaded"),
            );
            continue;
        }
        match load_entry(&path) {
            Ok((_, len)) if bytes + len > MAX_TOTAL_BYTES => skip(
                &path,
                format!("the total size of the entry files would exceed {MAX_TOTAL_BYTES} bytes"),
            ),
            Ok((entry, len)) => {
                bytes += len;
                entries.push(entry);
            }
            Err(why) => skip(&path, why),
        }
    }
    Ok((entries, skipped, total))
}

/// One entry file and its size, enforcing the limits drafts obey: title cleaned and capped,
/// body within the token limit, a bounded number of anchors.
fn load_entry(path: &Path) -> Result<(MemoryEntry, u64), String> {
    let text = read_nofollow(path, MAX_ENTRY_BYTES, false)?;
    let mut entry = MemoryEntry::from_markdown(&text)?;
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    if !valid_id(stem) || stem != entry.id {
        return Err(format!(
            "the id `{}` does not match the file name",
            entry.id
        ));
    }
    let tokens = entry.body.len().div_ceil(4) as u32;
    if tokens > MAX_BODY_TOKENS {
        return Err(format!(
            "the body is about {tokens} tokens; the limit is {MAX_BODY_TOKENS}"
        ));
    }
    if entry.anchors.len() > MAX_ANCHORS {
        return Err(format!("more than {MAX_ANCHORS} anchors"));
    }
    entry.title = sanitize_title(&entry.title);
    if entry.title.is_empty() {
        entry.title = "(untitled)".into();
    }
    Ok((entry, text.len() as u64))
}
