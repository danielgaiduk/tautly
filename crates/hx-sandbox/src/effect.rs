use std::path::{Path, PathBuf};
use std::time::SystemTime;

use async_trait::async_trait;
use tokio::process::Child;
use url::Url;

/// The side-effect class of a tool invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    ReadOnly,
    Write { paths: Vec<PathBuf> },
    Exec { argv: Vec<String> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileMeta {
    pub mtime: SystemTime,
    pub len: u64,
    pub mode: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnSpec {
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
    pub new_process_group: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    #[error("path escapes sandbox root: {0}")]
    OutsideRoot(PathBuf),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("network error: {0}")]
    Network(String),
    #[error("denied: {0}")]
    Denied(String),
}

#[async_trait]
pub trait Sandbox: Send + Sync {
    fn root(&self) -> &Path;
    /// Resolve `p` (relative to root, or absolute) to a canonical path inside the root.
    fn resolve(&self, p: &Path) -> Result<PathBuf, SandboxError>;
    async fn read(&self, p: &Path) -> Result<Vec<u8>, SandboxError>;
    async fn metadata(&self, p: &Path) -> Result<FileMeta, SandboxError>;
    /// Atomically replace the file at `p`. Pass `p` as the caller requested it, not
    /// pre-resolved: a path that reaches a protected path (`.git/`, `.hx/`, ...) only
    /// through a symlink is refused with [`SandboxError::Denied`].
    async fn write_atomic(
        &self,
        p: &Path,
        bytes: &[u8],
        mode: Option<u32>,
    ) -> Result<(), SandboxError>;
    /// Delete the file at `p` (confined to the root). Errors if it does not exist.
    /// Same symlink rule as [`Sandbox::write_atomic`].
    async fn remove_file(&self, p: &Path) -> Result<(), SandboxError>;
    /// Persist `bytes` (e.g. over-long tool output) in a sandbox-managed scratch area and
    /// return its root-relative path, so the model can `read` it later. Content-addressed.
    async fn spill(&self, bytes: &[u8]) -> Result<PathBuf, SandboxError>;
    async fn spawn(&self, spec: SpawnSpec) -> Result<Child, SandboxError>;
    fn walker(&self, base: &Path) -> Result<ignore::WalkBuilder, SandboxError>;
    async fn http_get(&self, url: &Url) -> Result<HttpResponse, SandboxError>;
}
