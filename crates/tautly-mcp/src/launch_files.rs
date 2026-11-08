//! The repository files a stdio server's command line runs, for its approval key (spec
//! 7.11.10, review B-I1/N1): the code a repository server runs is usually in the
//! repository (`node scripts/mcp.js`, `python -m pkg`, `npm run mcp`), and a later
//! revision can rewrite it without touching the command line.

use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use tautly_sandbox::lexical;

/// Largest file whose content a key covers.
const MAX_FILE: u64 = 1024 * 1024;
/// Most bytes of files read for one key.
const MAX_TOTAL: u64 = 8 * 1024 * 1024;
/// Most entries read for one key: files, and the directories walked.
const MAX_FILES: u32 = 4000;
/// Most bytecode headers read for one key (timestamp bytecode costs only its header).
const MAX_HEADERS: u32 = MAX_FILES;
/// Most candidate paths looked up for one key.
const MAX_CANDIDATES: usize = 4 * MAX_FILES as usize;
/// Deepest `-m` module keyed by content (`a.b.c` is 3); far beyond any real package.
const MAX_MODULE_PARTS: usize = 64;
/// Longest `-m` module keyed by content: a longer one is no path Python can open.
const MAX_MODULE_BYTES: usize = 4096;
/// Directory names never walked: version control, dependencies, build output.
const SKIPPED_DIRS: &[&str] = &[".git", "node_modules", "target", ".venv"];
/// Python's bytecode cache directory: walked for hash-based `.pyc` files only.
const PYCACHE: &str = "__pycache__";
/// The variable that moves a Python server's bytecode cache out of the repository
/// (`tautly-core` sets it); a command line that can drop it is never remembered.
const PYCACHE_VAR: &str = "PYTHONPYCACHEPREFIX";
/// Python's `-X pycache_prefix=…`, which overrides the variable.
const PYCACHE_OPTION: &str = "pycache_prefix";
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
/// the server is asked about at every start. So does a command line the candidate bounds
/// do not cover ([`MAX_CANDIDATES`], [`MAX_MODULE_PARTS`], [`MAX_MODULE_BYTES`]): its
/// files are not all keyed. Candidates outside the root add nothing.
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
    let (candidates, complete) = candidates(words, base);
    *rememberable &= complete;
    for (word, candidate) in candidates {
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

/// Whether the `words` (the command line, then the server's variable values, which a
/// shell string can splice in) can run their Python without the `PYTHONPYCACHEPREFIX`
/// the engine sets, or with another one (reviews S3-I1, S4-I1), so that a repository
/// `__pycache__` is read after all: an option cluster with `E` or `I` (`-E`, `-Em`,
/// `-sE`, `-I`; whatever the command, so `grep -E` in a shell string counts too), `env`
/// with `-i`, `-u`, a lone `-` or a prefix of `--ignore-environment` among the options
/// right after it (across the values of `-P`, `-C` and `--chdir`), a shell `exec -c`,
/// `unset`, any token naming the variable (`env -u PYTHONPYCACHEPREFIX`,
/// `PYTHONPYCACHEPREFIX=`, `PYTHONPYCACHEPREFIX=./x`), or any token containing
/// `pycache_prefix` (`-X pycache_prefix=/`, `-Xpycache_prefix=/`). Quotes are removed
/// and tokens are split as for [`files_named`]: shell strings are read lexically, as
/// well as that allows. Such a server is asked about at every start (its timestamp
/// bytecode could then match too).
pub(crate) fn can_drop_pycache_prefix(words: &[&String]) -> bool {
    // `env` or `exec` while its own options may still follow (they end at the first
    // token that is not one, as both commands parse them).
    let mut after: Option<&str> = None;
    // The next token is the value of an `env` option (`-P dir`, `-C dir`, `--chdir dir`),
    // not the command.
    let mut value = false;
    for word in words {
        // Quotes are removed, not split on: `-"E"` is `-E` to the shell.
        let bare: String = word
            .chars()
            .filter(|c| !matches!(c, '\'' | '"' | '`'))
            .collect();
        for token in tokens(&bare) {
            if token.contains(PYCACHE_VAR) || token.contains(PYCACHE_OPTION) || token == "unset" {
                return true;
            }
            if std::mem::take(&mut value) {
                continue;
            }
            match token.strip_prefix('-') {
                Some(cluster) if !cluster.starts_with('-') => {
                    let flags: String = cluster
                        .chars()
                        .take_while(char::is_ascii_alphabetic)
                        .collect();
                    if flags.contains(['E', 'I'])
                        || (after == Some("env")
                            && (flags.contains(['i', 'u']) || cluster.is_empty()))
                        || (after == Some("exec") && flags.contains('c'))
                    {
                        return true;
                    }
                    value = after == Some("env") && matches!(cluster, "P" | "C");
                }
                Some(long) => {
                    if after == Some("env") {
                        if long.len() > 1 && "-ignore-environment".starts_with(long) {
                            return true;
                        }
                        value = long == "-chdir";
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

/// `(word index, candidate path)` for every candidate of `words` (see [`files_named`]),
/// and whether that is all of them: false when the bounds cut them short.
fn candidates(words: &[&String], base: &Path) -> (Vec<(usize, String)>, bool) {
    let mut out = Vec::new();
    let mut runner = false;
    let mut complete = true;
    for (i, word) in words.iter().enumerate() {
        if out.len() > MAX_CANDIDATES {
            complete = false;
            break;
        }
        let tokens = tokens(word);
        for (j, token) in tokens.iter().enumerate() {
            // Checked per token too: one `sh -c` word can hold any number of them.
            if out.len() > MAX_CANDIDATES {
                complete = false;
                break;
            }
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
            let module = match python_module_flag(token) {
                Some(Some(module)) => Some((i, module)),
                // The module is the next token of a split word, else the next word.
                Some(None) => match tokens.get(j + 1) {
                    Some(module) if j > 0 => Some((i, *module)),
                    _ => words.get(i + 1).map(|m| (i + 1, m.as_str())),
                },
                None => None,
            };
            if let Some((w, module)) = module {
                complete &= module_paths(w, module, &mut out);
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
    if out.len() > MAX_CANDIDATES {
        out.truncate(MAX_CANDIDATES);
        complete = false;
    }
    (out, complete)
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
/// bytecode (`a/__pycache__` for both; `__pycache__` for a top-level module). False,
/// adding nothing, for a module over [`MAX_MODULE_PARTS`] or [`MAX_MODULE_BYTES`]: each
/// parent package costs a candidate as long as the module, so its candidates would grow
/// with the square of its length.
fn module_paths(word: usize, module: &str, out: &mut Vec<(usize, String)>) -> bool {
    if module.len() > MAX_MODULE_BYTES {
        return false;
    }
    let path = module.replace('.', "/");
    // Counted as path parts: `a/b` is as deep as `a.b`.
    if path.split('/').count() > MAX_MODULE_PARTS {
        return false;
    }
    if path.is_empty() {
        return true;
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
        None => return true,
    };
    out.push((word, cache));
    out.push((word, path));
    true
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
                // A linked `__pycache__` is read through by Python (review S4-M2): the
                // directory it resolves to is walked as a cache too, wherever it lies
                // (a cache walk lists only `.pyc` files, so a loop cannot form).
                if is_cache(&name) && path.metadata().is_ok_and(|m| m.is_dir()) {
                    budget.files = budget.files.checked_sub(1)?;
                    subdirs.push((path.clone(), entry.clone(), true));
                }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Review I1: a `-m` module written with `/` (`a/a/a…`, one dot-part) still has a
    /// path part per `/`, and one `sh -c` word can hold many of them: the parts are
    /// counted after the `.`-to-`/` change and the candidate cap holds inside a word, so
    /// a 246 KB command line yields a bounded candidate list (count and bytes), and is
    /// not complete (never remembered).
    #[test]
    fn slash_modules_and_one_long_word_are_bounded() {
        let module = format!("{}a", "a/".repeat(2047));
        let word = format!("python3 -m {module} > ").repeat(60);
        let short = format!("python3 -m {module}");
        let base = Path::new("/nonexistent-tautly-test-root");
        for w in [word, short] {
            let words = [&w];
            let (out, complete) = candidates(&words, base);
            assert!(!complete);
            assert!(out.len() <= MAX_CANDIDATES, "{} candidates", out.len());
            let bytes: usize = out.iter().map(|(_, c)| c.len()).sum();
            assert!(bytes < 16 * 1024 * 1024, "{bytes} bytes of candidates");
        }
        // A real package written with `/` still counts as complete.
        let ok = "python3 -m a/b/c".to_string();
        assert!(candidates(&[&ok], base).1);
    }
}
