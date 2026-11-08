use std::path::{Path, PathBuf};
use std::time::SystemTime;

use async_trait::async_trait;
use tokio::process::Child;
use url::Url;

/// Where [`Sandbox::spill_stream`] output goes.
pub type SpillWriter = Box<dyn tokio::io::AsyncWrite + Send + Unpin>;

/// The side-effect class of a tool invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Reads only: never asks.
    ReadOnly,
    /// Writes files.
    Write {
        /// The files written, as the tool names them.
        paths: Vec<PathBuf>,
    },
    /// Runs a command.
    Exec {
        /// The command's words (for a shell command, see the policy's parse).
        argv: Vec<String>,
    },
    /// Network egress to `host`: lowercase, no port, no trailing dot, no IPv6 brackets.
    Network {
        /// The host reached.
        host: String,
    },
}

/// What a tool needs to know of a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileMeta {
    /// Last modification time.
    pub mtime: SystemTime,
    /// Size in bytes.
    pub len: u64,
    /// Permission bits.
    pub mode: u32,
    /// Hard-link count: above 1 the content is reachable under other names.
    pub nlink: u64,
}

/// How to start a process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnSpec {
    /// The program and its arguments.
    pub argv: Vec<String>,
    /// The working directory.
    pub cwd: PathBuf,
    /// Variables added to the environment.
    pub env: Vec<(String, String)>,
    /// Variables removed from the inherited environment.
    pub env_remove: Vec<String>,
    /// Start it in a process group of its own, so it can be killed with its children.
    pub new_process_group: bool,
}

/// An HTTP response, read in full.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    /// The status code.
    pub status: u16,
    /// The headers, as name and value.
    pub headers: Vec<(String, String)>,
    /// The body.
    pub body: Vec<u8>,
}

/// Why the sandbox refused or failed an operation.
#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    /// A path that resolves outside the workspace root.
    #[error("path escapes sandbox root: {0}")]
    OutsideRoot(PathBuf),
    /// The file system failed.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// A network request failed.
    #[error("network error: {0}")]
    Network(String),
    /// Refused (a protected path reached through a symlink, an operation this sandbox does not offer).
    #[error("denied: {0}")]
    Denied(String),
}

/// Where tools touch the world: files under one root, processes and the network.
#[async_trait]
pub trait Sandbox: Send + Sync {
    /// The workspace root (canonical).
    fn root(&self) -> &Path;
    /// Resolve `p` (relative to root, or absolute) to a canonical path inside the root.
    fn resolve(&self, p: &Path) -> Result<PathBuf, SandboxError>;
    /// The bytes of the file at `p` (confined to the root).
    async fn read(&self, p: &Path) -> Result<Vec<u8>, SandboxError>;
    /// The metadata of the file at `p` (confined to the root).
    async fn metadata(&self, p: &Path) -> Result<FileMeta, SandboxError>;
    /// Atomically replace the file at `p`. Pass `p` as the caller requested it, not
    /// pre-resolved: a path that reaches a protected path (`.git/`, `.tautly/`, ...) only
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
    /// Start a spill file in the same scratch area for output too large to hold in memory:
    /// its root-relative path and the new file, written as the output arrives (named
    /// uniquely, not by content). `None` when this sandbox offers no such file; the caller
    /// then keeps the output in memory and uses [`Sandbox::spill`].
    async fn spill_stream(&self) -> Result<Option<(PathBuf, SpillWriter)>, SandboxError> {
        Ok(None)
    }
    /// Start a process with stdout and stderr piped.
    async fn spawn(&self, spec: SpawnSpec) -> Result<Child, SandboxError>;
    /// Like [`Sandbox::spawn`], with the child's stdin piped as well: for a process that
    /// speaks a protocol over stdin and stdout (an MCP stdio server). A sandbox that cannot
    /// run such processes refuses.
    async fn spawn_stdio(&self, spec: SpawnSpec) -> Result<Child, SandboxError> {
        let _ = spec;
        Err(SandboxError::Denied(
            "this sandbox cannot run stdio servers".into(),
        ))
    }
    /// A walker over `base` that respects `.gitignore` and hidden files.
    fn walker(&self, base: &Path) -> Result<ignore::WalkBuilder, SandboxError>;
    /// GET `url` and read the whole response.
    async fn http_get(&self, url: &Url) -> Result<HttpResponse, SandboxError>;
}
