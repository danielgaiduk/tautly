//! The repository files a stdio server's command line runs, for its approval key (spec
//! 7.11.10, review B-I1/N1): the code a repository server runs is usually in the
//! repository (`node scripts/mcp.js`, `python -m pkg`, `npm run mcp`), and a later
//! revision can rewrite it without touching the command line.

use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Largest file whose content a key covers.
const MAX_FILE: u64 = 1024 * 1024;
/// Most bytes of files read for one key.
const MAX_TOTAL: u64 = 8 * 1024 * 1024;
/// Most entries read for one key: files, and the directories walked.
const MAX_FILES: u32 = 4000;
/// Directory names never walked: version control, dependencies, build output, caches.
const SKIPPED_DIRS: &[&str] = &[".git", "node_modules", "target", ".venv", "__pycache__"];
/// Commands that run what a manifest in their working directory says.
const RUNNERS: &[&str] = &[
    "npm", "npx", "pnpm", "yarn", "bun", "uv", "uvx", "poetry", "make", "cargo", "just", "deno",
];
/// The manifests a runner reads from its working directory, the files it prefers over
/// them (`GNUmakefile`, `makefile`, `Justfile`, `.justfile`) and its configuration there
/// (`.npmrc`, `.yarnrc.yml`, `bunfig.toml`, `.cargo/config.toml`), which can redirect
/// what runs.
const MANIFESTS: &[&str] = &[
    "package.json",
    "pyproject.toml",
    "GNUmakefile",
    "makefile",
    "Makefile",
    "justfile",
    "Justfile",
    ".justfile",
    "Cargo.toml",
    "deno.json",
    "deno.jsonc",
    ".npmrc",
    ".yarnrc.yml",
    "bunfig.toml",
    ".cargo/config.toml",
];
/// Python options without a value, which may precede `m` in one token (`-um`, `-Im`).
const PYTHON_FLAGS: &str = "bBdEhiIOqsSuvx";
/// Characters a shell splits words on (besides whitespace), and quotes.
const SHELL_SPLIT: &[char] = &[';', '&', '|', '(', ')', '<', '>', '\'', '"', '`'];

/// What the files the command line `argv` runs inside the (canonical) `root` are, with
/// relative paths taken against `base` (the server's working directory). Candidates:
///
/// - every word (the command included), and every token of it split on whitespace and
///   shell metacharacters (`;&|()<>`, quotes, backticks), which covers `sh -c "cd srv &&
///   node main.js"`;
/// - the value after a token's first `=` (`--require=./x.js`), and what follows a
///   one-letter flag (`-r./pre.js`);
/// - for `-m <mod>` (also `-m<mod>`, and option letters ending in `m` such as `-um <mod>`),
///   `<mod>` with `.` as `/`, as a directory, `<mod>.py` and `<mod>.pyc`, and the
///   `__init__.py` (`.pyc`) of every parent package;
/// - when any token is a runner (npm, npx, pnpm, yarn, bun, uv, uvx, poetry, make, cargo,
///   just, deno; by basename, so `/usr/bin/env npm` and `sh -c "npm run x"` count), the
///   [`MANIFESTS`] in the working directory that exist.
///
/// A candidate inside the root (by name or once links are resolved) adds an entry: the
/// blake3 of a regular file; the recursive content of a directory (names, link targets,
/// file hashes; `.git`, `node_modules`, `target`, `.venv`, `__pycache__` and tautly's
/// `.tautly/tmp` skipped, links never followed); the resolved path of a link leading out
/// of the root; or `missing`. Bounds: [`MAX_FILE`] per file, [`MAX_TOTAL`] and
/// [`MAX_FILES`] entries per key. Content that cannot be hashed (over a bound, unreadable, a FIFO)
/// gets a value no other call produces and clears `rememberable`, so the server is asked
/// about at every start. Candidates outside the root add nothing.
pub(crate) fn files_named(
    root: &Path,
    base: &Path,
    argv: &[&String],
    rememberable: &mut bool,
) -> Vec<serde_json::Value> {
    let mut budget = Budget {
        bytes: MAX_TOTAL,
        files: MAX_FILES,
    };
    let mut seen = std::collections::BTreeSet::new();
    let mut entries = Vec::new();
    for (word, candidate) in candidates(argv, base) {
        if !seen.insert(candidate.clone()) {
            continue;
        }
        let path = base.join(&candidate);
        let real = path.canonicalize().ok();
        let inside =
            lexical(&path).starts_with(root) || real.as_ref().is_some_and(|r| r.starts_with(root));
        if !inside {
            continue;
        }
        let state = match real {
            None => serde_json::json!("missing"),
            Some(r) if !r.starts_with(root) => {
                serde_json::json!({ "target": r.to_string_lossy() })
            }
            Some(r) => {
                let hashed = if r.is_dir() {
                    hash_dir(root, &r, &mut budget).map(|h| ("tree", h))
                } else {
                    hash_file(&r, &mut budget).map(|h| ("blake3", h))
                };
                match hashed {
                    Some((kind, h)) => serde_json::json!({ kind: h }),
                    None => {
                        *rememberable = false;
                        serde_json::json!({ "unknown": nonce() })
                    }
                }
            }
        };
        entries.push(serde_json::json!({ "word": word, "path": candidate, "state": state }));
    }
    entries
}

