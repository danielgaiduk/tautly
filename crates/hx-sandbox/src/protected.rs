//! Paths that always require approval, regardless of mode (spec 5.5).

use std::collections::VecDeque;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

/// Directories whose entire subtree is protected, at any depth.
const PROTECTED_DIRS: &[&str] = &[".git", ".hx", ".vscode", ".idea"];

/// Files protected by basename, at any depth.
const PROTECTED_FILES: &[&str] = &[
    ".envrc",
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
pub(crate) fn path_reason(root: &Path, target: &str) -> Option<String> {
    if target == "/dev/null" {
        return None;
    }
    let Some(rel) = normalize(root, target) else {
        return Some(format!("path outside workspace: {target}"));
    };
    let resolved = resolve_links(root, Path::new(target));
    if is_protected(&rel) {
        // Name the real target too, so an approval prompt cannot hide it.
        return Some(match resolved {
            Some(Resolved::Inside(real)) if real != rel => format!(
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

const MAX_LINKS: usize = 40;

/// Follow symlinks along `target` (relative to `root`, or absolute) the way the
/// kernel would, including dangling links (a shell redirect creates their
/// target) and `..` after a link. Missing components are taken lexically.
/// `None` when `root` does not exist, so there is nothing on disk to follow.
pub(crate) fn resolve_links(root: &Path, target: &Path) -> Option<Resolved> {
    let root = root.canonicalize().ok()?;
    let mut cur = if target.is_absolute() {
        PathBuf::from("/")
    } else {
        root.clone()
    };
    let mut queue: VecDeque<OsString> = target
        .components()
        .map(|c| c.as_os_str().to_owned())
        .collect();
    let mut links = 0;
    while let Some(part) = queue.pop_front() {
        match Path::new(&part).components().next() {
            Some(Component::RootDir) => cur = PathBuf::from("/"),
            Some(Component::ParentDir) => {
                cur.pop();
            }
            Some(Component::Normal(name)) => {
                let next = cur.join(name);
                let link =
                    std::fs::symlink_metadata(&next).is_ok_and(|m| m.file_type().is_symlink());
                match link.then(|| std::fs::read_link(&next).ok()).flatten() {
                    Some(dest) => {
                        links += 1;
                        if links > MAX_LINKS {
                            return Some(Resolved::Loop);
                        }
                        for c in dest.components().rev() {
                            queue.push_front(c.as_os_str().to_owned());
                        }
                    }
                    None => cur = next,
                }
            }
            _ => {}
        }
    }
    Some(match cur.strip_prefix(&root) {
        Ok(rel) => Resolved::Inside(rel.to_path_buf()),
        Err(_) => Resolved::Outside(cur),
    })
}

/// Reason a write-redirect target (a literal shell word) needs approval.
pub(crate) fn redirect_reason(root: &Path, target: &str) -> Option<String> {
    if target == "/dev/null" {
        return None;
    }
    if target.starts_with('~') || target.contains('$') {
        return Some(format!("home-relative or variable path: {target}"));
    }
    if target.contains(['{', '*', '?', '[']) {
        return Some(format!("unresolvable path pattern: {target}"));
    }
    path_reason(root, target)
}

/// Reason an argv word (a literal shell word, not necessarily a path) needs
/// approval. Only words that are or may be protected paths trigger. For
/// `--opt=value` only the value is checked; for `key=value` (`dd of=x`) both the
/// whole word and the value are.
pub(crate) fn argv_word_reason(root: &Path, word: &str) -> Option<String> {
    match word.split_once('=') {
        Some((_, value)) if word.starts_with('-') => path_word_reason(root, value),
        Some((_, value)) => path_word_reason(root, word).or_else(|| path_word_reason(root, value)),
        None if word.starts_with('-') => None,
        None => path_word_reason(root, word),
    }
}

fn path_word_reason(root: &Path, word: &str) -> Option<String> {
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

fn lexical(p: &Path) -> PathBuf {
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
