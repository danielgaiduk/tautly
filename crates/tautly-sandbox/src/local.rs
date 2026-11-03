use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use tokio::process::{Child, Command};
use url::Url;

use crate::effect::{FileMeta, HttpResponse, Sandbox, SandboxError, SpawnSpec};
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
        let client = reqwest::Client::builder()
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
    /// Create `.tautly/tmp` with a `.gitignore` of `*` (unless one exists) and the spill
    /// directory. Done once, and again only if the directory disappears.
    async fn prepare_spill_dir(&self) -> Result<(), SandboxError> {
        let ignore = Path::new(".tautly/tmp/.gitignore");
        if !tokio::fs::try_exists(self.resolve(ignore)?).await? {
            self.write_atomic(ignore, b"*", None).await?;
        }
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

    async fn spawn(&self, spec: SpawnSpec) -> Result<Child, SandboxError> {
        let (prog, args) = spec
            .argv
            .split_first()
            .ok_or_else(|| SandboxError::Denied("empty argv".into()))?;
        let cwd = self.resolve(&spec.cwd)?;
        let mut cmd = Command::new(prog);
        cmd.args(args)
            .current_dir(cwd)
            .envs(spec.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null())
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