/// `(word index, candidate path)` for every candidate of `argv` (see [`files_named`]).
fn candidates(argv: &[&String], base: &Path) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut runner = false;
    for (i, word) in argv.iter().enumerate() {
        let tokens = std::iter::once(word.as_str()).chain(
            word.split(|c: char| c.is_whitespace() || SHELL_SPLIT.contains(&c))
                .filter(|t| !t.is_empty() && *t != word.as_str()),
        );
        for token in tokens {
            runner |= Path::new(token)
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| RUNNERS.contains(&n));
            out.push((i, token.to_string()));
            if let Some((_, value)) = token.split_once('=') {
                out.push((i, value.to_string()));
            }
            if let Some(rest) = short_flag_value(token) {
                out.push((i, rest.to_string()));
            }
            match python_module_flag(token) {
                Some(Some(module)) => module_paths(i, module, &mut out),
                Some(None) => {
                    if let Some(module) = argv.get(i + 1) {
                        module_paths(i + 1, module, &mut out);
                    }
                }
                None => {}
            }
        }
    }
    if runner {
        for m in MANIFESTS {
            if base.join(m).symlink_metadata().is_ok() {
                out.push((0, (*m).to_string()));
            }
        }
    }
    out
}

/// What follows a one-letter flag (`-r./pre.js` → `./pre.js`); none for `--long` flags
/// and bare letters.
fn short_flag_value(token: &str) -> Option<&str> {
    let rest = token.strip_prefix('-')?;
    let mut chars = rest.chars();
    let flag = chars.next()?;
    let value = chars.as_str();
    (flag.is_ascii_alphabetic() && !value.is_empty()).then_some(value)
}

/// Python's `-m` in a token: `Some(None)` when the module is the next word (`-m`, `-um`,
/// `-Im`), `Some(Some(mod))` when attached (`-mtool`, `-umtool`); none otherwise.
fn python_module_flag(token: &str) -> Option<Option<&str>> {
    let letters = token.strip_prefix('-')?;
    if letters.starts_with('-') {
        return None;
    }
    let flags = letters.trim_start_matches(|c| PYTHON_FLAGS.contains(c));
    let module = flags.strip_prefix('m')?;
    Some((!module.is_empty()).then_some(module))
}

