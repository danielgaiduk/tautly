//! Paths that always require approval, regardless of mode (spec 5.5).

use std::path::{Component, Path, PathBuf};

/// Directories whose entire subtree is protected, at any depth.
const PROTECTED_DIRS: &[&str] = &[".git", ".tautly", ".vscode", ".idea"];

/// Files protected by basename, at any depth.
const PROTECTED_FILES: &[&str] = &[
    ".envrc",
    // Git reads it once a setup step includes it (`include.path = ../.gitconfig`).
    ".gitconfig",
    "makefile",
    "gnumakefile",
    "package.json",
    "cargo.toml",
    "build.rs",
    ".bashrc",
    ".zshrc",
    ".profile",
    ".bash_profile",
    ".zprofile",
    ".zshenv",
    ".zlogin",
    ".zlogout",
    ".bash_login",
    ".bash_logout",
    "config.fish",
];

/// Whether `rel` (relative to the workspace root, lexically normalized) is
/// protected. Comparison is case-insensitive because common filesystems are.
pub fn is_protected(rel: &Path) -> bool {
    let parts: Vec<String> = rel
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy().to_lowercase()),
            _ => None,
        })
        .collect();
    if parts.iter().any(|p| PROTECTED_DIRS.contains(&p.as_str())) {
        return true;
    }
    if parts
        .windows(2)
        .any(|w| w[0] == ".github" && w[1] == "workflows")
    {
        return true;
    }
    parts
        .last()
        .is_some_and(|name| PROTECTED_FILES.contains(&name.as_str()))
}

/// Reason a tool-supplied path (taken literally, no shell expansion) needs
/// approval: outside the workspace or protected, judged on both the lexical
/// path and the path a write would actually reach after following symlinks
/// (a repository can commit `NOTES.md -> .git/config`).
pub(crate) fn path_reason(root: &Path, cwd: &Path, target: &str) -> Option<String> {
    if target == "/dev/null" {
        return None;
    }
    let rebased = rebase(cwd, target);
    let target = rebased.as_str();
    let Some(rel) = normalize(root, target) else {
        return Some(format!("path outside workspace: {target}"));
    };
    let resolved = resolve_links(root, Path::new(target));
    if is_protected(&rel) {
        // Name the real target too, so an approval prompt cannot hide it.
        return Some(match resolved {
            Some(Resolved::Inside(real)) if !same_path(&real, &rel) => format!(
                "protected path: {} (symlink to {})",
                rel.display(),
                real.display()
            ),
            _ => format!("protected path: {}", rel.display()),
        });
    }
    match resolved? {
        Resolved::Inside(real) => is_protected(&real)
            .then(|| format!("protected path: {} (via symlink {target})", real.display())),
        Resolved::Outside(real) => Some(format!(
            "path outside workspace: {} (via symlink {target})",
            real.display()
        )),
        Resolved::Loop => Some(format!("too many levels of symlinks: {target}")),
    }
}

/// Whether two root-relative paths are the same file name for name; the case of a
/// spelling alone (`.GIT` for `.git`) is not a symlink.
fn same_path(a: &Path, b: &Path) -> bool {
    a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
}

/// `target` as seen from working directory `cwd` (lexical; root-relative or
/// absolute). Absolute targets are unchanged.
pub(crate) fn rebase(cwd: &Path, target: &str) -> String {
    if cwd.as_os_str().is_empty() || target.starts_with('/') {
        target.to_string()
    } else {
        cwd.join(target).to_string_lossy().into_owned()
    }
}

/// What a path names once every symlink along it is followed.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Resolved {
    /// Root-relative path (the lexical one when no symlink is involved).
    Inside(PathBuf),
    /// A symlink leads outside the root; the absolute path reached.
    Outside(PathBuf),
    /// Too many symlinks (a loop).
    Loop,
}

/// Follow symlinks along `target` (relative to `root`, or absolute) the way the
/// kernel would, including dangling links (a shell redirect creates their
/// target) and `..` after a link; see [`crate::walk::walk`], which
/// `LocalSandbox::resolve` shares. `None` when `root` does not exist, so there is
/// nothing on disk to follow, or when the walk hits an error such as a file used as
/// a directory (the lexical checks still apply to such a path).
pub(crate) fn resolve_links(root: &Path, target: &Path) -> Option<Resolved> {
    let root = root.canonicalize().ok()?;
    crate::walk::walk(&root, target).ok()
}

