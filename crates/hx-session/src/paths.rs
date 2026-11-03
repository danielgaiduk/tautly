//! Filesystem locations shared by every crate that persists data.

use std::path::{Path, PathBuf};

/// The hx data directory: `HX_HOME` used as-is, else `$XDG_DATA_HOME/hx`,
/// else `~/.local/share/hx`. Empty variables are ignored.
pub fn data_home() -> PathBuf {
    if let Some(home) = non_empty_env("HX_HOME") {
        return home;
    }
    non_empty_env("XDG_DATA_HOME")
        .or_else(|| non_empty_env("HOME").map(|h| h.join(".local").join("share")))
        .unwrap_or_else(|| PathBuf::from(".local/share"))
        .join("hx")
}

fn non_empty_env(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// Stable 16-hex-digit identifier of a project: blake3 of its canonical path
/// (the path as given when it cannot be canonicalized).
pub fn project_hash(root: &Path) -> String {
    let canonical = canonical_root(root);
    let hash = blake3::hash(canonical.as_os_str().as_encoded_bytes());
    hash.to_hex()[..16].to_string()
}

pub(crate) fn canonical_root(root: &Path) -> PathBuf {
    root.canonicalize().unwrap_or_else(|_| root.to_path_buf())
}

/// `<data_home>/sessions/<project_hash>`.
pub(crate) fn project_dir(data_home: &Path, root: &Path) -> PathBuf {
    data_home.join("sessions").join(project_hash(root))
}
