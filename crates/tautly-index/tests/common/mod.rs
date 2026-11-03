#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tautly_index::Index;
use tautly_sandbox::{FileMeta, LocalSandbox, Sandbox, SandboxError};

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
    async fn spawn(
        &self,
        spec: tautly_sandbox::SpawnSpec,
    ) -> Result<tautly_sandbox::Child, SandboxError> {
        self.inner.spawn(spec).await
    }
    fn walker(&self, base: &Path) -> Result<ignore::WalkBuilder, SandboxError> {
        let mut b = self.inner.walker(base)?;
        b.filter_entry(|e| e.file_name() != "blocked");
        Ok(b)
    }
    async fn http_get(
        &self,
        url: &tautly_sandbox::Url,
    ) -> Result<tautly_sandbox::HttpResponse, SandboxError> {
        self.inner.http_get(url).await
    }
}

/// Writes `files` synthetic Rust files of `fns` functions each (about 45 bytes per
/// function) under `root`, spread over 350 directories; file `files / 2` also contains
/// the line `// the_one_needle_marker`. Deterministic. Shared by the scale test and
/// benchmarks.
pub fn synthetic_corpus(root: &Path, files: usize, fns: usize) {
    let mut seed = 0x2545_f491_4f6c_dd1du64;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let vocab: Vec<String> = (0..3000)
        .map(|i| format!("w{:x}_{}", next() % 100_000, i % 97))
        .collect();
    for i in 0..files {
        let dir = root.join(format!("d{}/e{}", i % 50, i % 7));
        std::fs::create_dir_all(&dir).unwrap();
        let mut body = String::new();
        for _ in 0..fns {
            let w = &vocab[(next() % vocab.len() as u64) as usize];
            body.push_str(&format!("fn {w}() {{ call_{w}(); }}\n"));
        }
        if i == files / 2 {
            body.push_str("// the_one_needle_marker\n");
        }
        std::fs::write(dir.join(format!("f{i}.rs")), body).unwrap();
    }
}

/// Delegates to a real sandbox, counting `metadata` calls (a full refresh stats every
/// file; a single reindex stats one) and failing `read` for chosen paths.
pub struct ProbeSandbox {
    pub inner: LocalSandbox,
    pub metadata_calls: std::sync::atomic::AtomicUsize,
    pub walker_calls: std::sync::atomic::AtomicUsize,
    pub fail_read: std::sync::Mutex<std::collections::HashSet<PathBuf>>,
    pub fail_metadata: std::sync::Mutex<std::collections::HashSet<PathBuf>>,
    pub fail_walk: std::sync::atomic::AtomicBool,
}

impl ProbeSandbox {
    pub fn new(root: &Path) -> Arc<ProbeSandbox> {
        Arc::new(ProbeSandbox {
            inner: LocalSandbox::new(root.to_path_buf()).unwrap(),
            metadata_calls: std::sync::atomic::AtomicUsize::new(0),
            walker_calls: std::sync::atomic::AtomicUsize::new(0),
            fail_read: Default::default(),
            fail_metadata: Default::default(),
            fail_walk: std::sync::atomic::AtomicBool::new(false),
        })
    }

    pub fn calls(&self) -> usize {
        self.metadata_calls
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn walks(&self) -> usize {
        self.walker_calls.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn fail_metadata_of(&self, rel: &str, on: bool) {
        let mut set = self.fail_metadata.lock().unwrap();
        if on {
            set.insert(PathBuf::from(rel));
        } else {
            set.remove(Path::new(rel));
        }
    }

    pub fn fail_reads_of(&self, rel: &str, on: bool) {
        let mut set = self.fail_read.lock().unwrap();
        if on {
            set.insert(PathBuf::from(rel));
        } else {
            set.remove(Path::new(rel));
        }
    }
}

#[async_trait::async_trait]
impl Sandbox for ProbeSandbox {
    fn root(&self) -> &Path {
        self.inner.root()
    }
    fn resolve(&self, p: &Path) -> Result<PathBuf, SandboxError> {
        self.inner.resolve(p)
    }
    async fn read(&self, p: &Path) -> Result<Vec<u8>, SandboxError> {
        let rel = p.strip_prefix(self.inner.root()).unwrap_or(p);
        if self.fail_read.lock().unwrap().contains(rel) {
            return Err(SandboxError::Io(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "injected read failure",
            )));
        }
        self.inner.read(p).await
    }
    async fn metadata(&self, p: &Path) -> Result<FileMeta, SandboxError> {
        self.metadata_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let rel = p.strip_prefix(self.inner.root()).unwrap_or(p);
        if self.fail_metadata.lock().unwrap().contains(rel) {
            return Err(SandboxError::Io(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "injected metadata failure",
            )));
        }
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
    async fn spawn(
        &self,
        spec: tautly_sandbox::SpawnSpec,
    ) -> Result<tautly_sandbox::Child, SandboxError> {
        self.inner.spawn(spec).await
    }
    fn walker(&self, base: &Path) -> Result<ignore::WalkBuilder, SandboxError> {
        self.walker_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if self.fail_walk.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(SandboxError::Denied("injected walk failure".into()));
        }
        self.inner.walker(base)
    }
    async fn http_get(
        &self,
        url: &tautly_sandbox::Url,
    ) -> Result<tautly_sandbox::HttpResponse, SandboxError> {
        self.inner.http_get(url).await
    }
}

/// Delegates to a real sandbox and runs a one-shot hook the first time a walker is asked
/// for (the start of a refresh's scan), to let a test act in the middle of a refresh.
pub struct HookSandbox {
    pub inner: LocalSandbox,
    pub on_walk: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

#[async_trait::async_trait]
impl Sandbox for HookSandbox {
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
    async fn spawn(
        &self,
        spec: tautly_sandbox::SpawnSpec,
    ) -> Result<tautly_sandbox::Child, SandboxError> {
        self.inner.spawn(spec).await
    }
    fn walker(&self, base: &Path) -> Result<ignore::WalkBuilder, SandboxError> {
        if let Some(hook) = self.on_walk.lock().unwrap().take() {
            hook();
        }
        self.inner.walker(base)
    }
    async fn http_get(
        &self,
        url: &tautly_sandbox::Url,
    ) -> Result<tautly_sandbox::HttpResponse, SandboxError> {
        self.inner.http_get(url).await
    }
}
