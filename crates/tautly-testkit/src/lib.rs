//! Helpers shared by the tests of the Tautly crates: an engine harness ([`Run`]),
//! temporary-directory fixtures ([`Fx`], [`Repo`]), a model catalog and a sandbox that
//! forwards to a [`LocalSandbox`] except where a test overrides it ([`Delegate`]).

use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::process::Command as Process;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tautly_core::{Command, Config, Engine, EngineBuilder, Event};
use tautly_provider::{Catalog, MockProvider};
use tautly_sandbox::{
    Child, FileMeta, HttpResponse, LocalSandbox, Sandbox, SandboxError, SpawnSpec, SpillWriter, Url,
};
use tempfile::TempDir;
use tokio::sync::mpsc;

/// How long a test waits for an event before it fails.
pub const WAIT: Duration = Duration::from_secs(30);

/// The model of [`catalog`].
pub const MODEL: &str = "mock/test";

/// [`MODEL`] with a `window`-token context, `max_output` and `edit_format`, unpriced.
pub fn catalog(window: u32, max_output: u32, edit_format: &str) -> Catalog {
    Catalog::from_toml(&format!(
        r#"[models."{MODEL}"]
context_window = {window}
max_output = {max_output}
parallel_tools = true
reasoning = "none"
cache = "none"
images = false
edit_format = "{edit_format}"
"#
    ))
    .unwrap()
}

/// A running engine and its events.
pub struct Run {
    pub engine: Engine,
    pub events: mpsc::Receiver<Event>,
    /// How long [`Run::next`] waits ([`WAIT`] unless a test sets it).
    pub wait: Duration,
}

pub fn start(b: EngineBuilder) -> Run {
    let (engine, events) = b.build().unwrap();
    Run {
        engine,
        events,
        wait: WAIT,
    }
}

impl Run {
    pub async fn send(&self, cmd: Command) {
        self.engine.send(cmd).await.unwrap();
    }

    pub async fn say(&self, text: &str) {
        self.send(Command::SendMessage {
            id: None,
            text: text.into(),
        })
        .await;
    }

    pub async fn next(&mut self) -> Event {
        tokio::time::timeout(self.wait, self.events.recv())
            .await
            .expect("timed out waiting for an event")
            .expect("event channel closed")
    }

    /// Events up to and including the first that matches `pred`.
    pub async fn until(&mut self, pred: impl Fn(&Event) -> bool) -> Vec<Event> {
        let mut out = Vec::new();
        loop {
            let ev = self.next().await;
            let done = pred(&ev);
            out.push(ev);
            if done {
                return out;
            }
        }
    }

    /// Send `text`; the events through `TurnFinished`.
    pub async fn turn_with(&mut self, text: &str) -> Vec<Event> {
        self.say(text).await;
        self.until(|e| matches!(e, Event::TurnFinished { .. }))
            .await
    }

    /// Shut down; the events until the engine stopped (its log is closed).
    pub async fn stop(mut self) -> Vec<Event> {
        self.send(Command::Shutdown).await;
        let mut out = Vec::new();
        let closed = tokio::time::timeout(self.wait, async {
            while let Some(ev) = self.events.recv().await {
                out.push(ev);
            }
        })
        .await;
        assert!(closed.is_ok(), "the engine did not stop");
        out
    }
}

/// A work tree and a data home, both temporary.
pub struct Fx {
    pub dir: TempDir,
    pub home: TempDir,
}

impl Default for Fx {
    fn default() -> Self {
        Self::new()
    }
}

impl Fx {
    pub fn new() -> Fx {
        Fx {
            dir: tempfile::tempdir().unwrap(),
            home: tempfile::tempdir().unwrap(),
        }
    }

    pub fn root(&self) -> PathBuf {
        self.dir.path().canonicalize().unwrap()
    }

    /// Write `rel` under the work tree, creating its directories.
    pub fn write(&self, rel: &str, text: &str) {
        let p = self.dir.path().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    /// An engine on `mock` over the work tree, with this data home.
    pub fn builder(&self, cfg: Config, mock: &MockProvider) -> EngineBuilder {
        EngineBuilder {
            provider: Some(Arc::new(mock.clone())),
            data_home: Some(self.home.path().to_path_buf()),
            ..EngineBuilder::new(cfg, self.dir.path())
        }
    }
}

/// Run git in `dir` with no user or system config; its stdout, or `None` if it failed.
pub fn git_in(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Process::new("git")
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(["-c", "user.name=T", "-c", "user.email=t@example.com"])
        .args(args)
        .output()
        .unwrap();
    out.status
        .success()
        .then(|| String::from_utf8(out.stdout).unwrap())
}

/// A work tree (a git repository or not) and a data home, both temporary.
pub struct Repo {
    pub dir: TempDir,
    pub home: TempDir,
}

impl Repo {
    /// A directory that is not a git work tree, holding `files`.
    pub fn plain(files: &[(&str, &str)]) -> Repo {
        let repo = Repo {
            dir: tempfile::tempdir().unwrap(),
            home: tempfile::tempdir().unwrap(),
        };
        for (rel, text) in files {
            repo.write(rel, text);
        }
        repo
    }

