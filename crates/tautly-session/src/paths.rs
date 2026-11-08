//! Filesystem locations shared by every crate that persists data.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// The Tautly data directory: `TAUTLY_HOME` used as-is, else `$XDG_DATA_HOME/tautly`,
/// else `~/.local/share/tautly`. Empty variables are ignored.
///
/// An error when the result would not be an absolute path (no `HOME`, or a relative
/// variable): a relative directory would resolve against the working directory, which is
/// usually the repository, and put sessions, blobs and memory episodes (tool output that
/// may hold secrets) inside it.
pub fn data_home() -> Result<PathBuf> {
    data_home_from(|name| std::env::var_os(name))
}

/// [`data_home`] with the environment lookup injected.
pub fn data_home_from(get: impl Fn(&str) -> Option<OsString>) -> Result<PathBuf> {
    let non_empty = |name: &str| get(name).filter(|v| !v.is_empty()).map(PathBuf::from);
    let absolute = |name: &str, dir: PathBuf| {
        if dir.is_absolute() {
            Ok(dir)
        } else {
            Err(Error::DataHome(format!("{name} is not an absolute path")))
        }
    };
    if let Some(home) = non_empty("TAUTLY_HOME") {
        return absolute("TAUTLY_HOME", home);
    }
    if let Some(xdg) = non_empty("XDG_DATA_HOME") {
        return absolute("XDG_DATA_HOME", xdg).map(|d| d.join("tautly"));
    }
    match non_empty("HOME") {
        Some(home) => absolute("HOME", home).map(|d| d.join(".local").join("share").join("tautly")),
        None => Err(Error::DataHome("HOME is not set".to_string())),
    }
}

/// Stable 16-hex-digit identifier of a work tree: blake3 of its canonical path
/// (the path as given when it cannot be canonicalized). Keys the code index, whose
/// contents differ between worktrees; sessions use [`project_id`].
pub fn project_hash(root: &Path) -> String {
    path_hash(&canonical_root(root))
}

/// Stable 16-hex-digit identifier of a project (spec 6.5 H10): blake3 of the canonical git
/// common directory when `root` is in a git work tree, so every worktree of a repository
/// has the same id; else [`project_hash`] of the root. Keys session logs, blobs and
/// memory episodes. Read from the git files; never runs git.
///
/// The git directory is the nearest ancestor's `.git`, as git finds it (without
/// `GIT_CEILING_DIRECTORIES`): a directory that is not itself a repository but lies inside
/// one (for example under a home directory kept in git) shares that repository's id.
/// Listings span such directories; [`list_here`](crate::list_here) keeps one root's.
///
/// A `.git` file or `commondir` counts only when git's own back-link confirms it
/// ([`git_dirs_at`](crate::git_dirs_at)): a checkout that arrived with a `.git` naming
/// another repository's directories gets its root's hash, not that project's id (and so
/// not its session logs or memory episodes).
pub fn project_id(root: &Path) -> String {
    project_id_with(root, crate::git::dirs(root).as_ref())
}

/// [`project_id`] with the git directories already resolved.
pub(crate) fn project_id_with(root: &Path, dirs: Option<&crate::git::GitDirs>) -> String {
    match dirs {
        Some(dirs) => path_hash(&dirs.common_dir),
        None => project_hash(root),
    }
}

fn path_hash(path: &Path) -> String {
    let hash = blake3::hash(path.as_os_str().as_encoded_bytes());
    hash.to_hex()[..16].to_string()
}

pub(crate) fn canonical_root(root: &Path) -> PathBuf {
    root.canonicalize().unwrap_or_else(|_| root.to_path_buf())
}

/// `<data_home>/sessions/<project_id>`.
pub(crate) fn project_dir(data_home: &Path, root: &Path) -> PathBuf {
    sessions_dir(data_home, &project_id(root))
}

/// `<data_home>/sessions/<project>`.
pub fn sessions_dir(data_home: &Path, project: &str) -> PathBuf {
    data_home.join("sessions").join(project)
}

/// `create_dir_all` with mode 0700 for every directory it creates: logs and
/// blobs hold tool output that may contain secrets.
pub fn create_private_dir_all(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}
