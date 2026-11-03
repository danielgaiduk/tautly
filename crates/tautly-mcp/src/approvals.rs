//! Remembered approvals of repository-declared servers (spec 7.11.10), in
//! `<data home>/mcp-approvals.json`.
//!
//! An approval is keyed by [`key`]: the canonical workspace root, the server name and
//! what decides what runs (transport, command line or URL, working directory, variable
//! and header names). Any change to those asks again.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::config::ServerSpec;

/// The approvals file in the data home.
pub const APPROVALS_FILE: &str = "mcp-approvals.json";
/// A larger approvals file is not read (it is treated as empty).
const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;

/// The approval key of server `name` declared as `spec` in the repository at `root`:
/// lowercase blake3 hex of the canonical root, the name, the transport, the argv (or the
/// URL), the working directory and the sorted variable (or header) names. Values are left
/// out: they may be secrets, and the prompt never shows them either.
pub fn key(root: &Path, name: &str, spec: &ServerSpec) -> String {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let launch = match spec {
        ServerSpec::Stdio {
            command,
            args,
            env,
            cwd,
        } => serde_json::json!({
            "transport": "stdio",
            "argv": std::iter::once(command).chain(args).collect::<Vec<_>>(),
            "cwd": cwd.as_ref().map(|c| c.to_string_lossy().into_owned()),
            "env": env.keys().collect::<Vec<_>>(),
        }),
        ServerSpec::Http { url, headers } => serde_json::json!({
            "transport": "http",
            "url": url.as_str(),
            "headers": headers.keys().collect::<Vec<_>>(),
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

    /// Record an approval and write the file atomically (temporary file, fsync, rename),
    /// readable by the user only (mode 0600; the directory is created 0700). Approvals
    /// another process recorded since [`open`](Self::open) are kept.
    pub fn approve(&mut self, key: &str, root: &Path, name: &str) -> io::Result<()> {
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

fn read(path: &Path) -> File {
    let Ok(meta) = std::fs::metadata(path) else {
        return File::default();
    };
    if !meta.is_file() || meta.len() > MAX_FILE_BYTES {
        return File::default();
    }
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    if !dir.exists() {
        std::fs::create_dir_all(dir)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    let file_name = path
        .file_name()
        .ok_or_else(|| io::Error::other("the approvals path has no file name"))?;
    let tmp = dir.join(format!(
        ".{}.{}.tmp",
        file_name.to_string_lossy(),
        std::process::id()
    ));
    let result = (|| {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&tmp)?;
        // An existing temporary file keeps its mode; make sure.
        f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        f.write_all(bytes)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)?;
        if let Ok(d) = std::fs::File::open(dir) {
            let _ = d.sync_all();
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}
