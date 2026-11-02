//! Paths that always require approval, regardless of mode (spec 5.5).

use std::path::{Component, Path, PathBuf};

/// Directories whose entire subtree is protected, at any depth.
const PROTECTED_DIRS: &[&str] = &[".git", ".hx", ".vscode", ".idea"];

/// Files protected by basename, at any depth.
const PROTECTED_FILES: &[&str] = &[
    ".envrc",
    "makefile",
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
/// approval: outside the workspace or protected.
pub(crate) fn path_reason(root: &Path, target: &str) -> Option<String> {
    if target == "/dev/null" {
        return None;
    }
    let Some(rel) = normalize(root, target) else {
        return Some(format!("path outside workspace: {target}"));
    };
    if is_protected(&rel) {
        return Some(format!("protected path: {}", rel.display()));
    }
    None
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

/// Reason a non-flag argv word (a literal shell word, not necessarily a path)
/// needs approval. Only words that are or may be protected paths trigger.
pub(crate) fn argv_word_reason(root: &Path, word: &str) -> Option<String> {
    let word = if word.starts_with('-') {
        word.split_once('=').map(|(_, v)| v)?
    } else {
        word
    };
    if matches!(word, "~" | "$HOME" | "${HOME}") {
        return None;
    }
    for prefix in ["~/", "$HOME/", "${HOME}/"] {
        if let Some(tail) = word.strip_prefix(prefix) {
            return is_protected(Path::new(tail)).then(|| format!("protected path: {word}"));
        }
    }
    if word.starts_with('~') || word.contains(['$', '{', '*', '?', '[']) {
        return Some(format!("unresolvable path pattern: {word}"));
    }
    let rel = normalize(root, word)?;
    is_protected(&rel).then(|| format!("protected path: {}", rel.display()))
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
