//! Remembered approvals of repository-declared servers (spec 7.11.10), in
//! `<data home>/mcp-approvals.json`.
//!
//! An approval is keyed by [`key`]: the canonical workspace root, the server name and
//! what decides what runs (transport, command line or URL, working directory, variable
//! and header names and values, and the content of the files a stdio command line names
//! inside the root). Any change to those asks again.

use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::config::ServerSpec;

/// The approvals file in the data home.
pub const APPROVALS_FILE: &str = "mcp-approvals.json";
/// A larger approvals file is not read (it is treated as empty).
const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;

/// Serializes this process's writers; the lock file (`flock`) serializes processes.
static WRITERS: Mutex<()> = Mutex::new(());

/// The approval key of server `name`, declared by the repository at `root` as `spec`:
/// lowercase blake3 hex of the canonical root, the name, the transport, the argv (or the
/// URL), the working directory, the sorted variable (or header) names, and a hash of the
/// sorted `(name, value)` pairs; for a stdio server also what the files its command line
/// names inside the root hold (see `files_named`). Reads those files (bounded).
///
/// Only repository servers have keys (the user's and the client's own servers need no
/// approval). Their values are repository content, not the user's secrets, and they decide
/// what runs (`NODE_OPTIONS`, `LD_PRELOAD`, `PATH`), so a changed value asks again.
pub fn key(root: &Path, name: &str, spec: &ServerSpec) -> String {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let pairs_hash = |m: &BTreeMap<String, String>| {
        let pairs: Vec<(&String, &String)> = m.iter().collect();
        blake3::hash(serde_json::to_string(&pairs).unwrap_or_default().as_bytes())
            .to_hex()
            .to_string()
    };
    let launch = match spec {
        ServerSpec::Stdio {
            command,
            args,
            env,
            cwd,
        } => {
            let argv: Vec<&String> = std::iter::once(command).chain(args).collect();
            let base = match cwd {
                Some(c) => root.join(c),
                None => root.clone(),
            };
            serde_json::json!({
                "transport": "stdio",
                "argv": argv,
                "cwd": cwd.as_ref().map(|c| c.to_string_lossy().into_owned()),
                "env": env.keys().collect::<Vec<_>>(),
                "env_values": pairs_hash(env),
                "files": files_named(&root, &base, &argv),
            })
        }
        ServerSpec::Http { url, headers } => serde_json::json!({
            "transport": "http",
            "url": url.as_str(),
            "headers": headers.keys().collect::<Vec<_>>(),
            "header_values": pairs_hash(headers),
        }),
    };
    let input = serde_json::json!({
        "root": root.to_string_lossy(),
        "name": name,
        "launch": launch,
    });
    blake3::hash(input.to_string().as_bytes())
        .to_hex()
        .to_string()
}

/// What the files a stdio server's command line names inside the (canonical) `root`
/// are, for its approval key: the code the server runs is usually a script in the
/// repository (`node scripts/mcp.js`), which a later revision can rewrite without
/// touching the command line.
///
/// Each word of `argv` (the command included), and the value after the first `=` of a
/// word such as `--require=./x.js`, is taken as a path (relative ones against `base`, the
/// server's working directory). One that lies inside the root, by name or once links are
/// resolved, adds an entry: the blake3 of the file's content when it resolves to a
/// regular file inside the root of at most [`MAX_KEYED_FILE`] bytes ([`MAX_KEYED_TOTAL`]
/// for all of them); the resolved path when it resolves outside the root (a link the
/// repository can re-point); `missing` or `dir`. A file that cannot be hashed (too big,
/// unreadable, not a regular file) gets a value that is new on every call, so the key
/// never matches a remembered approval and the user is asked each time. Words naming
/// nothing in the root (flags, package names, programs found on `PATH`) add nothing.
///
/// Not covered: what the script loads in turn (imports, `node_modules`), files named
/// only through variables, and programs found through a `PATH` the server's `env` sets.
fn files_named(root: &Path, base: &Path, argv: &[&String]) -> Vec<serde_json::Value> {
    let mut budget = MAX_KEYED_TOTAL;
    let mut entries = Vec::new();
    for (i, word) in argv.iter().enumerate() {
        let value = word.split_once('=').map(|(_, v)| v);
        for candidate in std::iter::once(word.as_str()).chain(value) {
            if candidate.is_empty() {
                continue;
            }
            let path = base.join(candidate);
            let real = path.canonicalize().ok();
            let inside = lexical(&path).starts_with(root)
                || real.as_ref().is_some_and(|r| r.starts_with(root));
            if !inside {
                continue;
            }
            let state = match real {
                None => serde_json::json!("missing"),
                Some(r) if !r.starts_with(root) => {
                    serde_json::json!({ "target": r.to_string_lossy() })
                }
                Some(r) if r.is_dir() => serde_json::json!("dir"),
                Some(r) => match hash_file(&r, &mut budget) {
                    Some(h) => serde_json::json!({ "blake3": h }),
                    None => serde_json::json!({ "unknown": nonce() }),
                },
            };
            entries.push(serde_json::json!({ "word": i, "path": candidate, "state": state }));
        }
    }
    entries
}

