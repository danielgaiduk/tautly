//! Finding a program on `PATH` that the repository cannot have put there (review B-I3,
//! N2): the one rule every program tautly starts by a bare name follows.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// The first executable `name` in the directories of `path` (a `PATH` value) that the
/// work tree at `root` cannot have put there. Skipped: an empty or relative entry (a
/// program runs with the root as its working directory, so `bin` or an empty entry would
/// name one the repository ships), an entry inside the root by name or once links are
/// resolved, and a candidate whose resolved path is inside the root.
///
/// The candidate is returned as found (`<entry>/<name>`, links not resolved), so a
/// multi-call shim that dispatches on its name (a `git` link to `mise`) keeps working;
/// what it resolves to was checked.
pub fn which_outside(name: &str, path: &OsStr, root: &Path) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let real_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let inside = |p: &Path| p.starts_with(root) || p.starts_with(&real_root);
    std::env::split_paths(path)
        .filter(|dir| dir.is_absolute() && !inside(dir))
        .filter_map(|dir| {
            let real_dir = dir.canonicalize().ok()?;
            let candidate = dir.join(name);
            let real = candidate.canonicalize().ok()?;
            let usable = !inside(&real_dir)
                && !inside(&real)
                && std::fs::metadata(&real)
                    .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0);
            usable.then_some(candidate)
        })
        .next()
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
