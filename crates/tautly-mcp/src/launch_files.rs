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
/// Most files read for one key (directories walked included).
const MAX_FILES: u32 = 4000;
/// Directory names never walked: version control, dependencies, build output, caches.
const SKIPPED_DIRS: &[&str] = &[".git", "node_modules", "target", ".venv", "__pycache__"];
/// Commands that run what a manifest in their working directory says.
const RUNNERS: &[&str] = &[
    "npm", "npx", "pnpm", "yarn", "bun", "uv", "uvx", "poetry", "make", "cargo", "just", "deno",
];
/// The manifests a runner reads from its working directory.
const MANIFESTS: &[&str] = &[
    "package.json",
    "pyproject.toml",
    "Makefile",
    "justfile",
    "Cargo.toml",
    "deno.json",
];
/// Characters a shell splits words on (besides whitespace), and quotes.
const SHELL_SPLIT: &[char] = &[';', '&', '|', '(', ')', '<', '>', '\'', '"', '`'];

/// What the files the command line `argv` runs inside the (canonical) `root` are, with
/// relative paths taken against `base` (the server's working directory). Candidates:
///
/// - every word (the command included), and every token of it split on whitespace and
///   shell metacharacters (`;&|()<>`, quotes), which covers `sh -c "cd srv && node
///   main.js"`;
/// - the value after a token's first `=` (`--require=./x.js`), and what follows a
///   one-letter flag (`-r./pre.js`);
/// - for `-m <mod>` (and `-m<mod>`), `<mod>` with `.` as `/`, as a directory and as
///   `<mod>.py`;
/// - when the command is a runner (npm, npx, pnpm, yarn, bun, uv, uvx, poetry, make,
///   cargo, just, deno), the manifests in the working directory (package.json,
///   pyproject.toml, Makefile, justfile, Cargo.toml, deno.json).
///
/// A candidate inside the root (by name or once links are resolved) adds an entry: the
/// blake3 of a regular file; the recursive content of a directory (names, link targets,
/// file hashes; `.git`, `node_modules`, `target`, `.venv`, `__pycache__` and tautly's
/// `.tautly/tmp` skipped, links never followed); the resolved path of a link leading out
/// of the root; or `missing`. Bounds: [`MAX_FILE`] per file, [`MAX_TOTAL`] and
/// [`MAX_FILES`] per key. Content that cannot be hashed (over a bound, unreadable, a FIFO)
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
    for (i, word) in argv.iter().enumerate() {
        let tokens = std::iter::once(word.as_str()).chain(
            word.split(|c: char| c.is_whitespace() || SHELL_SPLIT.contains(&c))
                .filter(|t| !t.is_empty() && *t != word.as_str()),
        );
        for token in tokens {
            out.push((i, token.to_string()));
            if let Some((_, value)) = token.split_once('=') {
                out.push((i, value.to_string()));
            }
            if let Some(rest) = short_flag_value(token) {
                out.push((i, rest.to_string()));
                if token.starts_with("-m") {
                    module_paths(i, rest, &mut out);
                }
            }
        }
        if *word == "-m"
            && let Some(module) = argv.get(i + 1)
        {
            module_paths(i + 1, module, &mut out);
        }
    }
    let command = argv
        .first()
        .and_then(|c| Path::new(c.as_str()).file_name())
        .and_then(|n| n.to_str());
    if command.is_some_and(|c| RUNNERS.contains(&c)) {
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

/// `module` (`a.b`) as the directory `a/b` and the file `a/b.py`.
fn module_paths(word: usize, module: &str, out: &mut Vec<(usize, String)>) {
    let path = module.replace('.', "/");
    if path.is_empty() {
        return;
    }
    out.push((word, format!("{path}.py")));
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
/// in name order, with a file's hash, a link's target (never followed) or a nested
/// directory's entries (an empty directory adds nothing). [`SKIPPED_DIRS`] and
/// `<root>/.tautly/tmp` are left out. None when a bound is exceeded or something cannot be
/// read or hashed.
fn hash_dir(root: &Path, dir: &Path, budget: &mut Budget) -> Option<String> {
    let mut hasher = blake3::Hasher::new();
    walk(root, dir, Path::new(""), &mut hasher, budget)?;
    Some(hasher.finalize().to_hex().to_string())
}

fn walk(
    root: &Path,
    dir: &Path,
    rel: &Path,
    hasher: &mut blake3::Hasher,
    budget: &mut Budget,
) -> Option<()> {
    let scratch = root.join(".tautly").join("tmp");
    let mut names: Vec<_> = std::fs::read_dir(dir)
        .ok()?
        .map(|e| e.map(|e| e.file_name()))
        .collect::<Result<_, _>>()
        .ok()?;
    names.sort();
    for name in names {
        let path = dir.join(&name);
        let meta = path.symlink_metadata().ok()?;
        let kind = meta.file_type();
        if kind.is_dir() && (SKIPPED_DIRS.iter().any(|s| name == *s) || path == scratch) {
            continue;
        }
        let entry = rel.join(&name);
        let line = if kind.is_symlink() {
            format!(
                "link\0{}",
                std::fs::read_link(&path).ok()?.to_string_lossy()
            )
        } else if kind.is_dir() {
            budget.files = budget.files.checked_sub(1)?;
            walk(root, &path, &entry, hasher, budget)?;
            continue;
        } else {
            format!("file\0{}", hash_file(&path, budget)?)
        };
        hasher.update(format!("{}\0{line}\n", entry.to_string_lossy()).as_bytes());
    }
    Some(())
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
