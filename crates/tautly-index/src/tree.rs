//! The in-memory workspace tree: every admitted file with its size, mtime and language,
//! loaded from the index database at open and kept current by every write to it.
//!
//! Kept small (spec 7.10): directory paths are stored once, each file holds only its name
//! and a 24-byte [`Slot`], and content hashes stay in the database ([`FileEntry::hash`]
//! is fetched when an entry is handed out).

use std::collections::BTreeMap;
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError, RwLock, RwLockReadGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use globset::GlobBuilder;

use crate::lang::Lang;
use crate::store::{Index, Inner};

/// One file of the workspace tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    /// Root-relative path.
    pub path: PathBuf,
    /// Size in bytes.
    pub size: u64,
    /// Last modification time.
    pub mtime: SystemTime,
    /// blake3 hex of the content as last indexed; empty for files over the size cap,
    /// which are never read.
    pub hash: String,
    /// The language its extension names, if the index knows it.
    pub lang: Option<Lang>,
}

/// A directory of the workspace tree, with what is at or under it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirInfo {
    /// Root-relative path.
    pub path: PathBuf,
    /// Files at or under it.
    pub files: usize,
    /// Those files per known language, most files first (then by name).
    pub langs: Vec<(Lang, usize)>,
}

/// What the tree remembers per file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Slot {
    /// Nanoseconds since the epoch.
    pub mtime: i64,
    /// Saturates at `u32::MAX` (only compared for change detection).
    pub size: u32,
    /// The write that produced this row (see [`Inner::apply`]); 0 for rows loaded at open.
    pub generation: u32,
    /// [`Lang::code`] of the file's language.
    pub lang: u8,
}

impl Slot {
    pub fn new(key: &str, size: i64, mtime: i64, generation: u32) -> Slot {
        Slot {
            mtime,
            size: clamp_size(size),
            generation,
            lang: Lang::code(Lang::from_path(Path::new(key))),
        }
    }

    /// Whether this row still describes a file of `size` bytes modified at `mtime`.
    pub fn same_stat(&self, size: i64, mtime: i64) -> bool {
        self.mtime == mtime && self.size == clamp_size(size)
    }

    fn entry(&self, key: String, hash: String) -> FileEntry {
        FileEntry {
            path: PathBuf::from(key),
            size: u64::from(self.size),
            mtime: UNIX_EPOCH + Duration::from_nanos(self.mtime.max(0) as u64),
            hash,
            lang: Lang::from_code(self.lang),
        }
    }
}

fn clamp_size(size: i64) -> u32 {
    u32::try_from(size.max(0)).unwrap_or(u32::MAX)
}

