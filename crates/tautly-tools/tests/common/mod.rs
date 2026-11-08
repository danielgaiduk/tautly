//! Shared fixtures for the index-backed tool tests: a sandbox that counts file reads and
//! walks, and an environment with a real code index over a temp directory.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;
use tautly_index::{Index, WatchHandle};
use tautly_provider::EditFormat;
use tautly_sandbox::{
    Child, FileMeta, HttpResponse, LocalSandbox, Sandbox, SandboxError, SpawnSpec, Url,
};
use tautly_tools::{Tool, ToolCx, ToolResult};
use tempfile::TempDir;

/// A [`LocalSandbox`] that records which files were read and how often it was walked.
pub struct Counting {
    inner: LocalSandbox,
    reads: Mutex<Vec<PathBuf>>,
    walks: AtomicUsize,
}

impl Counting {
    pub fn new(root: &Path) -> Self {
        Self {
            inner: LocalSandbox::new(root.to_path_buf()).unwrap(),
            reads: Mutex::new(Vec::new()),
            walks: AtomicUsize::new(0),
        }
    }

    /// Root-relative paths read since the last reset, sorted.
    pub fn reads(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .reads
            .lock()
            .unwrap()
            .iter()
            .map(|p| {
                p.strip_prefix(self.inner.root())
                    .unwrap_or(p)
                    .display()
                    .to_string()
            })
            .collect();
        v.sort();
        v
    }

    pub fn walks(&self) -> usize {
        self.walks.load(Ordering::SeqCst)
    }

    pub fn reset(&self) {
        self.reads.lock().unwrap().clear();
        self.walks.store(0, Ordering::SeqCst);
    }
}

#[async_trait]
impl Sandbox for Counting {
    fn root(&self) -> &Path {
        self.inner.root()
    }
    fn resolve(&self, p: &Path) -> Result<PathBuf, SandboxError> {
        self.inner.resolve(p)
    }
    async fn read(&self, p: &Path) -> Result<Vec<u8>, SandboxError> {
        self.reads.lock().unwrap().push(p.to_path_buf());
        self.inner.read(p).await
    }
    async fn metadata(&self, p: &Path) -> Result<FileMeta, SandboxError> {
        self.inner.metadata(p).await
    }
    async fn write_atomic(
        &self,
        p: &Path,
        bytes: &[u8],
        mode: Option<u32>,
    ) -> Result<(), SandboxError> {
        self.inner.write_atomic(p, bytes, mode).await
    }
    async fn remove_file(&self, p: &Path) -> Result<(), SandboxError> {
        self.inner.remove_file(p).await
    }
    async fn spill(&self, bytes: &[u8]) -> Result<PathBuf, SandboxError> {
        self.inner.spill(bytes).await
    }
    async fn spawn(&self, spec: SpawnSpec) -> Result<Child, SandboxError> {
        self.inner.spawn(spec).await
    }
    fn walker(&self, base: &Path) -> Result<ignore::WalkBuilder, SandboxError> {
        self.walks.fetch_add(1, Ordering::SeqCst);
        self.inner.walker(base)
    }
    async fn http_get(&self, url: &Url) -> Result<HttpResponse, SandboxError> {
        self.inner.http_get(url).await
    }
}

/// How much of the index a test environment has built.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Built {
    /// No index in the tool context.
    None,
    /// Refreshed (tree and symbols), trigrams not built.
    Refreshed,
    /// Refreshed and trigrams built.
    Full,
}

pub struct Env {
    pub dir: TempDir,
    _db: TempDir,
    pub sb: Counting,
    pub index: Arc<Index>,
    pub cx: ToolCx,
    /// The index's file watcher; the tools trust the index only while it runs.
    watch: Option<WatchHandle>,
}

impl Env {
    /// Stop the index's file watcher.
    pub fn stop_watcher(&mut self) {
        self.watch = None;
    }

    /// Start the index's file watcher again.
    pub fn start_watcher(&mut self) {
        self.watch = Some(Arc::clone(&self.index).start_watcher().unwrap());
    }

    /// A context without the index, over the same directory.
    pub fn cx_without_index(&self) -> ToolCx {
        ToolCx::new(EditFormat::StrReplace, self.dir.path().join(".spill")).with_turn(1)
    }

    pub async fn run(&self, tool: &dyn Tool, args: Value) -> ToolResult {
        tool.run(args, &self.sb, &self.cx).await
    }

    pub async fn run_without_index(&self, tool: &dyn Tool, args: Value) -> ToolResult {
        tool.run(args, &self.sb, &self.cx_without_index()).await
    }

    pub async fn put(&self, rel: &str, content: &[u8]) {
        self.sb
            .write_atomic(Path::new(rel), content, None)
            .await
            .unwrap();
    }
}

/// An environment over a fresh temp directory holding `files`, with the index built as far
/// as `built` says. The index has its own sandbox, so reads it makes are not counted.
pub async fn env(files: &[(&str, &[u8])], built: Built) -> Env {
    let dir = TempDir::new().unwrap();
    let db = TempDir::new().unwrap();
    let sb = Counting::new(dir.path());
    for (rel, content) in files {
        sb.write_atomic(Path::new(rel), content, None)
            .await
            .unwrap();
    }
    let index_sb = Arc::new(LocalSandbox::new(dir.path().to_path_buf()).unwrap());
    let index = Arc::new(Index::open(&db.path().join("index.db"), index_sb).unwrap());
    if built != Built::None {
        index.refresh().await.unwrap();
        assert!(index.is_ready());
    }
    if built == Built::Full {
        index.build_trigrams().await.unwrap();
        assert!(index.trigrams_ready());
    }
    let mut cx = ToolCx::new(EditFormat::StrReplace, dir.path().join(".spill")).with_turn(1);
    let mut watch = None;
    if built != Built::None {
        cx = cx.with_index(Arc::clone(&index));
        watch = Some(Arc::clone(&index).start_watcher().unwrap());
    }
    Env {
        dir,
        _db: db,
        sb,
        index,
        cx,
        watch,
    }
}
