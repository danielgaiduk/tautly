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
/// Most bytecode headers read for one key (timestamp bytecode costs only its header).
const MAX_HEADERS: u32 = MAX_FILES;
/// Directory names never walked: version control, dependencies, build output.
const SKIPPED_DIRS: &[&str] = &[".git", "node_modules", "target", ".venv"];
/// Python's bytecode cache directory: walked for hash-based `.pyc` files only.
const PYCACHE: &str = "__pycache__";
/// The variable that moves a Python server's bytecode cache out of the repository
/// (`tautly-core` sets it); a command line that can drop it is never remembered.
const PYCACHE_VAR: &str = "PYTHONPYCACHEPREFIX";
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
const PYTHON_FLAGS: &str = "bBdEhiIOPqRsSuvx";
/// Characters a shell splits words on (besides whitespace), and quotes.
const SHELL_SPLIT: &[char] = &[';', '&', '|', '(', ')', '<', '>', '\'', '"', '`'];

/// What the files the `words` run inside the (canonical) `root` are, with relative paths
/// taken against `base` (the server's working directory). The words are the command
/// line's, then the values of the server's variables (`NODE_OPTIONS=--require ./x.js`,
/// `PYTHONPATH=.`), which name files the same way. Candidates:
///
/// - every word, and every token of it split on whitespace and shell metacharacters
///   (`;&|()<>`, quotes, backticks), which covers `sh -c "cd srv && node main.js"`;
/// - the value after a token's first `=` (`--require=./x.js`), and what follows a
///   one-letter flag (`-r./pre.js`);
/// - for `-m <mod>` (also `-m<mod>`, and option letters ending in `m` such as `-um <mod>`;
///   inside a split word, `<mod>` is the next token of that word), `<mod>` with `.` as
///   `/`, as a directory, `<mod>.py` and `<mod>.pyc`, the `__init__.py` (`.pyc`) of every
///   parent package, and the `__pycache__` next to each of those files;
/// - when any token is a runner (npm, npx, pnpm, yarn, bun, uv, uvx, poetry, make, cargo,
///   just, deno; by basename, so `/usr/bin/env npm` and `sh -c "npm run x"` count), the
///   [`MANIFESTS`] in the working directory that exist.
///
/// A candidate inside the root (by name or once links are resolved) adds an entry: the
/// blake3 of a regular file; the recursive content of a directory (names, link targets,
/// file hashes; `.git`, `node_modules`, `target`, `.venv` and tautly's `.tautly/tmp`
/// skipped, links never followed; in a `__pycache__` only hash-based `.pyc` files, see
/// [`hash_dir`]); the resolved path of a link leading out of the root; or `missing`.
/// Bounds: [`MAX_FILE`] per file, [`MAX_TOTAL`], [`MAX_FILES`] entries and
/// [`MAX_HEADERS`] bytecode headers per key. Content that cannot be hashed (over a bound,
/// unreadable, a FIFO) gets a value no other call produces and clears `rememberable`, so
/// the server is asked about at every start. Candidates outside the root add nothing.
pub(crate) fn files_named(
    root: &Path,
    base: &Path,
    words: &[&String],
    rememberable: &mut bool,
) -> Vec<serde_json::Value> {
    let mut budget = Budget {
        bytes: MAX_TOTAL,
        files: MAX_FILES,
        headers: MAX_HEADERS,
    };
    let mut seen = std::collections::BTreeSet::new();
    let mut entries = Vec::new();
    for (word, candidate) in candidates(words, base) {
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
            // Python makes a `__pycache__` when it first writes its cache: a missing one
            // keys like an empty one, so that does not ask again.
            None if Path::new(&candidate)
                .file_name()
                .is_some_and(|n| n == PYCACHE) =>
            {
                serde_json::json!({ "tree": blake3::Hasher::new().finalize().to_hex().to_string() })
            }
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

/// Whether the command line `argv` can run its Python without the `PYTHONPYCACHEPREFIX`
/// the engine sets (review S3-I1), so that a repository `__pycache__` is read after all:
/// an option cluster with `E` or `I` (`-E`, `-Em`, `-sE`, `-I`; whatever the command, so
/// `grep -E` in a shell string counts too), `env` with `-i`/`-u` (in a cluster) or
/// `--ignore-environment` among the options right after it, a shell `exec -c`, `unset`,
/// or any token naming the variable (`env -u PYTHONPYCACHEPREFIX`, `PYTHONPYCACHEPREFIX=`,
/// `PYTHONPYCACHEPREFIX=./x`). Tokens are split as for [`files_named`]. Such a server is
/// asked about at every start (its timestamp bytecode could then match too).
pub(crate) fn can_drop_pycache_prefix(argv: &[&String]) -> bool {
    // `env` or `exec` while its own options may still follow (they end at the first
    // token that is not one, as both commands parse them).
    let mut after: Option<&str> = None;
    for word in argv {
        for token in tokens(word) {
            if token.contains(PYCACHE_VAR) || token == "unset" {
                return true;
            }
            match token.strip_prefix('-') {
                Some(cluster) if !cluster.starts_with('-') => {
                    let flags: String = cluster
                        .chars()
                        .take_while(char::is_ascii_alphabetic)
                        .collect();
                    if flags.contains(['E', 'I'])
                        || (after == Some("env") && flags.contains(['i', 'u']))
                        || (after == Some("exec") && flags.contains('c'))
                    {
                        return true;
                    }
                }
                Some(long) => {
                    if after == Some("env") && long == "-ignore-environment" {
                        return true;
                    }
                }
                None => {
                    after = match Path::new(token).file_name().and_then(|n| n.to_str()) {
                        Some("env") => Some("env"),
                        Some("exec") => Some("exec"),
                        _ => None,
                    };
                }
            }
        }
    }
    false
}

/// `word`, then its tokens split on whitespace and shell metacharacters (those that
/// differ from the whole word).
fn tokens(word: &str) -> Vec<&str> {
    std::iter::once(word)
        .chain(
            word.split(|c: char| c.is_whitespace() || SHELL_SPLIT.contains(&c))
                .filter(|t| !t.is_empty() && *t != word),
        )
        .collect()
}

/// `(word index, candidate path)` for every candidate of `words` (see [`files_named`]).
fn candidates(words: &[&String], base: &Path) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut runner = false;
    for (i, word) in words.iter().enumerate() {
        let tokens = tokens(word);
        for (j, token) in tokens.iter().enumerate() {
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
                // The module is the next token of a split word, else the next word.
                Some(None) => match tokens.get(j + 1) {
                    Some(module) if j > 0 => module_paths(i, module, &mut out),
                    _ => {
                        if let Some(module) = words.get(i + 1) {
                            module_paths(i + 1, module, &mut out);
                        }
                    }
                },
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
/// `__init__.py` (and `.pyc`) of each parent package (`a/__init__.py`), which runs first,
/// and the `__pycache__` next to each of those files, where Python looks for their
/// bytecode (`a/__pycache__` for both; `__pycache__` for a top-level module).
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
        out.push((word, format!("{parent}/{PYCACHE}")));
    }
    let cache = match parts.split_last() {
        Some((_, [])) => PYCACHE.to_string(),
        Some((_, dirs)) => format!("{}/{PYCACHE}", dirs.join("/")),
        None => return,
    };
    out.push((word, cache));
    out.push((word, path));
}

/// What is left of the per-key bounds.
struct Budget {
    bytes: u64,
    files: u32,
    headers: u32,
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

/// Whether the `.pyc` at `path` is hash-based (review S3-I1): its flags word (bytes
/// 4..8, little-endian) is non-zero. Python's import system writes only timestamp
/// bytecode (flags 0), so keying hash-based files alone keeps a key stable while Python
/// fills its own cache; a hash-based file comes from a tool, and an *unchecked* one runs
/// whatever the source says. The header read costs one of [`MAX_HEADERS`]. None when the
/// file cannot be read as a regular file of at least a header.
fn pyc_is_hash_based(path: &Path, budget: &mut Budget) -> Option<bool> {
    budget.headers = budget.headers.checked_sub(1)?;
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
        .ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut header = [0u8; 16];
    file.read_exact(&mut header).ok()?;
    let flags = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
    Some(flags != 0)
}

/// The blake3 hex of the directory `dir`'s content: every entry's path relative to it,
/// in name order, with a file's hash or a link's target (never followed), each field
/// length-prefixed so no two trees encode alike (names and targets may hold newlines);
/// an empty directory adds nothing. [`SKIPPED_DIRS`] and `<root>/.tautly/tmp` are left
/// out. A `__pycache__` (walked, or `dir` itself) contributes only its hash-based `.pyc`
/// files (through a link too, as Python reads them; see [`pyc_is_hash_based`]), tagged
/// `pyc`: the timestamp bytecode Python writes there, and anything else, is not read by
/// an import and adds nothing. Walked without recursion, so depth costs no stack; a
/// directory with more entries than the budget left is refused before its names are all
/// collected. None when a bound is exceeded or something cannot be read or hashed.
fn hash_dir(root: &Path, dir: &Path, budget: &mut Budget) -> Option<String> {
    use std::os::unix::ffi::OsStrExt;
    let scratch = root.join(".tautly").join("tmp");
    let mut hasher = blake3::Hasher::new();
    let field = |hasher: &mut blake3::Hasher, bytes: &[u8]| {
        hasher.update(&(bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    };
    let is_cache = |name: &std::ffi::OsStr| name == PYCACHE;
    // Directories still to read, deepest-first in name order: (absolute, relative,
    // whether it is a bytecode cache).
    let top_is_cache = dir.file_name().is_some_and(is_cache);
    let mut pending: Vec<(PathBuf, PathBuf, bool)> =
        vec![(dir.to_path_buf(), PathBuf::new(), top_is_cache)];
    while let Some((dir, rel, cache)) = pending.pop() {
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
            let entry = rel.join(&name);
            if cache {
                // Not bytecode, or not a file once links are followed (a dangling link,
                // which an import passes over): nothing Python would run.
                if !name.as_bytes().ends_with(b".pyc")
                    || !path.metadata().is_ok_and(|m| m.is_file())
                {
                    continue;
                }
                if !pyc_is_hash_based(&path, budget)? {
                    continue;
                }
                field(&mut hasher, entry.as_os_str().as_bytes());
                field(&mut hasher, b"pyc");
                field(&mut hasher, hash_file(&path, budget)?.as_bytes());
                continue;
            }
            let kind = path.symlink_metadata().ok()?.file_type();
            if kind.is_dir() {
                if SKIPPED_DIRS.iter().any(|s| name == *s) || path == scratch {
                    continue;
                }
                budget.files = budget.files.checked_sub(1)?;
                subdirs.push((path, entry, is_cache(&name)));
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
pub(crate) fn nonce() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let seed = format!("{}:{t}:{n}", std::process::id());
    blake3::hash(seed.as_bytes()).to_hex().to_string()
}
