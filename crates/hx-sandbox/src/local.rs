use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;

use async_trait::async_trait;
use tokio::process::{Child, Command};
use url::Url;

use crate::effect::{FileMeta, HttpResponse, Sandbox, SandboxError, SpawnSpec};
use crate::protected::{self, is_protected};

const MAX_BODY: usize = 10 * 1024 * 1024;

/// Sandbox confining filesystem access to a canonical root directory.
#[derive(Debug, Clone)]
pub struct LocalSandbox {
    root: PathBuf,
    client: reqwest::Client,
}

impl LocalSandbox {
    pub fn new(root: PathBuf) -> Result<Self, SandboxError> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()
            .map_err(|e| SandboxError::Network(e.to_string()))?;
        Ok(Self {
            root: root.canonicalize()?,
            client,
        })
    }
}

/// Lexically normalize `.` and `..`; `..` past the start yields None.
fn normalize(p: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                if !out.pop() {
                    return None;
                }
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    Some(out)
}

impl LocalSandbox {
    /// Refuse to modify a protected path reached through a symlink from a path
    /// that is not itself protected (`NOTES.md -> .git/config`). Naming a
    /// protected path directly is the permission policy's decision (it asks);
    /// following a repository-planted link to one never is. `p` is the path as
    /// the caller requested it; `resolved` is `self.resolve(p)`.
    fn guard_protected_target(&self, p: &Path, resolved: &Path) -> Result<(), SandboxError> {
        let Ok(real) = resolved.strip_prefix(&self.root) else {
            return Ok(());
        };
        if !is_protected(real) {
            return Ok(());
        }
        let named = protected::normalize(&self.root, &p.to_string_lossy())
            .is_some_and(|rel| is_protected(&rel));
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
        let outside = || SandboxError::OutsideRoot(p.to_path_buf());
        let joined = self.root.join(p); // absolute `p` replaces root
        // Walk components, canonicalizing the existing prefix as we go (so `..` after a
        // symlink is resolved against the real directory); the non-existent tail is
        // appended lexically.
        let mut existing = PathBuf::new();
        let mut tail: Vec<std::ffi::OsString> = Vec::new();
        let mut exists = true;
        for c in joined.components() {
            if !exists {
                match c {
                    Component::ParentDir => {
                        if tail.pop().is_none() {
                            return Err(outside());
                        }
                    }
                    Component::CurDir => {}
                    other => tail.push(other.as_os_str().to_owned()),
                }
                continue;
            }
            existing.push(c.as_os_str());
            if matches!(c, Component::RootDir | Component::Prefix(_)) {
                continue;
            }
            match existing.canonicalize() {
                Ok(canon) => existing = canon,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    exists = false;
                    existing.pop();
                    tail.push(c.as_os_str().to_owned());
                }
                Err(e) => return Err(e.into()),
            }
        }
        let mut result = existing;
        result.extend(tail);
        let result = normalize(&result).ok_or_else(outside)?;
        if result.starts_with(&self.root) {
            Ok(result)
        } else {
            Err(outside())
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

    async fn remove_file(&self, p: &Path) -> Result<(), SandboxError> {
        let path = self.resolve(p)?;
        self.guard_protected_target(p, &path)?;
        Ok(tokio::fs::remove_file(path).await?)
    }

    async fn spill(&self, bytes: &[u8]) -> Result<PathBuf, SandboxError> {
        let tmp = Path::new(".hx/tmp");
        let ignore = tmp.join(".gitignore");
        if !tokio::fs::try_exists(self.resolve(&ignore)?).await? {
            self.write_atomic(&ignore, b"*", None).await?;
        }
        let hash = blake3::hash(bytes).to_hex();
        let rel = tmp.join("spill").join(format!("{}.txt", &hash[..16]));
        self.write_atomic(&rel, bytes, None).await?;
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
