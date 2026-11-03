//! Git facts read straight from the repository's files, never by running git, so session
//! start stays fast (spec 6.5 H3, H10): where the git directory and the common directory
//! of a work tree are, and what `HEAD` points at.
//!
//! Every file is read with a size cap, and ref names are checked before they become paths,
//! so a hostile repository cannot make this read outside its git directories or read much.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

/// Largest `.git` file, `HEAD`, `commondir` or loose ref read.
const SMALL_FILE_MAX: u64 = 4096;
/// Largest `packed-refs` scanned. A scan costs about 1 ms per MB; this keeps a packed-only
/// branch well inside the 50 ms cold-start budget. Bigger files give no head.
const PACKED_REFS_MAX: u64 = 8 * 1024 * 1024;
/// Symbolic refs followed before giving up (git's own limit is 5).
const MAX_SYMREF_DEPTH: usize = 5;

/// The git directories of a work tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GitDirs {
    /// The work tree's own git directory (`.git`, or `<common>/worktrees/<name>`).
    pub git_dir: PathBuf,
    /// The directory shared by every worktree of the repository (objects, branches).
    pub common_dir: PathBuf,
}

/// The git directories of the work tree holding `root` (its nearest ancestor with a `.git`
/// directory or `gitdir:` file); None outside git or when they cannot be read.
pub(crate) fn dirs(root: &Path) -> Option<GitDirs> {
    let dot_git = root
        .ancestors()
        .map(|dir| dir.join(".git"))
        .find(|p| p.symlink_metadata().is_ok())?;
    let git_dir = if dot_git.is_dir() {
        dot_git
    } else {
        let text = read_small(&dot_git)?;
        let target = text.strip_prefix("gitdir:")?.trim();
        let base = dot_git.parent()?;
        base.join(target)
    };
    if !git_dir.join("HEAD").is_file() {
        return None;
    }
    let common_dir = match read_small(&git_dir.join("commondir")) {
        Some(rel) if !rel.trim().is_empty() => git_dir.join(rel.trim()),
        _ => git_dir.clone(),
    };
    Some(GitDirs {
        git_dir: git_dir.canonicalize().ok()?,
        common_dir: common_dir.canonicalize().ok()?,
    })
}

/// The commit `HEAD` names in the work tree holding `root`: a detached hash, or the hash
/// of the branch it points at (loose ref or `packed-refs`). None outside git, on an unborn
/// branch, or when the files cannot be read. Never runs git.
///
/// A reftable repository (`HEAD` is `ref: refs/heads/.invalid`, the refs live in
/// `reftable/`) gives None: the head is optional in the header, and the session-start
/// checkpoint, which runs git, records the authoritative one.
pub fn git_head(root: &Path) -> Option<String> {
    head(&dirs(root)?)
}

/// [`git_head`] with the git directories already resolved.
pub(crate) fn head(dirs: &GitDirs) -> Option<String> {
    let mut value = read_small(&dirs.git_dir.join("HEAD"))?;
    for _ in 0..MAX_SYMREF_DEPTH {
        let text = value.trim();
        let Some(name) = text.strip_prefix("ref:") else {
            return is_object_id(text).then(|| text.to_string());
        };
        let name = name.trim();
        if !is_safe_ref_name(name) {
            return None;
        }
        value = match loose_ref(dirs, name) {
            Some(v) => v,
            None => return packed_ref(&dirs.common_dir, name),
        };
    }
    None
}

/// A loose ref: the work tree's own git dir first (per-worktree refs), then the common dir.
fn loose_ref(dirs: &GitDirs, name: &str) -> Option<String> {
    [&dirs.git_dir, &dirs.common_dir]
        .into_iter()
        .find_map(|dir| read_small(&dir.join(name)))
}

/// `name` in `<common>/packed-refs` (`<hash> <name>` lines; `#` and `^` lines, and lines
/// that are not UTF-8, skipped). A file over [`PACKED_REFS_MAX`] is not read at all: the
/// head is optional, and scanning it would break the start-up budget.
fn packed_ref(common_dir: &Path, name: &str) -> Option<String> {
    let file = File::open(common_dir.join("packed-refs")).ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() || meta.len() > PACKED_REFS_MAX {
        return None;
    }
    let mut bytes = Vec::with_capacity(usize::try_from(meta.len()).unwrap_or(0));
    file.take(PACKED_REFS_MAX).read_to_end(&mut bytes).ok()?;
    let want = name.as_bytes();
    bytes.split(|&b| b == b'\n').find_map(|line| {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let space = line.iter().position(|&b| b == b' ')?;
        let (id, refname) = (&line[..space], &line[space + 1..]);
        let id = std::str::from_utf8(id).ok()?;
        (refname == want && is_object_id(id)).then(|| id.to_string())
    })
}

/// A ref name that is safe to join to a git directory: `refs/...`, with no empty, `.` or
/// `..` component and nothing but printable ASCII.
fn is_safe_ref_name(name: &str) -> bool {
    name.starts_with("refs/")
        && name.bytes().all(|b| b.is_ascii_graphic())
        && !name.contains('\\')
        && name
            .split('/')
            .all(|p| !p.is_empty() && p != "." && p != "..")
}

/// A SHA-1 or SHA-256 object id in lowercase hex.
fn is_object_id(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The text of a small regular file; None when missing, unreadable, too big or not UTF-8.
fn read_small(path: &Path) -> Option<String> {
    let file = File::open(path).ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() || meta.len() > SMALL_FILE_MAX {
        return None;
    }
    let mut text = String::new();
    file.take(SMALL_FILE_MAX).read_to_string(&mut text).ok()?;
    Some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ref_names_that_could_leave_the_git_dir_are_refused() {
        for bad in [
            "../x",
            "refs/../../etc/passwd",
            "refs//x",
            "refs/./x",
            "/etc/passwd",
            "HEAD",
            "refs/a b",
        ] {
            assert!(!is_safe_ref_name(bad), "{bad}");
        }
        assert!(is_safe_ref_name("refs/heads/feature/x-1"));
    }
}
