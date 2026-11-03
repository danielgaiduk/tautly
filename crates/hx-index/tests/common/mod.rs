#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use hx_index::Index;
use hx_sandbox::{FileMeta, LocalSandbox, Sandbox, SandboxError};

pub fn write_all(root: &Path, files: &[(&str, &str)]) {
    for (rel, content) in files {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }
}

pub fn open_at(root: &Path, db: &Path) -> Index {
    let sb = Arc::new(LocalSandbox::new(root.to_path_buf()).unwrap());
    Index::open(db, sb).unwrap()
}

/// A workspace plus an index whose database lives outside it (so the watcher never
/// sees the database's own writes).
pub struct Fixture {
    pub root: tempfile::TempDir,
    pub db_dir: tempfile::TempDir,
}

impl Fixture {
    pub fn new(files: &[(&str, &str)]) -> Fixture {
        let root = tempfile::tempdir().unwrap();
        write_all(root.path(), files);
        Fixture {
            root,
            db_dir: tempfile::tempdir().unwrap(),
        }
    }

    pub fn db(&self) -> PathBuf {
        self.db_dir.path().join("index.db")
    }

    pub fn open(&self) -> Index {
        open_at(self.root.path(), &self.db())
    }

    pub fn path(&self, rel: &str) -> PathBuf {
        self.root.path().join(rel)
    }
}

pub fn paths(v: &[PathBuf]) -> Vec<&str> {
    v.iter().map(|p| p.to_str().unwrap()).collect()
}

/// Poll `cond` every 10 ms until it holds or `limit` passes.
pub async fn eventually(limit: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    loop {
        if cond() {
            return true;
        }
        if start.elapsed() > limit {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Delegates to a real sandbox but hands out walkers that hide directories named
/// `blocked` (a stand-in for any extra confinement a sandbox applies in its walker).
pub struct BlockingSandbox {
    pub inner: LocalSandbox,
}

#[async_trait::async_trait]
impl Sandbox for BlockingSandbox {
    fn root(&self) -> &Path {
        self.inner.root()
    }
    fn resolve(&self, p: &Path) -> Result<PathBuf, SandboxError> {
        self.inner.resolve(p)
    }
    async fn read(&self, p: &Path) -> Result<Vec<u8>, SandboxError> {
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
    async fn spawn(&self, spec: hx_sandbox::SpawnSpec) -> Result<hx_sandbox::Child, SandboxError> {
        self.inner.spawn(spec).await
    }
    fn walker(&self, base: &Path) -> Result<ignore::WalkBuilder, SandboxError> {
        let mut b = self.inner.walker(base)?;
        b.filter_entry(|e| e.file_name() != "blocked");
        Ok(b)
    }
    async fn http_get(
        &self,
        url: &hx_sandbox::Url,
    ) -> Result<hx_sandbox::HttpResponse, SandboxError> {
        self.inner.http_get(url).await
    }
}
