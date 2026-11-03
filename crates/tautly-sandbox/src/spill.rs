//! The spill store (`.tautly/tmp/spill`): a content-addressed cache of tool outputs and read
//! views, and its cleanup.

use std::path::Path;
use std::time::{Duration, SystemTime};

/// Root-relative directory of the spill store.
pub const SPILL_DIR: &str = ".tautly/tmp/spill";

/// What [`cleanup_spills`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SpillCleanup {
    pub removed: usize,
    pub freed_bytes: u64,
    pub kept: usize,
    pub kept_bytes: u64,
}

/// Delete spill files under `root` older than `max_age`, then the oldest ones until the
/// rest fit in `max_bytes`. Safe at any time: a re-read whose baseline spill is gone shows
/// full content. Only regular files directly in the store are touched, and nothing at all
/// when any directory on the way is a symlink (a repository could plant one). A missing
/// store is not an error.
pub fn cleanup_spills(
    root: &Path,
    max_age: Duration,
    max_bytes: u64,
) -> std::io::Result<SpillCleanup> {
    let mut dir = root.to_path_buf();
    for part in Path::new(SPILL_DIR).components() {
        dir.push(part);
        match std::fs::symlink_metadata(&dir) {
            Ok(m) if m.is_dir() => {}
            Ok(_) => return Ok(SpillCleanup::default()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(SpillCleanup::default());
            }
            Err(e) => return Err(e),
        }
    }
    let now = SystemTime::now();
    let mut files: Vec<(SystemTime, u64, std::path::PathBuf)> = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let meta = entry.metadata()?;
        if meta.is_file() {
            files.push((meta.modified()?, meta.len(), entry.path()));
        }
    }
    // Oldest first.
    files.sort();
    let mut kept_bytes: u64 = files.iter().map(|(_, len, _)| len).sum();
    let mut out = SpillCleanup::default();
    for (mtime, len, path) in files {
        let old = now.duration_since(mtime).is_ok_and(|age| age > max_age);
        if old || kept_bytes > max_bytes {
            match std::fs::remove_file(&path) {
                Ok(()) => {
                    out.removed += 1;
                    out.freed_bytes += len;
                    kept_bytes -= len;
                    continue;
                }
                // Another process cleaned it up first.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    kept_bytes -= len;
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
        out.kept += 1;
    }
    out.kept_bytes = kept_bytes;
    Ok(out)
}