/// Reason a write-redirect target (a literal shell word) needs approval.
pub(crate) fn redirect_reason(root: &Path, cwd: &Path, target: &str) -> Option<String> {
    if target == "/dev/null" {
        return None;
    }
    if target.starts_with('~') || target.contains('$') {
        return Some(format!("home-relative or variable path: {target}"));
    }
    if target.contains(['{', '*', '?', '[']) {
        return Some(format!("unresolvable path pattern: {target}"));
    }
    path_reason(root, cwd, target)
}

/// Reason an argv word (a literal shell word, not necessarily a path) needs
/// approval. Only words that are or may be protected paths trigger. For
/// `--opt=value` only the value is checked; for `key=value` (`dd of=x`) both the
/// whole word and the value are.
pub(crate) fn argv_word_reason(root: &Path, cwd: &Path, word: &str) -> Option<String> {
    match word.split_once('=') {
        Some((_, value)) if word.starts_with('-') => path_word_reason(root, cwd, value),
        Some((_, value)) => {
            path_word_reason(root, cwd, word).or_else(|| path_word_reason(root, cwd, value))
        }
        None if word.starts_with('-') => None,
        None => path_word_reason(root, cwd, word),
    }
}

fn path_word_reason(root: &Path, cwd: &Path, word: &str) -> Option<String> {
    if matches!(word, "~" | "$HOME" | "${HOME}") {
        return None;
    }
    for prefix in ["~/", "$HOME/", "${HOME}/"] {
        if let Some(tail) = word.strip_prefix(prefix) {
            return is_protected(Path::new(tail)).then(|| format!("protected path: {word}"));
        }
    }
    if word.starts_with('~') || word.starts_with("$HOME") || word.starts_with("${HOME}") {
        return Some(format!("home-relative path: {word}"));
    }
    if word.contains(['$', '{', '*', '?', '[']) {
        // Cannot be resolved statically; only matters if it could name a protected path.
        let lower = word.to_lowercase();
        let token = PROTECTED_DIRS
            .iter()
            .chain(PROTECTED_FILES)
            .chain(&[".github"])
            .any(|t| lower.contains(t));
        return token.then(|| format!("unresolvable path pattern: {word}"));
    }
    let rebased = rebase(cwd, word);
    let word = rebased.as_str();
    let rel = normalize(root, word)?;
    if is_protected(&rel) {
        return Some(format!("protected path: {}", rel.display()));
    }
    // A word is not necessarily a path, so only a protected symlink target, or a
    // link to a device (`s.sh -> /dev/stdin` fed to an interpreter), counts.
    match resolve_links(root, Path::new(word))? {
        Resolved::Inside(real) if is_protected(&real) => Some(format!(
            "protected path: {} (via symlink {word})",
            real.display()
        )),
        Resolved::Outside(real) if real.starts_with("/dev") || real.starts_with("/proc") => Some(
            format!("symlink to device path: {word} -> {}", real.display()),
        ),
        _ => None,
    }
}

/// Lexically normalize `target` against `root`; `None` if it lands outside.
pub(crate) fn normalize(root: &Path, target: &str) -> Option<PathBuf> {
    let p = Path::new(target);
    let mut parts: Vec<&std::ffi::OsStr> = Vec::new();
    let rel = if p.is_absolute() {
        let root_norm = lexical(root);
        let abs = lexical(p);
        abs.strip_prefix(&root_norm).ok()?.to_path_buf()
    } else {
        p.to_path_buf()
    };
    for c in rel.components() {
        match c {
            Component::Normal(s) => parts.push(s),
            Component::ParentDir => {
                parts.pop()?;
            }
            Component::CurDir => {}
            _ => return None,
        }
    }
    Some(parts.into_iter().collect())
}

/// `p` with `.` and `..` components resolved by name (no links followed).
pub fn lexical(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            c => out.push(c),
        }
    }
    out
}
