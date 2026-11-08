use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use tokio::process::{Child, Command};
use url::Url;

use crate::effect::{FileMeta, HttpResponse, Sandbox, SandboxError, SpawnSpec, SpillWriter};
use crate::protected::{self, Resolved, is_protected};
use crate::spill::SPILL_DIR;
use crate::walk;

const MAX_BODY: usize = 10 * 1024 * 1024;

/// Sandbox confining filesystem access to a canonical root directory.
#[derive(Debug, Clone)]
pub struct LocalSandbox {
    root: PathBuf,
    client: reqwest::Client,
    /// `.tautly/tmp` (with its `.gitignore`) and the spill directory are known to exist.
    spill_ready: Arc<AtomicBool>,
}

impl LocalSandbox {
    pub fn new(root: PathBuf) -> Result<Self, SandboxError> {
        let client = crate::http_client_builder()
            .timeout(std::time::Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| SandboxError::Network(e.to_string()))?;
        Ok(Self {
            root: root.canonicalize()?,
            client,
            spill_ready: Arc::new(AtomicBool::new(false)),
        })
    }
}

impl LocalSandbox {
    /// Make sure `.tautly/tmp/.gitignore` ignores everything: checked at every spill, since
    /// a checkout can replace it mid-session (git treats an ignored file as expendable).
    ///
    /// A `.gitignore` that says anything else (empty, `!*`, other rules) would un-ignore the
    /// spill files, which hold full command output, so `git add -A` would take them: it is
    /// rewritten to `*`. One whose rules (lines that are not blank or `#` comments) are
    /// all exactly `*` is left as it is, so a tracked one never shows as modified. One that
    /// is not a regular file (a symbolic link, which would be written through, or a FIFO)
    /// refuses the spill.
    async fn ensure_spill_ignore(&self) -> Result<(), SandboxError> {
        const MAX_IGNORE: u64 = 4096;
        let ignore = Path::new(".tautly/tmp/.gitignore");
        let path = self.resolve(ignore)?;
        self.guard_protected_target(ignore, &path)?;
        let rewrite = match tokio::fs::symlink_metadata(self.root.join(ignore)).await {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
            Err(e) => return Err(e.into()),
            Ok(m) if m.is_file() => {
                m.len() > MAX_IGNORE
                    || !ignores_everything(&tokio::fs::read(self.root.join(ignore)).await?)
            }
            Ok(_) => {
                return Err(SandboxError::Denied(
                    ".tautly/tmp/.gitignore is not a regular file: output is not spilled".into(),
                ));
            }
        };
        if rewrite {
            self.write_atomic(ignore, b"*", None).await?;
        }
        Ok(())
    }

    /// Create the spill directory. Done once, and again only if it disappears.
    async fn prepare_spill_dir(&self) -> Result<(), SandboxError> {
        let dir = Path::new(SPILL_DIR);
        let path = self.resolve(dir)?;
        self.guard_protected_target(dir, &path)?;
        tokio::fs::create_dir_all(path).await?;
        self.spill_ready.store(true, Ordering::Release);
        Ok(())
    }

    /// Refuse to modify a protected path (`.git/`, `.tautly/`, ...) reached through a
    /// symlink: a repository can commit `NOTES.md -> .git/config` or `.tautly/tmp ->
    /// ../.git/hooks`. Naming the protected path itself is the permission
    /// policy's decision (it asks); following a link into one never is. `p` is
    /// the path as the caller requested it; `resolved` is `self.resolve(p)`.
    fn guard_protected_target(&self, p: &Path, resolved: &Path) -> Result<(), SandboxError> {
        let Ok(real) = resolved.strip_prefix(&self.root) else {
            return Ok(());
        };
        if !is_protected(real) {
            return Ok(());
        }
        // Protected-path matching is case-insensitive, so this comparison is too.
        let lower = |q: &Path| q.to_string_lossy().to_lowercase();
        let named = protected::normalize(&self.root, &p.to_string_lossy())
            .is_some_and(|rel| lower(&rel) == lower(real));
        if named {
            Ok(())
        } else {
            Err(SandboxError::Denied(format!(
                "{} resolves through a symlink to protected path {}",
                p.display(),
                real.display()
            )))
        }
    }

