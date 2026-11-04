//! Git facts read straight from the repository's files, never by running git, so session
//! start stays fast (spec 6.5 H3, H10): where the git directory and the common directory
//! of a work tree are, and what `HEAD` points at.
//!
//! Every file is read with a size cap, and ref names are checked before they become paths,
//! so a hostile repository cannot make this read outside its git directories or read much.

use std::io::Read;
use std::path::{Path, PathBuf};

/// Largest `.git` file, `HEAD`, `commondir` or loose ref read.
const SMALL_FILE_MAX: u64 = 4096;
/// Largest `packed-refs` scanned. A scan costs about 1 ms per MB; this keeps a packed-only
/// branch well inside the 50 ms cold-start budget. Bigger files give no head.
const PACKED_REFS_MAX: u64 = 8 * 1024 * 1024;
/// Symbolic refs followed before giving up (git's own limit is 5).
const MAX_SYMREF_DEPTH: usize = 5;

/// Largest git `config` scanned for a submodule's `core.worktree`.
const CONFIG_MAX: u64 = 64 * 1024;

/// The git directories of a work tree (canonical paths).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitDirs {
    /// The work tree's own git directory (`.git`, or `<common>/worktrees/<name>`).
    pub git_dir: PathBuf,
    /// The directory shared by every worktree of the repository (objects, branches).
    pub common_dir: PathBuf,
}

/// The git directories of the work tree holding `root` (its nearest ancestor with a
/// `.git`), as [`git_dirs_at`] verifies them; None outside git, or when they cannot be read or
/// verified.
pub(crate) fn dirs(root: &Path) -> Option<GitDirs> {
    let dot_git = root
        .ancestors()
        .map(|dir| dir.join(".git"))
        .find(|p| p.symlink_metadata().is_ok())?;
    git_dirs_at(&dot_git)
}

/// The git directories a work tree's `.git` entry names, trusted only as far as git's own
/// links confirm them. A checkout that arrived as an archive with its `.git` controls
/// that entry, and must not be able to name another repository's directories: they key
/// that project's session logs and private memory episodes, and checkpoints write refs
/// there. So:
///
/// - a `.git` directory is the repository, unless it has a `commondir` (a real
///   repository has none: git would share another repository's objects and refs);
/// - a `.git` file (`gitdir: <dir>`) counts when `<dir>` names it back: a linked
///   worktree's `<dir>` is `<common>/worktrees/<name>` (its `commondir` leading to
///   `<common>`) and its `gitdir` file names this `.git`; a submodule's (or any other
///   separate git directory's) has no `commondir`, and its `config` sets `core.worktree`
///   to this work tree;
/// - a `.git` symbolic link never counts.
///
/// None otherwise (also when a file is missing, unreadable or too big, or `HEAD` is
/// missing). Never runs git.
pub fn git_dirs_at(dot_git: &Path) -> Option<GitDirs> {
    let meta = dot_git.symlink_metadata().ok()?;
    let (git_dir, common_dir) = if meta.is_dir() {
        if dot_git.join("commondir").symlink_metadata().is_ok() {
            return None;
        }
        let git_dir = dot_git.canonicalize().ok()?;
        (git_dir.clone(), git_dir)
    } else if meta.is_file() {
        let text = read_small(dot_git)?;
        let target = text.lines().next()?.strip_prefix("gitdir:")?.trim();
        if target.is_empty() {
            return None;
        }
        let work_tree = dot_git.parent()?;
        let git_dir = work_tree.join(target).canonicalize().ok()?;
        let common_dir = if git_dir.join("commondir").symlink_metadata().is_ok() {
            linked_worktree_common_dir(&git_dir, dot_git)?
        } else if names_work_tree(&git_dir, work_tree) {
            git_dir.clone()
        } else {
            return None;
        };
        (git_dir, common_dir)
    } else {
        return None;
    };
    if !git_dir.join("HEAD").is_file() {
        return None;
    }
    Some(GitDirs {
        git_dir,
        common_dir,
    })
}

/// The common directory of the linked worktree whose (canonical) git directory is
/// `git_dir`, when git's links confirm it belongs to the `.git` file `dot_git`: `git_dir`
/// is `<common>/worktrees/<name>` and its `gitdir` names `dot_git` (absolute, or relative
/// to `git_dir` as `worktree.useRelativePaths` writes it).
fn linked_worktree_common_dir(git_dir: &Path, dot_git: &Path) -> Option<PathBuf> {
    let rel = read_small(&git_dir.join("commondir"))?;
    let rel = rel.trim();
    if rel.is_empty() {
        return None;
    }
    let common_dir = git_dir.join(rel).canonicalize().ok()?;
    if git_dir.parent()? != common_dir.join("worktrees") {
        return None;
    }
    let back = read_small(&git_dir.join("gitdir"))?;
    let back = git_dir.join(back.trim()).canonicalize().ok()?;
    (back == dot_git.canonicalize().ok()?).then_some(common_dir)
}

/// Whether the configuration of the (canonical) separate git directory `git_dir` sets
/// `core.worktree` to `work_tree`, as git does for a submodule's
/// `<super>/.git/modules/<name>`. Only plain `[core]` lines are understood: anything else
/// (an include, an escaped or quoted value) is not a match.
fn names_work_tree(git_dir: &Path, work_tree: &Path) -> bool {
    let Some(config) = read_capped(&git_dir.join("config"), CONFIG_MAX) else {
        return false;
    };
    let mut in_core = false;
    let mut value = None;
    for line in config.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix('[') {
            in_core = rest
                .strip_suffix(']')
                .is_some_and(|name| name.trim().eq_ignore_ascii_case("core"));
            continue;
        }
        let Some((key, v)) = line.split_once('=') else {
            continue;
        };
        if in_core && key.trim().eq_ignore_ascii_case("worktree") {
            value = Some(v.trim());
        }
    }
    let Some(value) = value.filter(|v| !v.is_empty() && !v.contains(['"', '\\', ';', '#'])) else {
        return false;
    };
    match (git_dir.join(value).canonicalize(), work_tree.canonicalize()) {
        (Ok(named), Ok(here)) => named == here,
        _ => false,
    }
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
    let file = open_nonblocking(&common_dir.join("packed-refs"))?;
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
    read_capped(path, SMALL_FILE_MAX)
}

/// The text of a regular file of at most `max` bytes; None when missing, unreadable, too
/// big or not UTF-8. Opened without blocking, so a FIFO cannot stall start-up.
fn read_capped(path: &Path, max: u64) -> Option<String> {
    let file = open_nonblocking(path)?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() || meta.len() > max {
        return None;
    }
    let mut text = String::new();
    file.take(max).read_to_string(&mut text).ok()?;
    Some(text)
}

/// `path` opened for reading without blocking (a FIFO opens at once, and is then refused
/// as not a regular file).
fn open_nonblocking(path: &Path) -> Option<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
        .ok()
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