/// Largest file inside the root whose content an approval key covers.
const MAX_KEYED_FILE: u64 = 1024 * 1024;
/// Most bytes of files read for one approval key.
const MAX_KEYED_TOTAL: u64 = 8 * 1024 * 1024;

/// The blake3 hex of the regular file at `path`, if it is at most [`MAX_KEYED_FILE`]
/// bytes and fits in `budget` (which it is taken from). Opened without blocking, so a
/// FIFO is refused rather than waited on.
fn hash_file(path: &Path, budget: &mut u64) -> Option<String> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
        .ok()?;
    let meta = file.metadata().ok()?;
    let cap = MAX_KEYED_FILE.min(*budget);
    if !meta.is_file() || meta.len() > cap {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(cap + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 > cap {
        return None;
    }
    *budget -= bytes.len() as u64;
    Some(blake3::hash(&bytes).to_hex().to_string())
}

/// `path` with `.` and `..` components resolved by name (no links followed).
fn lexical(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// A value no other call returns: keys an approval that must never be remembered.
fn nonce() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let seed = format!("{}:{t}:{n}", std::process::id());
    blake3::hash(seed.as_bytes()).to_hex().to_string()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct File {
    #[serde(default)]
    approvals: BTreeMap<String, Entry>,
}

/// What an approval was for, so the file can be read (and pruned) by hand.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    root: String,
    name: String,
    /// Unix seconds.
    approved_at: u64,
}

/// The approvals file, read at [`open`](Self::open).
#[derive(Debug)]
pub struct ApprovalStore {
    path: PathBuf,
    file: File,
}

impl ApprovalStore {
    /// Read the store at `path`; a missing, unreadable, oversized or corrupt file is an
    /// empty store.
    pub fn open(path: &Path) -> Self {
        ApprovalStore {
            path: path.to_path_buf(),
            file: read(path),
        }
    }

    pub fn is_approved(&self, key: &str) -> bool {
        self.file.approvals.contains_key(key)
    }

    /// Record an approval. Writers are serialized (a process-wide mutex, then an `flock`
    /// on `<file>.lock` against other processes); under the lock the file is read again,
    /// merged and replaced atomically through a uniquely named temporary file (mode 0600,
    /// fsync, rename). The directory is created 0700 when missing. Approvals recorded by
    /// others since [`open`](Self::open) are kept.
    pub fn approve(&mut self, key: &str, root: &Path, name: &str) -> io::Result<()> {
        let _writer = WRITERS.lock().unwrap_or_else(PoisonError::into_inner);
        let dir = parent(&self.path);
        if !dir.exists() {
            std::fs::create_dir_all(dir)?;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
        let _lock = FileLock::take(&lock_path(&self.path))?;
        let mut file = read(&self.path);
        file.approvals
            .extend(std::mem::take(&mut self.file.approvals));
        let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        let approved_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        file.approvals.insert(
            key.to_string(),
            Entry {
                root: root.to_string_lossy().into_owned(),
                name: name.to_string(),
                approved_at,
            },
        );
        let bytes = serde_json::to_vec_pretty(&file).map_err(io::Error::other)?;
        write_atomic(&self.path, &bytes)?;
        self.file = file;
        Ok(())
    }
}

fn parent(path: &Path) -> &Path {
    path.parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

fn lock_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".lock");
    path.with_file_name(name)
}

/// An exclusive `flock` on a lock file, released when dropped (the descriptor closes).
struct FileLock(#[allow(dead_code)] std::fs::File);

impl FileLock {
    fn take(path: &Path) -> io::Result<FileLock> {
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)?;
        // SAFETY: `flock` on a descriptor this function owns.
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(FileLock(f))
    }
}

/// The store at `path`, read through one descriptor that must be a regular file, at most
/// [`MAX_FILE_BYTES`].
fn read(path: &Path) -> File {
    let Ok(f) = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
    else {
        return File::default();
    };
    if !f.metadata().is_ok_and(|m| m.is_file()) {
        return File::default();
    }
    let mut bytes = Vec::new();
    if f.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes).is_err()
        || bytes.len() as u64 > MAX_FILE_BYTES
    {
        return File::default();
    }
    serde_json::from_slice(&bytes).unwrap_or_default()
}

fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = parent(path);
    // A unique name per write; created 0600 by `tempfile`, removed if anything fails.
    let mut tmp = tempfile::Builder::new()
        .prefix(".mcp-approvals.")
        .suffix(".tmp")
        .tempfile_in(dir)?;
    tmp.as_file()
        .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}