    /// Spawn `spec` with the given stdin; stdout and stderr are piped.
    fn spawn_with(&self, spec: SpawnSpec, stdin: Stdio) -> Result<Child, SandboxError> {
        let (prog, args) = spec
            .argv
            .split_first()
            .ok_or_else(|| SandboxError::Denied("empty argv".into()))?;
        let cwd = self.resolve(&spec.cwd)?;
        // A bare name is looked up here, not by the OS: on the child's `PATH`, absolute
        // entries outside the root only (`which_outside`), so a relative or empty entry
        // cannot run a program the repository ships. It keeps its bare name as arg0.
        let mut cmd = if prog.contains('/') {
            Command::new(prog)
        } else {
            let path = match spec.env.iter().rev().find(|(k, _)| k == "PATH") {
                Some((_, v)) => Some(std::ffi::OsString::from(v)),
                None if spec.env_remove.iter().any(|k| k == "PATH") => None,
                None => std::env::var_os("PATH"),
            };
            let found = match path.as_deref() {
                Some(p) => crate::path_search::which_outside_or_skipped(prog, p, &self.root),
                None => Err(None),
            };
            let found = found.map_err(|skipped| {
                let why = match skipped {
                    Some(p) => format!(" (skipped {} inside the work tree)", p.display()),
                    None => String::new(),
                };
                SandboxError::Io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("{prog} not found on PATH outside the work tree{why}"),
                ))
            })?;
            let mut cmd = Command::new(found);
            cmd.arg0(prog);
            cmd
        };
        cmd.args(args)
            .current_dir(cwd)
            .envs(spec.env.iter().map(|(k, v)| (k, v)))
            .stdin(stdin)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for k in &spec.env_remove {
            cmd.env_remove(k);
        }
        if spec.new_process_group {
            cmd.process_group(0);
        }
        Ok(cmd.spawn()?)
    }
}

/// Whether a `.gitignore` holds only `*` rules (at least one), besides empty lines and
/// `#` comments, read as git reads it: each line loses one trailing `\r` and its trailing
/// unescaped spaces, nothing else. Leading whitespace, tabs and other spaces (NBSP) are
/// part of the pattern for git, so `" *"` or `"*\t"` is not `*` and does not ignore the
/// spill files.
fn ignores_everything(text: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(text) else {
        return false;
    };
    let mut rules = text
        .split('\n')
        .map(|line| {
            let mut line = line.strip_suffix('\r').unwrap_or(line);
            while line.ends_with(' ') && !line.ends_with("\\ ") {
                line = &line[..line.len() - 1];
            }
            line
        })
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .peekable();
    rules.peek().is_some() && rules.all(|l| l == "*")
}

#[async_trait]
impl Sandbox for LocalSandbox {
    fn root(&self) -> &Path {
        &self.root
    }

    fn resolve(&self, p: &Path) -> Result<PathBuf, SandboxError> {
        // The same walk the permission policy uses to see where a path leads, so the
        // file a write reaches is the file the policy approved. The root is canonical
        // already; a path without `..` over missing names costs one `canonicalize`.
        match walk::walk(&self.root, p)? {
            Resolved::Inside(rel) if rel.as_os_str().is_empty() => Ok(self.root.clone()),
            Resolved::Inside(rel) => Ok(self.root.join(rel)),
            Resolved::Outside(_) => Err(SandboxError::OutsideRoot(p.to_path_buf())),
            Resolved::Loop => Err(SandboxError::Io(std::io::Error::from_raw_os_error(
                libc::ELOOP,
            ))),
        }
    }

    async fn read(&self, p: &Path) -> Result<Vec<u8>, SandboxError> {
        let path = self.resolve(p)?;
        Ok(tokio::fs::read(path).await?)
    }

    async fn metadata(&self, p: &Path) -> Result<FileMeta, SandboxError> {
        let path = self.resolve(p)?;
        let m = tokio::fs::metadata(path).await?;
        Ok(FileMeta {
            mtime: m.modified()?,
            len: m.len(),
            mode: m.mode(),
            nlink: m.nlink(),
        })
    }

    async fn write_atomic(
        &self,
        p: &Path,
        bytes: &[u8],
        mode: Option<u32>,
    ) -> Result<(), SandboxError> {
        let path = self.resolve(p)?;
        self.guard_protected_target(p, &path)?;
        let bytes = bytes.to_vec();
        tokio::task::spawn_blocking(move || -> Result<(), SandboxError> {
            use std::io::Write;
            let parent = path
                .parent()
                .ok_or_else(|| SandboxError::Denied("path has no parent".into()))?;
            std::fs::create_dir_all(parent)?;
            // Created 0666 so the kernel applies the umask (new-file default), rather
            // than NamedTempFile's owner-only 0600.
            let mut tmp = tempfile::Builder::new()
                .permissions(std::fs::Permissions::from_mode(0o666))
                .tempfile_in(parent)?;
            tmp.write_all(&bytes)?;
            tmp.as_file().sync_all()?;
            // Preserve the existing file's mode unless one is given.
            let mode = match mode {
                Some(m) => Some(m),
                None => std::fs::metadata(&path).ok().map(|m| m.mode() & 0o7777),
            };
            if let Some(m) = mode {
                tmp.as_file()
                    .set_permissions(std::fs::Permissions::from_mode(m))?;
            }
            tmp.persist(&path).map_err(|e| SandboxError::Io(e.error))?;
            Ok(())
        })
        .await
        .map_err(|e| SandboxError::Io(std::io::Error::other(e)))?
    }

