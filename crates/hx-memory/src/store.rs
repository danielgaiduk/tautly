//! The memory store: loading, applying changes, the index block and search.

use std::fs;
use std::io::{Read, Write};
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
const MAX_ENTRY_BYTES: u64 = 64 * 1024;

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
    /// Loaded on the first search; dropped when an episode is written.
    episodes: Mutex<Option<Arc<Vec<Episode>>>>,
}

fn io_err(path: &Path) -> impl FnOnce(std::io::Error) -> MemoryError + '_ {
    move |source| MemoryError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// Create `file` in `dir` atomically (temp file + rename), mode 0600, creating `dir` with
/// mode 0700 when missing.
pub(crate) fn write_private(dir: &Path, file: &str, content: &str) -> Result<PathBuf, MemoryError> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(io_err(dir))?;
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
        let dir = project_root.join(".hx").join("memory");
        let episodes_dir = data_home
            .join("memory")
            .join(hx_session::paths::project_hash(project_root))
            .join("episodes");
        let (entries, skipped) = load_entries(&dir)?;
        let mut store = MemoryStore {
            index,
            dir,
            episodes_dir,
            entries,
            docs: Vec::new(),
            skipped,
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

    fn rebuild(&mut self) {
        self.entries.sort_by(|a, b| sort_key(a).cmp(&sort_key(b)));
        self.docs = self
            .entries
            .iter()
            .map(|e| search::doc_of(&e.title, &e.body))
            .collect();
    }

    /// Re-check every anchor against the index; returns how many entries are now stale.
    pub fn refresh_staleness(&mut self) -> usize {
        let index = &self.index;
        for e in &mut self.entries {
            e.stale = e.anchors.iter().any(|a| stale::is_stale(index, a));
        }
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
        let mut anchors = Vec::new();
        for (path, symbol) in &d.anchors {
            let shown = match symbol {
                Some(s) => format!("{} (symbol `{s}`)", path.display()),
                None => path.display().to_string(),
            };
            let bad = |reason: String| MemoryError::BadAnchor {
                anchor: shown.clone(),
                reason,
            };
            let rel = normalize_rel(path).map_err(|why| bad(why.into()))?;
            let symbol = match symbol.as_deref().map(str::trim) {
                Some("") => return Err(bad("the symbol name is empty".into())),
                other => other.map(String::from),
            };
            let hash = stale::anchor_hash(&self.index, &rel, symbol.as_deref()).map_err(bad)?;
            anchors.push(MemAnchor {
                path: rel,
                symbol,
                hash,
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
            write_private(&self.dir, &format!("{}.md", e.id), &e.to_markdown())?;
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
        slot.get_or_insert_with(|| Arc::new(episodes::load_all(&self.episodes_dir)))
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
            let (source, title, text, stale) = match self.entries.get(i) {
                Some(e) => (
                    HitSource::Project(e.id.clone()),
                    e.title.clone(),
                    e.body.as_str(),
                    e.stale,
                ),
                None => {
                    let ep = &episodes[i - self.entries.len()];
                    (
                        HitSource::Episode(ep.session_id.clone()),
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
        let path = write_private(&self.episodes_dir, &format!("{session_id}.md"), markdown)?;
        *self.episodes.lock().unwrap_or_else(PoisonError::into_inner) = None;
        Ok(path)
    }
}

/// The entries in `dir` and the files that were skipped. A missing directory is empty.
#[allow(clippy::type_complexity)]
fn load_entries(dir: &Path) -> Result<(Vec<MemoryEntry>, Vec<(PathBuf, String)>), MemoryError> {
    let read = match fs::read_dir(dir) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((vec![], vec![])),
        Err(e) => return Err(io_err(dir)(e)),
    };
    let mut paths: Vec<PathBuf> = read
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "md"))
        .collect();
    paths.sort();
    let (mut entries, mut skipped) = (Vec::new(), Vec::new());
    for path in paths {
        match load_entry(&path) {
            Ok(e) => entries.push(e),
            Err(why) => skipped.push((path, why)),
        }
    }
    Ok((entries, skipped))
}

fn load_entry(path: &Path) -> Result<MemoryEntry, String> {
    let meta = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if !meta.is_file() {
        return Err("not a regular file".into());
    }
    if meta.len() > MAX_ENTRY_BYTES {
        return Err(format!("larger than {MAX_ENTRY_BYTES} bytes"));
    }
    let mut text = String::new();
    fs::File::open(path)
        .and_then(|mut f| f.read_to_string(&mut text))
        .map_err(|e| e.to_string())?;
    let entry = MemoryEntry::from_markdown(&text)?;
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    if !valid_id(stem) || stem != entry.id {
        return Err(format!(
            "the id `{}` does not match the file name",
            entry.id
        ));
    }
    Ok(entry)
}