    /// A git repository with `files` committed on `main`.
    pub fn git_repo(files: &[(&str, &str)]) -> Repo {
        let repo = Repo::plain(&[]);
        repo.git(&["init", "-q", "-b", "main"]);
        for (rel, text) in files {
            repo.write(rel, text);
        }
        repo.git(&["add", "-A"]);
        repo.git(&["commit", "-q", "-m", "init"]);
        repo
    }

    pub fn root(&self) -> PathBuf {
        self.dir.path().canonicalize().unwrap()
    }

    /// Write `rel` under the work tree, creating its directories.
    pub fn write(&self, rel: &str, text: &str) {
        let p = self.root().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    pub fn read(&self, rel: &str) -> Option<String> {
        std::fs::read_to_string(self.root().join(rel)).ok()
    }

    /// [`git_in`] the work tree.
    pub fn git(&self, args: &[&str]) -> Option<String> {
        git_in(&self.root(), args)
    }
}

/// A test sandbox that forwards every call to [`Delegate::inner`] unless it overrides it.
/// Wrap it in [`Delegating`] to use it as a [`Sandbox`].
///
/// `spill_stream` and `spawn_stdio` are not forwarded: they keep the [`Sandbox`] defaults
/// (no spill file; stdio servers refused) unless the test overrides them.
#[async_trait]
pub trait Delegate: Send + Sync {
    fn inner(&self) -> &LocalSandbox;

    fn resolve(&self, p: &Path) -> Result<PathBuf, SandboxError> {
        self.inner().resolve(p)
    }
    async fn read(&self, p: &Path) -> Result<Vec<u8>, SandboxError> {
        self.inner().read(p).await
    }
    async fn metadata(&self, p: &Path) -> Result<FileMeta, SandboxError> {
        self.inner().metadata(p).await
    }
    async fn write_atomic(
        &self,
        p: &Path,
        bytes: &[u8],
        mode: Option<u32>,
    ) -> Result<(), SandboxError> {
        self.inner().write_atomic(p, bytes, mode).await
    }
    async fn remove_file(&self, p: &Path) -> Result<(), SandboxError> {
        self.inner().remove_file(p).await
    }
    async fn spill(&self, bytes: &[u8]) -> Result<PathBuf, SandboxError> {
        self.inner().spill(bytes).await
    }
    // `spill_stream` and `spawn_stdio` do not forward: they copy the `Sandbox` trait
    // defaults, as the wrappers did, and must be kept equal to them.
    async fn spill_stream(&self) -> Result<Option<(PathBuf, SpillWriter)>, SandboxError> {
        Ok(None)
    }
    async fn spawn(&self, spec: SpawnSpec) -> Result<Child, SandboxError> {
        self.inner().spawn(spec).await
    }
    async fn spawn_stdio(&self, spec: SpawnSpec) -> Result<Child, SandboxError> {
        let _ = spec;
        Err(SandboxError::Denied(
            "this sandbox cannot run stdio servers".into(),
        ))
    }
    fn walker(&self, base: &Path) -> Result<ignore::WalkBuilder, SandboxError> {
        self.inner().walker(base)
    }
    async fn http_get(&self, url: &Url) -> Result<HttpResponse, SandboxError> {
        self.inner().http_get(url).await
    }
}

/// A [`Delegate`] as a [`Sandbox`]; derefs to the delegate.
pub struct Delegating<T>(pub T);

impl<T> Deref for Delegating<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}

#[async_trait]
impl<T: Delegate> Sandbox for Delegating<T> {
    fn root(&self) -> &Path {
        self.0.inner().root()
    }
    fn resolve(&self, p: &Path) -> Result<PathBuf, SandboxError> {
        self.0.resolve(p)
    }
    async fn read(&self, p: &Path) -> Result<Vec<u8>, SandboxError> {
        self.0.read(p).await
    }
    async fn metadata(&self, p: &Path) -> Result<FileMeta, SandboxError> {
        self.0.metadata(p).await
    }
    async fn write_atomic(
        &self,
        p: &Path,
        bytes: &[u8],
        mode: Option<u32>,
    ) -> Result<(), SandboxError> {
        self.0.write_atomic(p, bytes, mode).await
    }
    async fn remove_file(&self, p: &Path) -> Result<(), SandboxError> {
        self.0.remove_file(p).await
    }
    async fn spill(&self, bytes: &[u8]) -> Result<PathBuf, SandboxError> {
        self.0.spill(bytes).await
    }
    async fn spill_stream(&self) -> Result<Option<(PathBuf, SpillWriter)>, SandboxError> {
        self.0.spill_stream().await
    }
    async fn spawn(&self, spec: SpawnSpec) -> Result<Child, SandboxError> {
        self.0.spawn(spec).await
    }
    async fn spawn_stdio(&self, spec: SpawnSpec) -> Result<Child, SandboxError> {
        self.0.spawn_stdio(spec).await
    }
    fn walker(&self, base: &Path) -> Result<ignore::WalkBuilder, SandboxError> {
        self.0.walker(base)
    }
    async fn http_get(&self, url: &Url) -> Result<HttpResponse, SandboxError> {
        self.0.http_get(url).await
    }
}
