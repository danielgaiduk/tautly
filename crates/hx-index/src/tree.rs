//! The in-memory workspace tree: every admitted file with its size, mtime and hash,
//! loaded from the index database at open and kept current by every write to it.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError, RwLock, RwLockReadGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use globset::GlobBuilder;

use crate::store::{Index, Inner};

/// One file of the workspace tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    /// Root-relative path.
    pub path: PathBuf,
    pub size: u64,
    pub mtime: SystemTime,
    /// blake3 hex of the content as last indexed; empty for files over the size cap,
    /// which are never read.
    pub hash: String,
}

/// What the tree remembers per file; the hash is held as raw bytes to keep a file at
/// roughly a hundred bytes.
struct Slot {
    size: u64,
    mtime: i64,
    hash: Option<[u8; 32]>,
}

impl Slot {
    fn entry(&self, key: &str) -> FileEntry {
        FileEntry {
            path: PathBuf::from(key),
            size: self.size,
            mtime: UNIX_EPOCH + Duration::from_nanos(self.mtime.max(0) as u64),
            hash: self
                .hash
                .map(|h| blake3::Hash::from_bytes(h).to_hex().to_string())
                .unwrap_or_default(),
        }
    }
}

/// Files keyed by their `/`-separated root-relative path, so iteration is path order.
#[derive(Default)]
pub(crate) struct TreeData {
    files: BTreeMap<String, Slot>,
    /// Files the trigram index cannot vouch for (over the size cap, not UTF-8).
    opaque: BTreeSet<String>,
}

impl TreeData {
    pub fn insert(&mut self, key: String, size: i64, mtime: i64, hash: &str, text: bool) {
        let hash = blake3::Hash::from_hex(hash).ok().map(|h| *h.as_bytes());
        if text {
            self.opaque.remove(&key);
        } else {
            self.opaque.insert(key.clone());
        }
        self.files.insert(
            key,
            Slot {
                size: size.max(0) as u64,
                mtime,
                hash,
            },
        );
    }

    /// Content unchanged; only the stat data moved.
    pub fn touch(&mut self, key: &str, size: i64, mtime: i64) {
        if let Some(slot) = self.files.get_mut(key) {
            slot.size = size.max(0) as u64;
            slot.mtime = mtime;
        }
    }

    pub fn remove(&mut self, key: &str) {
        self.files.remove(key);
        self.opaque.remove(key);
    }

    pub fn opaque(&self) -> impl Iterator<Item = &String> {
        self.opaque.iter()
    }

    /// Every file at or under the directory `dir` (empty for the root).
    pub fn keys_under(&self, dir: &str) -> Vec<String> {
        let prefix = dir_prefix(dir);
        self.range_from(&prefix)
            .take_while(|(k, _)| k.starts_with(prefix.as_str()))
            .map(|(k, _)| k.clone())
            .collect()
    }

    fn range_from<'a>(&'a self, from: &str) -> impl Iterator<Item = (&'a String, &'a Slot)> {
        self.files
            .range::<str, _>((Bound::Included(from), Bound::Unbounded))
    }
}

fn dir_prefix(dir: &str) -> String {
    if dir.is_empty() {
        String::new()
    } else {
        format!("{dir}/")
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
        let prefix = dir_prefix(&self.inner.key(base));
        let data = self.data();
        data.range_from(&prefix)
            .take_while(|(k, _)| k.starts_with(prefix.as_str()))
            .filter(|(k, _)| matcher.is_match(Path::new(&k[prefix.len()..])))
            .map(|(k, _)| PathBuf::from(k))
            .collect()
    }

    /// The files directly inside `dir`, sorted by path (subdirectories are not listed).
    pub fn list_dir(&self, dir: &Path) -> Vec<FileEntry> {
        let prefix = dir_prefix(&self.inner.key(dir));
        let data = self.data();
        let mut out = Vec::new();
        let mut from = Bound::Included(prefix.clone());
        loop {
            let next = data
                .files
                .range::<String, _>((from.clone(), Bound::Unbounded))
                .next();
            let Some((key, slot)) = next else { break };
            let Some(rest) = key.strip_prefix(prefix.as_str()) else {
                break;
            };
            match rest.find('/') {
                None => {
                    out.push(slot.entry(key));
                    from = Bound::Excluded(key.clone());
                }
                // Skip the whole subdirectory: its keys are contiguous, and `0` is
                // the character after `/`.
                Some(i) => from = Bound::Included(format!("{prefix}{}0", &rest[..i])),
            }
        }
        out
    }

    pub fn get(&self, path: &Path) -> Option<FileEntry> {
        let key = self.inner.key(path);
        self.data().files.get(&key).map(|s| s.entry(&key))
    }

    pub fn len(&self) -> usize {
        self.data().files.len()
    }

    pub fn is_empty(&self) -> bool {
        self.data().files.is_empty()
    }
}

/// A pattern without a `/` matches at any depth, like gitignore.
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
