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
/// runs inside the root hold (see `launch_files`). Reads those files (bounded).
///
/// Only repository servers have keys (the user's and the client's own servers need no
/// approval). Their values are repository content, not the user's secrets, and they decide
/// what runs (`NODE_OPTIONS`, `LD_PRELOAD`, `PATH`), so a changed value asks again.
pub fn key(root: &Path, name: &str, spec: &ServerSpec) -> String {
    approval_key(root, name, spec).key
}

/// An approval key, and whether an approval under it may be remembered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalKey {
    pub key: String,
    /// False when a file the command line names could not be hashed (too big,
    /// unreadable, not a regular file), or when the command line can drop the private
    /// bytecode cache the engine forces on it (`python -E`, `env -u PYTHONPYCACHEPREFIX`;
    /// see `launch_files::can_drop_pycache_prefix`): the key then holds a value no other
    /// call produces, so a stored approval could never match. Such a server is approved
    /// for the run only, and nothing is written to the approvals file.
    pub rememberable: bool,
}

/// [`key`], with whether it may be remembered.
pub fn approval_key(root: &Path, name: &str, spec: &ServerSpec) -> ApprovalKey {
    let mut rememberable = true;
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
            // Variable values name files like argv words do (`NODE_OPTIONS=--require
            // ./x.js`, `PYTHONPATH=.`).
            let words: Vec<&String> = argv.iter().copied().chain(env.values()).collect();
            let base = match cwd {
                Some(c) => root.join(c),
                None => root.clone(),
            };
            // A command line that can run its Python without the private bytecode cache
            // the engine forces (`python -E`, `env -u PYTHONPYCACHEPREFIX`, `-X
            // pycache_prefix=`; a flag may sit in a variable the shell expands) is asked
            // about at every start: the key then matches nothing stored.
            let droppable = crate::launch_files::can_drop_pycache_prefix(&words);
            rememberable &= !droppable;
            serde_json::json!({
                "transport": "stdio",
                "argv": argv,
                "cwd": cwd.as_ref().map(|c| c.to_string_lossy().into_owned()),
                "env": env.keys().collect::<Vec<_>>(),
                "env_values": pairs_hash(env),
                "files": crate::launch_files::files_named(&root, &base, &words, &mut rememberable),
                "pycache_prefix": if droppable {
                    serde_json::json!({ "droppable": crate::launch_files::nonce() })
                } else {
                    serde_json::json!("forced")
                },
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
    let key = blake3::hash(input.to_string().as_bytes())
        .to_hex()
        .to_string();
    ApprovalKey { key, rememberable }
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
