//! Finding a program on `PATH` that the repository cannot have put there (review B-I3,
//! N2): the one rule every program tautly starts by a bare name follows.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// The first executable `name` in the directories of `path` (a `PATH` value) that the
/// work tree at `root` cannot have put there. Skipped: an empty or relative entry (a
/// program runs with the root as its working directory, so `bin` or an empty entry would
/// name one the repository ships), an entry inside the root by name or once links are
/// resolved, and a candidate whose resolved path is inside the root. "Inside" also means
/// inside any work tree of the root's repository: a path one of whose ancestors has a
/// `.git` resolving to the root's common git directory ([`common_dir`]): the main tree of
/// a linked worktree, a sibling worktree, or the submodule checkout a fork was made from.
///
/// The candidate is returned as found (`<entry>/<name>`, links not resolved), so a
/// multi-call shim that dispatches on its name (a `git` link to `mise`) keeps working;
/// what it resolves to was checked.
pub fn which_outside(name: &str, path: &OsStr, root: &Path) -> Option<PathBuf> {
    which_outside_or_skipped(name, path, root).ok()
}

/// [`which_outside`], or (`Err`) the first executable `name` it skipped because it lies in
/// the work tree (an empty or relative entry is taken against the root, where the program
/// would run), for an error that says why nothing was found: an in-repository toolchain
/// (an activated `.venv/bin`, a direnv profile) is the usual cause.
pub fn which_outside_or_skipped(
    name: &str,
    path: &OsStr,
    root: &Path,
) -> Result<PathBuf, Option<PathBuf>> {
    let real_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let common = common_dir(&real_root);
    let same_repository = |p: &Path| {
        common
            .as_ref()
            .is_some_and(|c| p.ancestors().any(|a| common_dir(a).as_ref() == Some(c)))
    };
    let inside = |p: &Path| p.starts_with(root) || p.starts_with(&real_root) || same_repository(p);
    let mut skipped = None;
    for dir in std::env::split_paths(path) {
        let candidate = dir.join(name);
        let real_dir = dir.canonicalize().ok();
        let real = candidate.canonicalize().ok();
        let in_tree = !dir.is_absolute()
            || inside(&dir)
            || real_dir.as_deref().is_some_and(inside)
            || real.as_deref().is_some_and(inside);
        if in_tree {
            if skipped.is_none() {
                let at = if dir.is_absolute() {
                    candidate
                } else {
                    real_root.join(&dir).join(name)
                };
                skipped = at.canonicalize().ok().filter(|r| executable(r)).map(|_| at);
            }
            continue;
        }
        if real.as_deref().is_some_and(executable) {
            return Ok(candidate);
        }
    }
    Err(skipped)
}

/// The common git directory of the work tree at `dir` (canonical), if `dir` has a `.git`:
/// that directory itself; or, for a `gitdir:` file, the directory it names, or where that
/// directory's `commondir` leads (a linked worktree's). So a main work tree, its linked
/// worktrees, and a submodule checkout (`<super>/.git/modules/<name>`) with its own linked
/// worktrees each resolve to their repository's one common directory (CP5 review I2,
/// re-review N3). The `.git` is not followed when it is a link, only small files are read,
/// and git's back-links are not verified: it only widens what is skipped.
fn common_dir(dir: &Path) -> Option<PathBuf> {
    let small = |p: &Path| {
        let m = std::fs::symlink_metadata(p).ok()?;
        (m.is_file() && m.len() <= 4096)
            .then(|| std::fs::read_to_string(p).ok())
            .flatten()
    };
    let dot_git = dir.join(".git");
    if std::fs::symlink_metadata(&dot_git).is_ok_and(|m| m.is_dir()) {
        return dot_git.canonicalize().ok();
    }
    let text = small(&dot_git)?;
    let git_dir = dir.join(text.lines().next()?.strip_prefix("gitdir:")?.trim());
    match small(&git_dir.join("commondir")) {
        Some(common) => git_dir.join(common.trim()).canonicalize().ok(),
        None => git_dir.canonicalize().ok(),
    }
}

/// A regular file with an execute bit (links followed).
fn executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An executable script at `path`.
    fn program(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn skips_relative_entries_and_the_work_tree() {
        let top = tempfile::tempdir().unwrap();
        let top = top.path().canonicalize().unwrap();
        let root = top.join("repo");
        program(&root.join("git"));
        program(&root.join("bin/git"));
        program(&root.join("node_modules/.bin/git"));
        let outside = top.join("tools");
        program(&outside.join("git"));
        // The repository's directories named relative to this process's working
        // directory: a relative entry is found there, and the program then runs with the
        // root as its working directory (probe P1).
        let cwd = std::env::current_dir().unwrap().canonicalize().unwrap();
        let up = "../".repeat(cwd.components().count() - 1);
        let rel = |p: &Path| format!("{up}{}", p.strip_prefix("/").unwrap().display());
        assert!(Path::new(&rel(&root.join("bin"))).join("git").is_file());
        std::os::unix::fs::symlink(&outside, root.join("linked")).unwrap();
        let o = outside.display();
        for path in [
            // An empty entry is the working directory: the root (probe P1b).
            format!(":{o}"),
            format!("{}:{o}", rel(&root.join("bin"))),
            format!("bin:node_modules/.bin:{o}"),
            format!(
                "{}:{}:{}:{o}",
                root.display(),
                root.join("bin").display(),
                root.join("node_modules/.bin").display(),
            ),
            format!("{}/./bin:{o}", root.display()),
            format!("{}/../bin:{o}", root.join("node_modules").display()),
        ] {
            let found = which_outside("git", OsStr::new(&path), &root);
            assert_eq!(found, Some(outside.join("git")), "PATH={path}");
        }
        // Only the repository's: none.
        let path = format!(":bin:{}", root.join("bin").display());
        assert_eq!(which_outside("git", OsStr::new(&path), &root), None);
        // A directory reached through a link inside the root is the repository's too.
        let path = root.join("linked");
        assert_eq!(which_outside("git", path.as_os_str(), &root), None);
        // Not executable, or a directory: skipped.
        let plain = top.join("plain");
        std::fs::create_dir_all(plain.join("git")).unwrap();
        let path = format!("{}:{o}", plain.display());
        assert_eq!(
            which_outside("git", OsStr::new(&path), &root),
            Some(outside.join("git"))
        );
    }

    /// Review N4: the candidate is returned unresolved, so a shim that dispatches on its
    /// name still sees `git`; a link that resolves into the root is refused.
    #[test]
    fn returns_the_link_not_its_target() {
        let top = tempfile::tempdir().unwrap();
        let top = top.path().canonicalize().unwrap();
        let root = top.join("repo");
        std::fs::create_dir_all(&root).unwrap();
        let shims = top.join("shims");
        program(&top.join("mise"));
        std::fs::create_dir_all(&shims).unwrap();
        std::os::unix::fs::symlink(top.join("mise"), shims.join("git")).unwrap();
        assert_eq!(
            which_outside("git", shims.as_os_str(), &root),
            Some(shims.join("git"))
        );
        program(&root.join("evil"));
        std::fs::remove_file(shims.join("git")).unwrap();
        std::os::unix::fs::symlink(root.join("evil"), shims.join("git")).unwrap();
        assert_eq!(which_outside("git", shims.as_os_str(), &root), None);
    }
}