/// The directory and file name of a `/`-separated key (the root directory is `""`).
fn split(key: &str) -> (&str, &str) {
    key.rsplit_once('/').unwrap_or(("", key))
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

fn dir_prefix(dir: &str) -> String {
    if dir.is_empty() {
        String::new()
    } else {
        format!("{dir}/")
    }
}

type Files = Vec<(Box<str>, Slot)>;

/// Files grouped by directory: directory key to its files, sorted by name.
#[derive(Default)]
pub(crate) struct TreeData {
    dirs: BTreeMap<Box<str>, Files>,
    len: usize,
}

impl TreeData {
    /// Add or replace the row of `key`.
    pub fn insert(&mut self, key: &str, slot: Slot) {
        let (dir, name) = split(key);
        if !self.dirs.contains_key(dir) {
            self.dirs.insert(dir.into(), Vec::new());
        }
        let Some(files) = self.dirs.get_mut(dir) else {
            return;
        };
        match files.binary_search_by(|(n, _)| (**n).cmp(name)) {
            Ok(i) => files[i].1 = slot,
            Err(i) => {
                files.insert(i, (name.into(), slot));
                self.len += 1;
            }
        }
    }

    pub fn get(&self, key: &str) -> Option<Slot> {
        let (dir, name) = split(key);
        let files = self.dirs.get(dir)?;
        let i = files.binary_search_by(|(n, _)| (**n).cmp(name)).ok()?;
        Some(files[i].1)
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut Slot> {
        let (dir, name) = split(key);
        let files = self.dirs.get_mut(dir)?;
        let i = files.binary_search_by(|(n, _)| (**n).cmp(name)).ok()?;
        Some(&mut files[i].1)
    }

    pub fn remove(&mut self, key: &str) {
        let (dir, name) = split(key);
        let Some(files) = self.dirs.get_mut(dir) else {
            return;
        };
        if let Ok(i) = files.binary_search_by(|(n, _)| (**n).cmp(name)) {
            files.remove(i);
            self.len -= 1;
            if files.is_empty() {
                self.dirs.remove(dir);
            }
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    /// Give back the room left by growing the per-directory lists (after a bulk load).
    pub fn shrink(&mut self) {
        for files in self.dirs.values_mut() {
            files.shrink_to_fit();
        }
    }

    /// The directory `dir` (if it has files) and every directory below it, as stored.
    fn dirs_at_or_under<'a>(&'a self, dir: &str) -> impl Iterator<Item = (&'a str, &'a Files)> {
        let own = (!dir.is_empty())
            .then(|| self.dirs.get_key_value(dir))
            .flatten();
        // Every key starting with `dir/` (all keys for the root) is contiguous.
        let prefix = dir_prefix(dir);
        let below = self
            .dirs
            .range::<str, _>((Bound::Included(prefix.as_str()), Bound::Unbounded))
            .take_while(move |(k, _)| k.starts_with(prefix.as_str()));
        own.into_iter().chain(below).map(|(k, files)| (&**k, files))
    }

    /// Whether `key` is a file, or a directory with files under it.
    pub fn contains_or_under(&self, key: &str) -> bool {
        self.get(key).is_some() || self.dirs_at_or_under(key).next().is_some()
    }

    /// Every file at or under the directory `dir` (empty for the root).
    pub fn keys_under(&self, dir: &str) -> Vec<String> {
        let mut out = Vec::new();
        for (d, files) in self.dirs_at_or_under(dir) {
            out.extend(files.iter().map(|(name, _)| join(d, name)));
        }
        out
    }

    /// Call `f` with the directory, name and slot of every file.
    pub fn for_each(&self, mut f: impl FnMut(&str, &str, &Slot)) {
        for (dir, files) in &self.dirs {
            for (name, slot) in files {
                f(dir, name, slot);
            }
        }
    }
}

/// A live, cheaply cloneable view of the workspace tree: every call reads the current
/// state.
#[derive(Clone)]
pub struct TreeView {
    inner: Arc<Inner>,
}

impl TreeView {
    fn data(&self) -> RwLockReadGuard<'_, TreeData> {
        self.inner
            .tree()
            .read()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Entries for these rows, with their hashes from the database (fetched after the tree
    /// lock is released, so a writer holding the database never waits on a reader).
    fn entries(&self, rows: Vec<(String, Slot)>) -> Vec<FileEntry> {
        let keys: Vec<&str> = rows.iter().map(|(k, _)| k.as_str()).collect();
        let hashes = self.inner.stored_hashes(&keys);
        rows.into_iter()
            .zip(hashes)
            .map(|((key, slot), hash)| slot.entry(key, hash))
            .collect()
    }

    /// Files matching `pattern`, matched against paths relative to `base` (a directory,
    /// root-relative or absolute under the root), as root-relative paths in sorted
    /// order. Same semantics as the `glob` tool: `*` never crosses `/`, and a pattern
    /// without a `/` matches at any depth. An invalid pattern matches nothing.
    pub fn glob(&self, pattern: &str, base: &Path) -> Vec<PathBuf> {
        let Ok(glob) = GlobBuilder::new(&anchor(pattern))
            .literal_separator(true)
            .build()
        else {
            return Vec::new();
        };
        let matcher = glob.compile_matcher();
        let base = self.inner.key(base);
        let skip = dir_prefix(&base).len();
        // Only the directory the pattern's literal leading components name can match
        // (spec 9.1: glob from the tree < 5 ms on 100,000 files).
        let under = match literal_dir(pattern) {
            lit if lit.is_empty() => base.clone(),
            lit => join(&base, &lit),
        };
        let mut out: Vec<String> = Vec::new();
        {
            let data = self.data();
            for (dir, files) in data.dirs_at_or_under(&under) {
                for (name, _) in files {
                    let key = join(dir, name);
                    if matcher.is_match(Path::new(&key[skip..])) {
                        out.push(key);
                    }
                }
            }
        }
        out.sort_unstable();
        out.into_iter().map(PathBuf::from).collect()
    }

    /// The files directly inside `dir`, sorted by path (subdirectories are not listed).
    pub fn list_dir(&self, dir: &Path) -> Vec<FileEntry> {
        let dir = self.inner.key(dir);
        let rows: Vec<(String, Slot)> = {
            let data = self.data();
            data.dirs.get(dir.as_str()).map_or_else(Vec::new, |files| {
                files
                    .iter()
                    .map(|(name, slot)| (join(&dir, name), *slot))
                    .collect()
            })
        };
        self.entries(rows)
    }

    /// The entry for root-relative `path`, if the tree has that file.
    pub fn get(&self, path: &Path) -> Option<FileEntry> {
        let key = self.inner.key(path);
        let slot = self.data().get(&key)?;
        self.entries(vec![(key, slot)]).pop()
    }

    /// Whether `path` is a file of the tree, or a directory with files under it. A path the
    /// tree does not know may still exist on disk (hidden or ignored, or created since).
    pub fn contains(&self, path: &Path) -> bool {
        let key = self.inner.key(path);
        self.data().contains_or_under(&key)
    }

    /// The number of files in the tree.
    pub fn len(&self) -> usize {
        self.data().len()
    }

    /// Whether the tree has no files.
    pub fn is_empty(&self) -> bool {
        self.data().len() == 0
    }

    /// Every directory at most `depth` levels below the root that has files at or under
    /// it, sorted by path (component by component).
    pub fn dirs(&self, depth: usize) -> Vec<DirInfo> {
        let mut acc: BTreeMap<PathBuf, (usize, BTreeMap<u8, usize>)> = BTreeMap::new();
        {
            let data = self.data();
            for (dir, files) in &data.dirs {
                if dir.is_empty() {
                    continue;
                }
                let parts: Vec<&str> = dir.split('/').collect();
                for n in 1..=parts.len().min(depth) {
                    let (count, langs) = acc.entry(parts[..n].iter().collect()).or_default();
                    *count += files.len();
                    for (_, slot) in files {
                        if slot.lang != 0 {
                            *langs.entry(slot.lang).or_default() += 1;
                        }
                    }
                }
            }
        }
        acc.into_iter()
            .map(|(path, (files, langs))| {
                let mut langs: Vec<(Lang, usize)> = langs
                    .into_iter()
                    .filter_map(|(code, n)| Some((Lang::from_code(code)?, n)))
                    .collect();
                langs.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.as_str().cmp(b.0.as_str())));
                DirInfo { path, files, langs }
            })
            .collect()
    }
}

/// A pattern without a `/` matches at any depth, like gitignore.
/// The leading directory components of `pattern` that hold no glob syntax (`"src/a"` for
/// `"src/a/**/*.rs"`); empty when there are none or the pattern matches at any depth.
fn literal_dir(pattern: &str) -> String {
    let Some((dirs, _file)) = pattern.rsplit_once('/') else {
        return String::new();
    };
    dirs.split('/')
        .take_while(|c| {
            !c.is_empty() && *c != "." && *c != ".." && !c.contains(['*', '?', '[', '{', '\\'])
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn anchor(pattern: &str) -> String {
    if pattern.contains('/') {
        pattern.to_string()
    } else {
        format!("**/{pattern}")
    }
}

impl Index {
    /// A live view of the workspace tree, answered from memory.
    pub fn tree(&self) -> TreeView {
        TreeView {
            inner: Arc::clone(&self.inner),
        }
    }
}

pub(crate) type SharedTree = RwLock<TreeData>;

#[cfg(test)]
mod tests {
    use super::*;

    fn slot(generation: u32) -> Slot {
        Slot::new("x.rs", 1, 2, generation)
    }

    #[test]
    fn slots_stay_small() {
        assert!(std::mem::size_of::<Slot>() <= 24);
        assert!(std::mem::size_of::<(Box<str>, Slot)>() <= 40);
    }

    #[test]
    fn insert_get_remove_keep_counts_and_drop_empty_directories() {
        let mut t = TreeData::default();
        t.insert("a/b.rs", slot(1));
        t.insert("a/a.rs", slot(1));
        t.insert("a/b.rs", slot(2));
        t.insert("top.rs", slot(1));
        assert_eq!(t.len(), 3);
        assert_eq!(t.get("a/b.rs").unwrap().generation, 2);
        assert_eq!(t.keys_under("a"), ["a/a.rs", "a/b.rs"]);
        t.remove("a/a.rs");
        t.remove("a/b.rs");
        t.remove("a/missing.rs");
        assert_eq!(t.len(), 1);
        assert!(!t.contains_or_under("a"));
        assert!(t.contains_or_under(""));
        assert_eq!(t.keys_under(""), ["top.rs"]);
    }

    #[test]
    fn literal_dir_stops_at_glob_syntax() {
        assert_eq!(literal_dir("src/a/**/*.rs"), "src/a");
        assert_eq!(literal_dir("src/main.rs"), "src");
        assert_eq!(literal_dir("*.rs"), "");
        assert_eq!(literal_dir("**/x/*.rs"), "");
        assert_eq!(literal_dir("src/{a,b}/*.rs"), "src");
        assert_eq!(literal_dir("./src/*.rs"), "");
        assert_eq!(literal_dir("/abs/*.rs"), "");
    }
}
