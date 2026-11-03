//! Root-relative paths for records (spec 6.5 H2): a log written in one checkout (or
//! worktree) names files that can be found under another, and a path read back from a log
//! is re-confined under the current root before use.

use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

/// A path relative to the session root, `/`-separated, as records store it.
///
/// Deserializing accepts any string: a bad path in one record must not make the whole log
/// unreadable. [`resolve`](Self::resolve) is the only way back to a filesystem path, and it
/// refuses anything that could leave the root.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RelPath(String);

/// Why a [`RelPath`] cannot be resolved under a root.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfineError {
    #[error("empty path")]
    Empty,
    #[error("path {0:?} is absolute")]
    Absolute(String),
    #[error("path {0:?} has a `..` component")]
    ParentDir(String),
    #[error("path contains a NUL byte")]
    Nul,
    #[error("path {0:?} has an empty or `.` component")]
    CurDir(String),
}

impl RelPath {
    /// `p` relative to `root`; None when `p` is not below `root` (or is `root` itself), has
    /// a `..` component, or is not valid UTF-8 (a record holds strings). Both paths are
    /// taken as given: the caller passes the canonical root and canonical paths.
    pub fn from_abs(root: &Path, p: &Path) -> Option<RelPath> {
        let rest = p.strip_prefix(root).ok()?;
        let mut parts = Vec::new();
        for c in rest.components() {
            match c {
                Component::Normal(s) => parts.push(s.to_str()?),
                Component::CurDir => {}
                Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
            }
        }
        let rel = RelPath(parts.join("/"));
        rel.check().ok()?;
        Some(rel)
    }

    /// The stored text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The path under `root`. Refuses an empty path, an absolute one, a `..`, `.` or empty
    /// component and a NUL byte. The check is lexical: a symlink below `root` is the
    /// sandbox's business when the path is used.
    pub fn resolve(&self, root: &Path) -> Result<PathBuf, ConfineError> {
        self.check()?;
        Ok(root.join(&self.0))
    }

    fn check(&self) -> Result<(), ConfineError> {
        let s = self.0.as_str();
        if s.is_empty() {
            return Err(ConfineError::Empty);
        }
        if s.contains('\0') {
            return Err(ConfineError::Nul);
        }
        if s.starts_with('/') {
            return Err(ConfineError::Absolute(s.to_string()));
        }
        for part in s.split('/') {
            match part {
                ".." => return Err(ConfineError::ParentDir(s.to_string())),
                "" | "." => return Err(ConfineError::CurDir(s.to_string())),
                _ => {}
            }
        }
        Ok(())
    }
}
