//! Where a checkpointer keeps the exclude rules each checkpoint was taken with (the
//! user's excludes file and `info/exclude`, which undo reads back): in the session data
//! home, owner-only, never in the repository (whose refs a push can publish).
//!
//! `<dir>/<commit>` names the rules by their blake3 digest; `<dir>/rules/<digest>` holds
//! them, once however many checkpoints share them. Directories are 0700, files 0600, and
//! every file is written to a temporary name first and renamed into place.

use std::fs;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

pub(crate) struct ExcludesStore {
    dir: PathBuf,
}

impl ExcludesStore {
    pub(crate) fn new(dir: PathBuf) -> ExcludesStore {
        ExcludesStore { dir }
    }

    /// Keep `rules` as checkpoint `commit`'s.
    pub(crate) fn put(&self, commit: &str, rules: &[u8]) -> std::io::Result<()> {
        if !is_object_id(commit) {
            return Err(std::io::Error::other(format!(
                "not an object id: {commit:?}"
            )));
        }
        let digest = blake3::hash(rules).to_hex().to_string();
        let kept = self.dir.join("rules");
        private_dir(&kept)?;
        let file = kept.join(&digest);
        if !file.exists() {
            write_atomically(&kept, &file, rules)?;
        }
        write_atomically(&self.dir, &self.dir.join(commit), digest.as_bytes())
    }

    /// The rules kept for checkpoint `commit` (none: none kept, or damaged).
    pub(crate) fn get(&self, commit: &str) -> Option<Vec<u8>> {
        if !is_object_id(commit) {
            return None;
        }
        let digest = fs::read_to_string(self.dir.join(commit)).ok()?;
        if digest.len() != 64 || !digest.bytes().all(is_lower_hex) {
            return None;
        }
        let rules = fs::read(self.dir.join("rules").join(&digest)).ok()?;
        (blake3::hash(&rules).to_hex().as_str() == digest).then_some(rules)
    }
}

fn is_lower_hex(b: u8) -> bool {
    matches!(b, b'0'..=b'9' | b'a'..=b'f')
}

/// A SHA-1 or SHA-256 object id, as git prints it.
fn is_object_id(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.bytes().all(is_lower_hex)
}

fn private_dir(dir: &Path) -> std::io::Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

/// Write `bytes` to `dest` (in `dir`) through a temporary file, mode 0600.
fn write_atomically(dir: &Path, dest: &Path, bytes: &[u8]) -> std::io::Result<()> {
    private_dir(dir)?;
    let tmp = dir.join(format!(
        ".tmp-{}-{}",
        std::process::id(),
        TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(bytes)?;
        fs::rename(&tmp, dest)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn files(dir: &std::path::Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(files(&p));
            } else {
                out.push(p);
            }
        }
        out
    }

    #[test]
    fn rules_round_trip_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let store = ExcludesStore::new(dir.path().join("checkpoint-excludes"));
        let (c1, c2) = ("a".repeat(40), "b".repeat(64));
        store.put(&c1, b".env\n!keep\n\xff\n").unwrap();
        store.put(&c2, b".env\n!keep\n\xff\n").unwrap();
        assert_eq!(store.get(&c1).as_deref(), Some(&b".env\n!keep\n\xff\n"[..]));
        assert_eq!(store.get(&c2).as_deref(), Some(&b".env\n!keep\n\xff\n"[..]));
        assert_eq!(store.get(&"c".repeat(40)), None);
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir.path().join("checkpoint-excludes")), 0o700);
        let all = files(dir.path());
        assert!(!all.is_empty());
        for p in &all {
            assert_eq!(mode(p), 0o600, "{}", p.display());
        }
        // The same rules are kept once.
        let copies = all
            .iter()
            .filter(|p| std::fs::read(p).unwrap() == b".env\n!keep\n\xff\n")
            .count();
        assert_eq!(copies, 1);
        // Empty rules are rules too.
        store.put(&"d".repeat(40), b"").unwrap();
        assert_eq!(store.get(&"d".repeat(40)), Some(Vec::new()));
    }

    #[test]
    fn only_object_ids_name_entries() {
        let dir = tempfile::tempdir().unwrap();
        let store = ExcludesStore::new(dir.path().join("x"));
        for bad in [
            "",
            "../x",
            "ABCDEF0123456789ABCDEF0123456789ABCDEF01",
            "a/b",
            &"a".repeat(41),
        ] {
            assert!(store.put(bad, b"r").is_err(), "{bad:?}");
            assert_eq!(store.get(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn a_damaged_entry_reads_as_none() {
        let dir = tempfile::tempdir().unwrap();
        let store = ExcludesStore::new(dir.path().join("x"));
        let c = "e".repeat(40);
        store.put(&c, b"rules\n").unwrap();
        for p in files(dir.path()) {
            if std::fs::read(&p).unwrap() == b"rules\n" {
                std::fs::write(&p, b"other\n").unwrap();
            }
        }
        assert_eq!(store.get(&c), None);
    }
}