/// `module` (`a.b`) as the directory `a/b` and the files `a/b.py` and `a/b.pyc`, with the
/// `__init__.py` (and `.pyc`) of each parent package (`a/__init__.py`), which runs first.
fn module_paths(word: usize, module: &str, out: &mut Vec<(usize, String)>) {
    let path = module.replace('.', "/");
    if path.is_empty() {
        return;
    }
    out.push((word, format!("{path}.py")));
    // A sourceless module: bytecode imported where no source is.
    out.push((word, format!("{path}.pyc")));
    let parts: Vec<&str> = path.split('/').collect();
    for n in 1..parts.len() {
        let parent = parts[..n].join("/");
        out.push((word, format!("{parent}/__init__.py")));
        out.push((word, format!("{parent}/__init__.pyc")));
    }
    out.push((word, path));
}

/// What is left of the per-key bounds.
struct Budget {
    bytes: u64,
    files: u32,
}

/// The blake3 hex of the regular file at `path`, within the bounds (which it is taken
/// from). Opened without blocking, so a FIFO is refused rather than waited on.
fn hash_file(path: &Path, budget: &mut Budget) -> Option<String> {
    budget.files = budget.files.checked_sub(1)?;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
        .ok()?;
    let meta = file.metadata().ok()?;
    let cap = MAX_FILE.min(budget.bytes);
    if !meta.is_file() || meta.len() > cap {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(cap + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 > cap {
        return None;
    }
    budget.bytes -= bytes.len() as u64;
    Some(blake3::hash(&bytes).to_hex().to_string())
}

/// The blake3 hex of the directory `dir`'s content: every entry's path relative to it,
/// in name order, with a file's hash or a link's target (never followed), each field
/// length-prefixed so no two trees encode alike (names and targets may hold newlines);
/// an empty directory adds nothing. [`SKIPPED_DIRS`] and `<root>/.tautly/tmp` are left
/// out. Walked without recursion, so depth costs no stack; a directory with more entries
/// than the budget left is refused before its names are all collected. None when a bound
/// is exceeded or something cannot be read or hashed.
fn hash_dir(root: &Path, dir: &Path, budget: &mut Budget) -> Option<String> {
    use std::os::unix::ffi::OsStrExt;
    let scratch = root.join(".tautly").join("tmp");
    let mut hasher = blake3::Hasher::new();
    let field = |hasher: &mut blake3::Hasher, bytes: &[u8]| {
        hasher.update(&(bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    };
    // Directories still to read, deepest-first in name order: (absolute, relative).
    let mut pending: Vec<(PathBuf, PathBuf)> = vec![(dir.to_path_buf(), PathBuf::new())];
    while let Some((dir, rel)) = pending.pop() {
        let mut names = Vec::new();
        for entry in std::fs::read_dir(&dir).ok()? {
            if names.len() as u64 >= u64::from(budget.files) {
                return None;
            }
            names.push(entry.ok()?.file_name());
        }
        names.sort();
        let mut subdirs = Vec::new();
        for name in names {
            let path = dir.join(&name);
            let kind = path.symlink_metadata().ok()?.file_type();
            let entry = rel.join(&name);
            if kind.is_dir() {
                if SKIPPED_DIRS.iter().any(|s| name == *s) || path == scratch {
                    continue;
                }
                budget.files = budget.files.checked_sub(1)?;
                subdirs.push((path, entry));
                continue;
            }
            let (tag, value): (&[u8], Vec<u8>) = if kind.is_symlink() {
                let target = std::fs::read_link(&path).ok()?;
                (b"link", target.as_os_str().as_bytes().to_vec())
            } else {
                (b"file", hash_file(&path, budget)?.into_bytes())
            };
            field(&mut hasher, entry.as_os_str().as_bytes());
            field(&mut hasher, tag);
            field(&mut hasher, &value);
        }
        // Popped in name order: the first subdirectory last.
        pending.extend(subdirs.into_iter().rev());
    }
    Some(hasher.finalize().to_hex().to_string())
}

/// `path` with `.` and `..` components resolved by name (no links followed).
fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// A value no other call returns: keys an approval that must never be remembered.
fn nonce() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let seed = format!("{}:{t}:{n}", std::process::id());
    blake3::hash(seed.as_bytes()).to_hex().to_string()
}