    /// Unlinks `p` itself, as `unlink` does: a final symlink (dangling, or pointing
    /// outside the root or into a protected path) is removed, never followed, so the
    /// file it names is untouched. The parent directory is resolved through the walker,
    /// so the root and the protected-path guard are checked on parent plus final name.
    async fn remove_file(&self, p: &Path) -> Result<(), SandboxError> {
        let name = match p.components().next_back() {
            Some(Component::Normal(name)) => name.to_owned(),
            _ => return Err(SandboxError::Denied("path has no file name".into())),
        };
        let parent = self.resolve(p.parent().unwrap_or(Path::new("")))?;
        let path = parent.join(name);
        self.guard_protected_target(p, &path)?;
        // A link whose destination is a protected path is still refused, as before:
        // unlinking it is harmless to the target, but callers (and the patch tool's
        // tests) rely on a link into `.git/` not being deletable. A destination that is
        // outside the root, dangling or looping is not refused: only the link goes.
        if let Ok(real) = self.resolve(p) {
            self.guard_protected_target(p, &real)?;
        }
        Ok(tokio::fs::remove_file(path).await?)
    }

    /// A cache write: skipped when the content-addressed file is already there with the
    /// right length, and never fsynced (a lost spill only costs a full re-read). The rename
    /// still keeps readers from seeing a partial file.
    async fn spill(&self, bytes: &[u8]) -> Result<PathBuf, SandboxError> {
        let hash = blake3::hash(bytes).to_hex();
        let rel = Path::new(SPILL_DIR).join(format!("{}.txt", &hash[..16]));
        let path = self.resolve(&rel)?;
        self.guard_protected_target(&rel, &path)?;
        self.ensure_spill_ignore().await?;
        let len = bytes.len() as u64;
        if tokio::fs::metadata(&path)
            .await
            .is_ok_and(|m| m.is_file() && m.len() == len)
        {
            return Ok(rel);
        }
        if !self.spill_ready.load(Ordering::Acquire)
            || !tokio::fs::try_exists(path.parent().unwrap_or(&self.root)).await?
        {
            self.prepare_spill_dir().await?;
        }
        let bytes = bytes.to_vec();
        tokio::task::spawn_blocking(move || -> Result<(), SandboxError> {
            use std::io::Write;
            let parent = path
                .parent()
                .ok_or_else(|| SandboxError::Denied("path has no parent".into()))?;
            let mut tmp = tempfile::Builder::new()
                .permissions(std::fs::Permissions::from_mode(0o666))
                .tempfile_in(parent)?;
            tmp.write_all(&bytes)?;
            tmp.persist(&path).map_err(|e| SandboxError::Io(e.error))?;
            Ok(())
        })
        .await
        .map_err(|e| SandboxError::Io(std::io::Error::other(e)))??;
        Ok(rel)
    }

    async fn spill_stream(&self) -> Result<Option<(PathBuf, SpillWriter)>, SandboxError> {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let name = format!(
            "stream-{}-{nanos}-{}.txt",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let rel = Path::new(SPILL_DIR).join(name);
        let path = self.resolve(&rel)?;
        self.guard_protected_target(&rel, &path)?;
        self.ensure_spill_ignore().await?;
        if !self.spill_ready.load(Ordering::Acquire)
            || !tokio::fs::try_exists(path.parent().unwrap_or(&self.root)).await?
        {
            self.prepare_spill_dir().await?;
        }
        // A new file only: never one a link or an earlier run left at that name.
        let file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
            .await?;
        Ok(Some((rel, Box::new(file))))
    }

    async fn spawn(&self, spec: SpawnSpec) -> Result<Child, SandboxError> {
        self.spawn_with(spec, Stdio::null())
    }

    async fn spawn_stdio(&self, spec: SpawnSpec) -> Result<Child, SandboxError> {
        self.spawn_with(spec, Stdio::piped())
    }

    fn walker(&self, base: &Path) -> Result<ignore::WalkBuilder, SandboxError> {
        let mut b = ignore::WalkBuilder::new(self.resolve(base)?);
        let root = self.root.clone();
        // Drop symlinked entries whose target lies outside the root, so confinement
        // holds even with `follow_links(true)`.
        b.filter_entry(move |e| {
            !e.path_is_symlink() || e.path().canonicalize().is_ok_and(|c| c.starts_with(&root))
        });
        Ok(b)
    }

    async fn http_get(&self, url: &Url) -> Result<HttpResponse, SandboxError> {
        let net = |e: reqwest::Error| SandboxError::Network(e.to_string());
        let mut resp = self.client.get(url.clone()).send().await.map_err(net)?;
        let status = resp.status().as_u16();
        let headers = resp
            .headers()
            .iter()
            .map(|(k, v)| {
                (
                    k.to_string(),
                    String::from_utf8_lossy(v.as_bytes()).into_owned(),
                )
            })
            .collect();
        let mut body = Vec::new();
        while let Some(chunk) = resp.chunk().await.map_err(net)? {
            if body.len() + chunk.len() > MAX_BODY {
                return Err(SandboxError::Network(
                    "response body exceeds 10 MB cap".into(),
                ));
            }
            body.extend_from_slice(&chunk);
        }
        Ok(HttpResponse {
            status,
            headers,
            body,
        })
    }
}
