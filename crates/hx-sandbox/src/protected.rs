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

/// Why `target` cannot be silently written, or `None` if it is an ordinary
/// path inside `root`. `target` is a path or a literal shell word.
pub(crate) fn approval_reason(root: &Path, target: &str) -> Option<String> {
    if target == "/dev/null" {
        return None;
    }
    if target.starts_with('~') || target.contains('$') {
        return Some(format!("home-relative or variable path: {target}"));
    }
    if target.contains(['{', '*', '?', '[']) {
        return Some(format!("unresolvable path pattern: {target}"));
    }
    let Some(rel) = relative_to_root(root, Path::new(target)) else {
        return Some(format!("path outside workspace: {target}"));
    };
    if is_protected(&rel) {
        return Some(format!("protected path: {}", rel.display()));
    }
    None
}

/// Lexically normalize `p` against `root`; `None` if it lands outside.
fn relative_to_root(root: &Path, p: &Path) -> Option<PathBuf> {
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
