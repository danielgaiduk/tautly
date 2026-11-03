//! Git checkpoints and user-turn undo (spec 7.11.11), on temporary git repositories made
//! here (never this project's own): snapshots through `Checkpointer`, and the engine's
//! checkpoints, `Command::Undo`, pruning and session deletion against a scripted provider.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command as Process;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tautly_core::checkpoint::{self, skip_warning};
use tautly_core::undo::{UndoError, undo};
use tautly_core::{
    CheckpointConfig, Checkpointer, CliOverrides, Command, Config, Engine, EngineBuilder, Event,
    SessionRecord, Skip, sessions,
};
use tautly_provider::{
    Catalog, MockProvider, MockTurn, ModelId, ProviderEvent, RawUsage, StopReason,
};
use tautly_sandbox::{LocalSandbox, Mode, Sandbox};
use tautly_session::SessionReader;
use tempfile::TempDir;
use tokio::sync::mpsc;

const MODEL: &str = "mock/test";
const WAIT: Duration = Duration::from_secs(30);

// ------------------------------------------------------------------------ fixtures

/// A temporary git repository (and a data home), with git run on it in isolation from
/// the machine's git configuration.
struct Repo {
    dir: TempDir,
    home: TempDir,
}

impl Repo {
    /// An empty repository (unborn `main`).
    fn new() -> Repo {
        let repo = Repo::bare_dir();
        repo.git(&["init", "-q", "-b", "main"]);
        repo
    }

    /// A directory that is not a repository.
    fn bare_dir() -> Repo {
        Repo {
            dir: tempfile::tempdir().unwrap(),
            home: tempfile::tempdir().unwrap(),
        }
    }

    /// A repository with `files` committed on `main`.
    fn with(files: &[(&str, &str)]) -> Repo {
        let repo = Repo::new();
        for (p, text) in files {
            repo.write(p, text);
        }
        repo.git(&["add", "-A"]);
        repo.git(&["commit", "-q", "-m", "init"]);
        repo
    }

    fn root(&self) -> PathBuf {
        self.dir.path().canonicalize().unwrap()
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.root().join(rel)
    }

    fn write(&self, rel: &str, text: &str) {
        let p = self.path(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    fn read(&self, rel: &str) -> Option<String> {
        std::fs::read_to_string(self.path(rel)).ok()
    }

    fn git_cmd(&self, args: &[&str]) -> Process {
        let mut cmd = Process::new("git");
        cmd.current_dir(self.root())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CEILING_DIRECTORIES", self.root().parent().unwrap())
            .args([
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args);
        cmd
    }

    /// Run git; its stdout, trimmed. Panics on failure.
    fn git(&self, args: &[&str]) -> String {
        let out = self.git_cmd(args).output().unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    /// Run git; whether it succeeded.
    fn git_ok(&self, args: &[&str]) -> bool {
        self.git_cmd(args).output().unwrap().status.success()
    }

    fn refs(&self, prefix: &str) -> Vec<String> {
        let out = self.git(&["for-each-ref", "--format=%(refname)", prefix]);
        out.lines().map(str::to_string).collect()
    }

    fn sandbox(&self) -> Arc<dyn Sandbox> {
        Arc::new(LocalSandbox::new(self.root()).unwrap())
    }

    fn checkpointer(&self, cfg: CheckpointConfig) -> Checkpointer {
        Checkpointer::new(self.sandbox(), self.root(), "s1".into(), cfg)
            .expect("a git work tree has a checkpointer")
    }

    fn builder(&self, cfg: Config, mock: &MockProvider) -> EngineBuilder {
        EngineBuilder {
            provider: Some(Arc::new(mock.clone())),
            data_home: Some(self.home.path().to_path_buf()),
            ..EngineBuilder::new(cfg, self.dir.path())
        }
    }
}

fn catalog() -> Catalog {
    Catalog::from_toml(&format!(
        r#"[models."{MODEL}"]
context_window = 200000
max_output = 8000
parallel_tools = true
reasoning = "none"
cache = "none"
images = false
edit_format = "str_replace"
"#
    ))
    .unwrap()
}

fn config() -> Config {
    let mut c = Config {
        model: ModelId::from(MODEL),
        catalog_overrides: Some(catalog()),
        ..Config::default()
    };
    c.permissions.mode = Mode::Yolo;
    c
}

fn call(id: &str, name: &str, args: Value) -> ProviderEvent {
    ProviderEvent::ToolCall {
        id: id.into(),
        name: name.into(),
        input_json: args.to_string(),
    }
}

fn bash(id: &str, command: &str) -> ProviderEvent {
    call(id, "bash", json!({ "command": command }))
}

fn write(id: &str, path: &str, content: &str) -> ProviderEvent {
    call(id, "write", json!({ "path": path, "content": content }))
}

fn tools_step(calls: Vec<ProviderEvent>) -> MockTurn {
    let mut evs = calls;
    evs.push(ProviderEvent::Done {
        stop: StopReason::ToolUse,
    });
    MockTurn::Events(evs)
}

fn text_step(text: &str) -> MockTurn {
    MockTurn::Events(vec![
        ProviderEvent::TextDelta(text.into()),
        ProviderEvent::Usage(RawUsage {
            input: 100,
            output: 10,
            cache_read: 0,
            cache_write: 0,
        }),
        ProviderEvent::Done {
            stop: StopReason::EndTurn,
        },
    ])
}

struct Run {
    engine: Engine,
    events: mpsc::Receiver<Event>,
}

fn start(b: EngineBuilder) -> Run {
    let (engine, events) = b.build().unwrap();
    Run { engine, events }
}

impl Run {
    async fn send(&self, cmd: Command) {
        self.engine.send(cmd).await.unwrap();
    }

    async fn next(&mut self) -> Event {
        tokio::time::timeout(WAIT, self.events.recv())
            .await
            .expect("timed out waiting for an event")
            .expect("event channel closed")
    }

    async fn until(&mut self, pred: impl Fn(&Event) -> bool) -> Vec<Event> {
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

    async fn turn_with(&mut self, text: &str) -> Vec<Event> {
        self.send(Command::SendMessage { text: text.into() }).await;
        self.until(|e| matches!(e, Event::TurnFinished { .. }))
            .await
    }

    /// Send `Undo` and wait for its answer (`Undone`, or the `Error`).
    async fn undo(&mut self, turn: Option<u32>, dry_run: bool) -> Event {
        self.send(Command::Undo { turn, dry_run }).await;
        let evs = self
            .until(|e| matches!(e, Event::Undone { .. } | Event::Error { .. }))
            .await;
        evs.last().unwrap().clone()
    }

    async fn fork(&mut self) -> String {
        self.fork_at(None).await
    }

    async fn fork_at(&mut self, at: Option<u32>) -> String {
        self.send(Command::Fork {
            at,
            label: Some("branch".into()),
        })
        .await;
        let evs = self
            .until(|e| matches!(e, Event::SessionChanged { .. } | Event::Error { .. }))
            .await;
        assert!(warnings(&evs).is_empty(), "{evs:?}");
        match evs.last() {
            Some(Event::SessionChanged { session_id }) => session_id.clone(),
            other => panic!("the fork failed: {other:?}"),
        }
    }

    async fn stop(mut self) {
        self.send(Command::Shutdown).await;
        let closed =
            tokio::time::timeout(WAIT, async { while self.events.recv().await.is_some() {} }).await;
        assert!(closed.is_ok(), "the engine did not stop");
    }
}

fn records_at(path: &Path) -> Vec<SessionRecord> {
    let reader = SessionReader::<SessionRecord>::open(path).unwrap();
    reader.records().map(|r| r.unwrap().1).collect()
}

fn warnings(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::Warning { message } => Some(message.clone()),
            _ => None,
        })
        .collect()
}

fn checkpoints(recs: &[SessionRecord]) -> Vec<(u32, String, u32)> {
    recs.iter()
        .filter_map(|r| match r {
            SessionRecord::Checkpoint {
                n, commit, turn, ..
            } => Some((*n, commit.clone(), *turn)),
            _ => None,
        })
        .collect()
}

fn executable(path: &Path, text: &str) {
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

// ----------------------------------------------------------------------- snapshots

#[tokio::test]
async fn checkpoint_leaves_index_head_stash_and_branches_untouched() {
    let repo = Repo::with(&[("a.txt", "a\n"), ("b.txt", "b\n")]);
    repo.git(&["branch", "side"]);
    repo.write("a.txt", "stashed\n");
    repo.git(&["stash", "push", "-q", "-m", "mine"]);
    repo.write("b.txt", "staged\n");
    repo.git(&["add", "b.txt"]);
    repo.write("a.txt", "unstaged\n");
    repo.write("untracked.txt", "u\n");
    let index = std::fs::read(repo.path(".git/index")).unwrap();
    let head = repo.git(&["rev-parse", "HEAD"]);
    let stash = repo.git(&["stash", "list"]);
    let heads = repo.git(&["for-each-ref", "refs/heads"]);
    let status = repo.git(&["status", "--porcelain"]);

    let mut cp = repo.checkpointer(CheckpointConfig::default());
    let made = cp.snapshot(0, true).await.unwrap();
    assert!(made.dirty);
    assert_eq!(made.head.as_deref(), Some(head.as_str()));

    assert_eq!(std::fs::read(repo.path(".git/index")).unwrap(), index);
    assert_eq!(repo.git(&["rev-parse", "HEAD"]), head);
    assert_eq!(repo.git(&["stash", "list"]), stash);
    assert_eq!(repo.git(&["for-each-ref", "refs/heads"]), heads);
    assert_eq!(repo.git(&["status", "--porcelain"]), status);
    assert_eq!(repo.read("a.txt").as_deref(), Some("unstaged\n"));
    // Only its own ref was added; the snapshot holds the work tree as it is.
    assert_eq!(
        repo.refs("refs/tautly/"),
        std::slice::from_ref(&made.refname)
    );
    assert_eq!(
        repo.git(&["show", &format!("{}:a.txt", made.commit)]),
        "unstaged"
    );
    assert_eq!(
        repo.git(&["show", &format!("{}:b.txt", made.commit)]),
        "staged"
    );
}

#[tokio::test]
async fn checkpoint_includes_untracked_and_excludes_ignored() {
    let repo = Repo::with(&[("a.txt", "a\n"), (".gitignore", "target/\n*.log\n")]);
    repo.write("target/x", "built\n");
    repo.write("debug.log", "log\n");
    repo.write("new/u.txt", "u\n");
    let mut cp = repo.checkpointer(CheckpointConfig::default());
    let made = cp.snapshot(0, true).await.unwrap();
    let files = repo.git(&["ls-tree", "-r", "--name-only", &made.commit]);
    let files: Vec<&str> = files.lines().collect();
    assert_eq!(files, [".gitignore", "a.txt", "new/u.txt"]);
}

#[tokio::test]
async fn unborn_head_repo_checkpoints() {
    let repo = Repo::new();
    repo.write("f.txt", "f\n");
    let mut cp = repo.checkpointer(CheckpointConfig::default());
    let made = cp.snapshot(0, true).await.unwrap();
    assert_eq!(made.head, None);
    assert!(made.dirty);
    let commit = repo.git(&["cat-file", "-p", &made.commit]);
    assert!(!commit.contains("\nparent "), "{commit}");
    assert_eq!(
        repo.git(&["ls-tree", "-r", "--name-only", &made.commit]),
        "f.txt"
    );
    // Still unborn.
    assert!(!repo.git_ok(&["rev-parse", "-q", "--verify", "HEAD"]));
}

#[tokio::test]
async fn hooks_filters_and_fsmonitor_never_run() {
    let repo = Repo::with(&[("a.txt", "a\n"), ("p.dat", "p\n")]);
    let marker = repo.home.path().join("marker");
    let touch = format!("touch '{}'", marker.display());
    let hook = format!("#!/bin/sh\n{touch}\n");
    let hooks = repo.path(".git/hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    for name in [
        "pre-commit",
        "post-commit",
        "post-checkout",
        "post-index-change",
        "reference-transaction",
        "pre-auto-gc",
    ] {
        executable(&hooks.join(name), &hook);
    }
    let fsmonitor = repo.home.path().join("fsmonitor.sh");
    executable(&fsmonitor, &hook);
    repo.git(&["config", "core.fsmonitor", fsmonitor.to_str().unwrap()]);
    // Filter drivers of every kind (clean, smudge, a long-running process), required,
    // declared for every file, as a downloaded repository could ship them.
    let filter = format!("{touch}; cat");
    for (key, value) in [
        ("filter.evil.clean", filter.as_str()),
        ("filter.evil.smudge", filter.as_str()),
        ("filter.evil.required", "true"),
        ("filter.Proc.process", touch.as_str()),
        ("filter.Proc.required", "true"),
    ] {
        repo.git(&["config", key, value]);
    }
    repo.write(".gitattributes", "*.txt filter=evil\n*.dat filter=Proc\n");

    let mut cp = repo.checkpointer(CheckpointConfig::default());
    let first = cp.snapshot(0, true).await.unwrap();
    repo.write("a.txt", "changed\n");
    repo.write("b.txt", "new\n");
    repo.write("p.dat", "changed\n");
    let second = cp.snapshot(1, true).await.unwrap();
    assert_ne!(first.commit, second.commit);
    // The snapshot holds the work tree's bytes (no clean filter ran).
    assert_eq!(
        repo.git(&["cat-file", "-p", &format!("{}:a.txt", second.commit)]),
        "changed"
    );
    cp.backup().await.unwrap();
    // A restore (undo checks files out of a checkpoint) runs no smudge or process filter.
    let mock = MockProvider::new(vec![
        tools_step(vec![bash(
            "b1",
            "printf 'bash\\n' > a.txt && printf 'bash\\n' > p.dat && touch c.txt",
        )]),
        text_step("done"),
    ]);
    let mut run = start(repo.builder(config(), &mock));
    run.turn_with("go").await;
    let answer = run.undo(None, false).await;
    run.stop().await;
    assert!(matches!(answer, Event::Undone { .. }), "{answer:?}");
    assert_eq!(repo.read("a.txt").as_deref(), Some("changed\n"));
    assert_eq!(repo.read("p.dat").as_deref(), Some("changed\n"));
    assert_eq!(repo.read("c.txt"), None);
    assert!(!marker.exists(), "a hook, filter or the fsmonitor ran");

    // Control: the same repository runs them for git itself.
    repo.git_ok(&["update-ref", "refs/heads/control", "HEAD"]);
    assert!(marker.exists(), "the hooks were not set up to run");
    std::fs::remove_file(&marker).unwrap();
    repo.git_ok(&["add", "a.txt"]);
    assert!(marker.exists(), "the filters were not set up to run");
}

#[tokio::test]
async fn core_worktree_cannot_redirect_checkpoints_or_undo() {
    let repo = Repo::with(&[("a.txt", "a\n")]);
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("victim.txt"), "victim\n").unwrap();
    repo.git(&["config", "core.worktree", outside.path().to_str().unwrap()]);

    let mut cp = repo.checkpointer(CheckpointConfig::default());
    let made = cp.snapshot(0, true).await.unwrap();
    assert_eq!(
        repo.git(&["ls-tree", "-r", "--name-only", &made.commit]),
        "a.txt"
    );

    let mock = MockProvider::new(vec![
        tools_step(vec![bash("b1", "printf 'b\\n' > a.txt && touch new.txt")]),
        text_step("done"),
    ]);
    let mut run = start(repo.builder(config(), &mock));
    run.turn_with("go").await;
    let answer = run.undo(None, false).await;
    run.stop().await;
    assert!(
        matches!(&answer, Event::Undone { restored, deleted, .. }
            if restored == &["a.txt"] && deleted == &["new.txt"]),
        "{answer:?}"
    );
    assert_eq!(repo.read("a.txt").as_deref(), Some("a\n"));
    assert_eq!(repo.read("new.txt"), None);
    let left: Vec<_> = std::fs::read_dir(outside.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(left, ["victim.txt"]);
    assert_eq!(
        std::fs::read_to_string(outside.path().join("victim.txt")).unwrap(),
        "victim\n"
    );
}

#[tokio::test]
async fn unchanged_tree_reuses_previous_checkpoint() {
    let repo = Repo::with(&[("a.txt", "a\n")]);
    let mut cp = repo.checkpointer(CheckpointConfig::default());
    let first = cp.snapshot(0, true).await.unwrap();
    assert!(!first.reused);
    // Nothing changed: the tree is the same.
    let again = cp.snapshot(0, true).await.unwrap();
    assert!(again.reused);
    assert_eq!((again.n, &again.commit), (first.n, &first.commit));
    // The write generation and the watcher say nothing changed.
    let fast = cp.snapshot(0, false).await.unwrap();
    assert!(fast.reused);
    assert_eq!(fast.n, first.n);
    // A change makes a new checkpoint on top of the previous one.
    repo.write("a.txt", "b\n");
    let next = cp.snapshot(1, true).await.unwrap();
    assert!(!next.reused);
    assert_eq!(next.n, first.n + 1);
    let parent = repo.git(&["rev-parse", &format!("{}^", next.commit)]);
    assert_eq!(parent, first.commit);
    assert_eq!(repo.refs("refs/tautly/").len(), 2);
}

#[tokio::test]
async fn timeout_skips_with_warning() {
    let repo = Repo::with(&[("a.txt", "a\n")]);
    let pid_file = repo.home.path().join("sleep.pid");
    let shim = repo.home.path().join("git");
    executable(
        &shim,
        &format!(
            "#!/bin/sh\nsleep 30 &\necho $! > '{}'\nwait\n",
            pid_file.display()
        ),
    );
    let cfg = CheckpointConfig {
        timeout_ms: 300,
        ..CheckpointConfig::default()
    };
    let mut cp = repo.checkpointer(cfg).with_git(&shim);
    let began = Instant::now();
    let skipped = cp.snapshot(0, true).await;
    assert_eq!(skipped, Err(Skip::Timeout));
    assert!(
        began.elapsed() < Duration::from_secs(3),
        "{:?}",
        began.elapsed()
    );
    assert!(
        skip_warning(&Skip::Timeout).starts_with("checkpoint skipped: "),
        "{}",
        skip_warning(&Skip::Timeout)
    );
    // The shim's child went with it.
    let pid: i32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let end = Instant::now() + Duration::from_secs(2);
    // SAFETY: signal 0 only checks for existence.
    while unsafe { libc::kill(pid, 0) } == 0 && Instant::now() < end {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_ne!(
        unsafe { libc::kill(pid, 0) },
        0,
        "the shim's sleep survived"
    );
}

#[tokio::test]
async fn untracked_limits_skip_with_warning() {
    let repo = Repo::with(&[("a.txt", "a\n")]);
    repo.write("u1.txt", "1\n");
    repo.write("u2.txt", "2\n");
    let mut cp = repo.checkpointer(CheckpointConfig {
        max_untracked_files: 1,
        ..CheckpointConfig::default()
    });
    assert!(matches!(
        cp.snapshot(0, true).await,
        Err(Skip::TooManyUntracked { files: 2, .. })
    ));
    repo.write("big.bin", &"x".repeat(2 * 1024 * 1024));
    let mut cp = repo.checkpointer(CheckpointConfig {
        max_untracked_mb: 1,
        ..CheckpointConfig::default()
    });
    match cp.snapshot(0, true).await {
        Err(Skip::TooManyUntracked { files: 3, bytes }) => {
            assert!(bytes > 2 * 1024 * 1024)
        }
        other => panic!("{other:?}"),
    }
    assert!(repo.refs("refs/tautly/").is_empty());

    // In a session: a warning, and the log says so.
    let mut cfg = config();
    cfg.checkpoints.max_untracked_files = 1;
    let mock = MockProvider::new(vec![text_step("hi")]);
    let mut run = start(repo.builder(cfg, &mock));
    let events = run.turn_with("hello").await;
    let path = run.engine.session_path().to_path_buf();
    run.stop().await;
    assert!(
        warnings(&events)
            .iter()
            .any(|w| w.starts_with("checkpoint skipped: ")),
        "{events:?}"
    );
    let recs = records_at(&path);
    assert!(
        recs.iter()
            .any(|r| matches!(r, SessionRecord::CheckpointSkipped { .. })),
        "{recs:?}"
    );
    assert!(checkpoints(&recs).is_empty());
}

#[test]
fn project_layer_cannot_disable_checkpoints() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    let user = dir.path().join("user/config.toml");
    std::fs::create_dir_all(root.join(".tautly")).unwrap();
    std::fs::write(
        root.join(".tautly/config.toml"),
        "[checkpoints]\nenabled = false\nmax_untracked_files = 1\n",
    )
    .unwrap();
    let loaded = Config::load_from(Some(&user), &root, CliOverrides::default()).unwrap();
    assert_eq!(loaded.config.checkpoints, CheckpointConfig::default());
    assert!(
        loaded.warnings.iter().any(|w| w.contains("checkpoints")),
        "{:?}",
        loaded.warnings
    );
    // The user's own file and flag may.
    std::fs::create_dir_all(user.parent().unwrap()).unwrap();
    std::fs::write(&user, "[checkpoints]\nkeep_days = 3\n").unwrap();
    let cli = CliOverrides {
        checkpoints: Some(false),
        ..CliOverrides::default()
    };
    let cfg = Config::load_from(Some(&user), &root, cli).unwrap().config;
    assert!(!cfg.checkpoints.enabled);
    assert_eq!(cfg.checkpoints.keep_days, 3);
}

#[tokio::test]
async fn checkpoints_at_session_start_and_turn_end() {
    let repo = Repo::with(&[("a.txt", "a\n")]);
    let mock = MockProvider::new(vec![
        tools_step(vec![write("w1", "a.txt", "edited\n")]),
        text_step("done"),
    ]);
    let mut run = start(repo.builder(config(), &mock));
    run.turn_with("edit").await;
    let (id, path) = run.engine.current_session();
    run.stop().await;
    let recs = records_at(&path);
    let turn_start = recs
        .iter()
        .position(|r| matches!(r, SessionRecord::TurnStart { .. }))
        .unwrap();
    let turn_end = recs
        .iter()
        .position(|r| matches!(r, SessionRecord::TurnEnd { .. }))
        .unwrap();
    let at = |i: usize| match &recs[i] {
        SessionRecord::Checkpoint {
            n,
            commit,
            turn,
            step,
            ..
        } => Some((*n, commit.clone(), *turn, *step)),
        _ => None,
    };
    // Taken in the background at session start: logged before the turn's first tool
    // call, perhaps after its `TurnStart`.
    let first_call = recs
        .iter()
        .position(|r| matches!(r, SessionRecord::ToolCall { .. }))
        .unwrap();
    assert!(turn_start < first_call);
    let start_cp = (0..first_call)
        .filter_map(at)
        .find(|c| (c.2, c.3) == (0, 0))
        .expect("no session-start checkpoint");
    let end_cp = at(turn_end - 1).expect("no checkpoint right before TurnEnd");
    assert_eq!(end_cp.2, 1);
    assert!(end_cp.0 > start_cp.0, "the edit changed the tree");
    assert_eq!(
        repo.git(&["show", &format!("{}:a.txt", end_cp.1)]),
        "edited"
    );
    for n in [start_cp.0, end_cp.0] {
        let name = format!("refs/tautly/checkpoints/{id}/{n}");
        assert_eq!(repo.refs(&name), std::slice::from_ref(&name));
    }
}

#[tokio::test]
async fn checkpoint_taken_before_bash() {
    let repo = Repo::with(&[("a.txt", "a\n")]);
    let mock = MockProvider::new(vec![
        tools_step(vec![bash("b1", "printf 'by bash\\n' > a.txt")]),
        text_step("done"),
    ]);
    let mut run = start(repo.builder(config(), &mock));
    run.turn_with("go").await;
    let (_, path) = run.engine.current_session();
    run.stop().await;
    let recs = records_at(&path);
    let result = recs
        .iter()
        .position(|r| matches!(r, SessionRecord::ToolResult { id, .. } if id == "b1"))
        .unwrap();
    let call = recs
        .iter()
        .position(|r| matches!(r, SessionRecord::ToolCall { id, .. } if id == "b1"))
        .unwrap();
    let before = recs[call..result]
        .iter()
        .find_map(|r| match r {
            SessionRecord::Checkpoint { commit, turn, .. } => Some((commit.clone(), *turn)),
            _ => None,
        })
        .expect("no checkpoint between the call and its result");
    assert_eq!(before.1, 1);
    assert_eq!(repo.git(&["show", &format!("{}:a.txt", before.0)]), "a");
}

// ---------------------------------------------------------------------------- undo

#[tokio::test]
async fn non_git_dir_has_no_checkpointer_and_undo_notes_it() {
    let dir = Repo::bare_dir();
    let sb = dir.sandbox();
    assert!(Checkpointer::new(sb, dir.root(), "s".into(), CheckpointConfig::default()).is_none());
    let mock = MockProvider::new(vec![
        tools_step(vec![
            write("w1", "a.txt", "by tool\n"),
            bash("b1", "printf 'by bash\\n' > made.txt"),
        ]),
        text_step("done"),
    ]);
    let mut run = start(dir.builder(config(), &mock));
    run.turn_with("go").await;
    match run.undo(None, false).await {
        Event::Undone {
            turn,
            restored,
            deleted,
            dry_run,
            backup_ref,
            note,
        } => {
            assert_eq!(turn, 1);
            assert!(restored.is_empty(), "{restored:?}");
            assert_eq!(deleted, ["a.txt"]);
            assert!(!dry_run);
            assert_eq!(backup_ref, None);
            assert_eq!(
                note.as_deref(),
                Some("command side effects not undone (not a git repository)")
            );
        }
        other => panic!("{other:?}"),
    }
    run.stop().await;
    assert_eq!(dir.read("a.txt"), None);
    assert_eq!(dir.read("made.txt").as_deref(), Some("by bash\n"));
}

/// A repository with a tracked file and an ignored build directory.
fn build_repo() -> Repo {
    let repo = Repo::with(&[
        ("tracked.txt", "orig\n"),
        (".gitignore", "target/\n"),
        ("keep.txt", "keep\n"),
    ]);
    repo.write("target/x", "old\n");
    repo
}

fn bash_turn_mock() -> MockProvider {
    MockProvider::new(vec![
        tools_step(vec![bash(
            "b1",
            "printf 'changed\\n' > tracked.txt && touch new.txt && printf 'built\\n' > target/x",
        )]),
        text_step("done"),
    ])
}

#[tokio::test]
async fn undo_reverts_bash_side_effects() {
    let repo = build_repo();
    let mut run = start(repo.builder(config(), &bash_turn_mock()));
    run.turn_with("build").await;
    assert_eq!(repo.read("tracked.txt").as_deref(), Some("changed\n"));
    let answer = run.undo(None, false).await;
    let (_, path) = run.engine.current_session();
    run.stop().await;
    let Event::Undone {
        turn,
        restored,
        deleted,
        dry_run,
        backup_ref,
        note,
    } = answer
    else {
        panic!("{answer:?}")
    };
    assert_eq!(turn, 1);
    assert_eq!(restored, ["tracked.txt"]);
    assert_eq!(deleted, ["new.txt"]);
    assert!(!dry_run);
    assert_eq!(
        note.as_deref(),
        Some("ignored files and submodules are not restored")
    );
    assert_eq!(repo.read("tracked.txt").as_deref(), Some("orig\n"));
    assert_eq!(repo.read("new.txt"), None);
    assert_eq!(repo.read("target/x").as_deref(), Some("built\n"));
    assert_eq!(repo.read("keep.txt").as_deref(), Some("keep\n"));
    let backup = backup_ref.expect("a backup ref");
    assert_eq!(repo.refs(&backup), std::slice::from_ref(&backup));
    let recs = records_at(&path);
    assert!(
        recs.iter().any(|r| matches!(
            r,
            SessionRecord::Undone { turn: 1, backup_ref: Some(b), .. } if *b == backup
        )),
        "{recs:?}"
    );
}

#[tokio::test]
async fn undo_combines_preimages_and_checkpoints_in_reverse() {
    let repo = Repo::with(&[("A.txt", "a0\n"), ("B.txt", "b0\n")]);
    let mock = MockProvider::new(vec![
        tools_step(vec![write("w1", "A.txt", "a1\n")]),
        tools_step(vec![bash(
            "b1",
            "printf 'a2\\n' > A.txt && printf 'b1\\n' > B.txt",
        )]),
        tools_step(vec![write("w2", "B.txt", "b2\n")]),
        text_step("done"),
    ]);
    let mut run = start(repo.builder(config(), &mock));
    run.turn_with("go").await;
    assert_eq!(repo.read("A.txt").as_deref(), Some("a2\n"));
    assert_eq!(repo.read("B.txt").as_deref(), Some("b2\n"));
    let answer = run.undo(None, false).await;
    run.stop().await;
    let Event::Undone {
        mut restored,
        deleted,
        ..
    } = answer
    else {
        panic!("{answer:?}")
    };
    restored.sort();
    assert_eq!(restored, ["A.txt", "B.txt"]);
    assert!(deleted.is_empty());
    assert_eq!(repo.read("A.txt").as_deref(), Some("a0\n"));
    assert_eq!(repo.read("B.txt").as_deref(), Some("b0\n"));
}

#[tokio::test]
async fn undo_dry_run_changes_nothing_and_lists_paths() {
    let repo = build_repo();
    let mut run = start(repo.builder(config(), &bash_turn_mock()));
    run.turn_with("build").await;
    let refs = repo.refs("refs/tautly/");
    let answer = run.undo(None, true).await;
    let (_, path) = run.engine.current_session();
    run.stop().await;
    let Event::Undone {
        turn,
        restored,
        deleted,
        dry_run,
        backup_ref,
        ..
    } = answer
    else {
        panic!("{answer:?}")
    };
    assert_eq!(turn, 1);
    assert!(dry_run);
    assert_eq!(restored, ["tracked.txt"]);
    assert_eq!(deleted, ["new.txt"]);
    assert_eq!(backup_ref, None);
    assert_eq!(repo.read("tracked.txt").as_deref(), Some("changed\n"));
    assert_eq!(repo.read("new.txt").as_deref(), Some(""));
    assert_eq!(repo.refs("refs/tautly/"), refs);
    assert!(
        !records_at(&path)
            .iter()
            .any(|r| matches!(r, SessionRecord::Undone { .. }))
    );
}

#[tokio::test]
async fn undo_takes_backup_ref_first() {
    let repo = build_repo();
    let mut run = start(repo.builder(config(), &bash_turn_mock()));
    run.turn_with("build").await;
    // The user writes a note of their own meanwhile (no file the turn changed).
    repo.write("notes.txt", "mine\n");
    let answer = run.undo(None, false).await;
    run.stop().await;
    let Event::Undone { backup_ref, .. } = answer else {
        panic!("{answer:?}")
    };
    let backup = backup_ref.unwrap();
    assert!(
        backup.contains("/undo-"),
        "{backup} is not an undo backup ref"
    );
    // Both the turn's work and the user's note can be had back from it.
    assert_eq!(
        repo.git(&["show", &format!("{backup}:tracked.txt")]),
        "changed"
    );
    assert_eq!(repo.git(&["show", &format!("{backup}:notes.txt")]), "mine");
    assert_eq!(repo.read("notes.txt").as_deref(), Some("mine\n"));
    assert_eq!(repo.read("tracked.txt").as_deref(), Some("orig\n"));
}

#[tokio::test]
async fn undo_refuses_a_file_changed_after_the_turn() {
    let repo = build_repo();
    let mut run = start(repo.builder(config(), &bash_turn_mock()));
    run.turn_with("build").await;
    repo.write("tracked.txt", "human\n");
    let answer = run.undo(None, false).await;
    run.stop().await;
    assert!(
        matches!(&answer, Event::Error { message, .. } if message.contains("tracked.txt")),
        "{answer:?}"
    );
    assert_eq!(repo.read("tracked.txt").as_deref(), Some("human\n"));
    assert_eq!(repo.read("new.txt").as_deref(), Some(""));
}

#[tokio::test]
async fn undo_never_deletes_a_file_ignored_before_the_turn() {
    let repo = Repo::with(&[("tracked.txt", "orig\n"), (".gitignore", ".env\n")]);
    repo.write(".env", "SECRET=1\n");
    repo.write("notes.txt", "mine\n");
    // The turn stops ignoring `.env`, starts ignoring `notes.txt` and adds a file.
    let mock = MockProvider::new(vec![
        tools_step(vec![bash(
            "b1",
            "printf 'notes.txt\\n' > .gitignore && printf 'changed\\n' > tracked.txt \
             && touch new.txt",
        )]),
        text_step("done"),
    ]);
    let mut run = start(repo.builder(config(), &mock));
    run.turn_with("go").await;
    let answer = run.undo(None, false).await;
    run.stop().await;
    let Event::Undone {
        mut restored,
        deleted,
        note,
        ..
    } = answer
    else {
        panic!("{answer:?}")
    };
    restored.sort();
    assert_eq!(restored, [".gitignore", "tracked.txt"]);
    assert_eq!(deleted, ["new.txt"]);
    let note = note.unwrap();
    assert!(note.contains(".env"), "{note}");
    assert_eq!(repo.read(".gitignore").as_deref(), Some(".env\n"));
    assert_eq!(repo.read("tracked.txt").as_deref(), Some("orig\n"));
    assert_eq!(repo.read("new.txt"), None);
    assert_eq!(repo.read(".env").as_deref(), Some("SECRET=1\n"));
    assert_eq!(repo.read("notes.txt").as_deref(), Some("mine\n"));
}

#[tokio::test]
async fn undo_refuses_a_file_the_turn_ignored_and_someone_changed() {
    let repo = Repo::with(&[("tracked.txt", "orig\n")]);
    repo.write("notes.txt", "mine\n");
    let mock = MockProvider::new(vec![
        tools_step(vec![bash(
            "b1",
            "printf 'notes.txt\\n' > .gitignore && printf 'changed\\n' > tracked.txt",
        )]),
        text_step("done"),
    ]);
    let mut run = start(repo.builder(config(), &mock));
    run.turn_with("go").await;
    // Ignored now, so no checkpoint shows this edit.
    repo.write("notes.txt", "human\n");
    let answer = run.undo(None, false).await;
    run.stop().await;
    assert!(
        matches!(&answer, Event::Error { message, .. } if message.contains("notes.txt")),
        "{answer:?}"
    );
    assert_eq!(repo.read("notes.txt").as_deref(), Some("human\n"));
    assert_eq!(repo.read("tracked.txt").as_deref(), Some("changed\n"));
}

#[tokio::test]
async fn undo_removes_links_the_turn_made_without_following_them() {
    let repo = Repo::with(&[("src/a.txt", "a\n"), ("keep.txt", "keep\n")]);
    let victim = repo.home.path().join("victim.txt");
    std::fs::write(&victim, "victim\n").unwrap();
    let command = format!(
        "ln -s /tmp out && ln -s src lib && ln -s missing dangling && rm keep.txt \
         && ln -s '{}' keep.txt",
        victim.display()
    );
    let mock = MockProvider::new(vec![
        tools_step(vec![bash("b1", &command)]),
        text_step("done"),
    ]);
    let mut run = start(repo.builder(config(), &mock));
    run.turn_with("go").await;
    let answer = run.undo(None, false).await;
    run.stop().await;
    let Event::Undone {
        restored,
        mut deleted,
        ..
    } = answer
    else {
        panic!("{answer:?}")
    };
    deleted.sort();
    assert_eq!(deleted, ["dangling", "lib", "out"]);
    assert_eq!(restored, ["keep.txt"]);
    for link in ["out", "lib", "dangling"] {
        assert!(
            std::fs::symlink_metadata(repo.path(link)).is_err(),
            "{link} is still there"
        );
    }
    let keep = std::fs::symlink_metadata(repo.path("keep.txt")).unwrap();
    assert!(keep.file_type().is_file());
    assert_eq!(repo.read("keep.txt").as_deref(), Some("keep\n"));
    assert_eq!(std::fs::read_to_string(&victim).unwrap(), "victim\n");
    assert_eq!(repo.read("src/a.txt").as_deref(), Some("a\n"));
    assert!(Path::new("/tmp").exists());
}

#[tokio::test]
async fn failed_undo_logs_only_the_files_it_changed() {
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        return; // root ignores directory permissions
    }
    let repo = Repo::with(&[("a.txt", "a\n")]);
    let mock = MockProvider::new(vec![
        tools_step(vec![bash("b1", "mkdir d e && touch d/new1.txt e/new2.txt")]),
        text_step("done"),
    ]);
    let mut run = start(repo.builder(config(), &mock));
    run.turn_with("go").await;
    let locked = repo.path("e");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();
    let answer = run.undo(None, false).await;
    let (_, path) = run.engine.current_session();
    run.stop().await;
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(matches!(answer, Event::Error { .. }), "{answer:?}");
    assert_eq!(repo.read("d/new1.txt"), None);
    assert_eq!(repo.read("e/new2.txt").as_deref(), Some(""));
    let logged: Vec<Vec<String>> = records_at(&path)
        .into_iter()
        .filter_map(|r| match r {
            SessionRecord::Undone { files, .. } => {
                Some(files.iter().map(|f| f.path.as_str().to_string()).collect())
            }
            _ => None,
        })
        .collect();
    assert_eq!(logged, [["d/new1.txt"]]);
}

#[tokio::test]
async fn undo_respects_the_untracked_limits() {
    let repo = Repo::with(&[("a.txt", "a\n")]);
    let mut cfg = config();
    cfg.checkpoints.max_untracked_files = 3;
    let mock = MockProvider::new(vec![
        tools_step(vec![bash("b1", "printf 'b\\n' > a.txt")]),
        text_step("done"),
    ]);
    let mut run = start(repo.builder(cfg, &mock));
    run.turn_with("go").await;
    for i in 0..5 {
        repo.write(&format!("u{i}.txt"), "u\n");
    }
    let answer = run.undo(None, false).await;
    run.stop().await;
    assert!(
        matches!(&answer, Event::Error { message, .. } if message.contains("untracked")),
        "{answer:?}"
    );
    assert_eq!(repo.read("a.txt").as_deref(), Some("b\n"));
    assert!(
        repo.refs("refs/tautly/")
            .iter()
            .all(|r| !r.contains("/undo-"))
    );
}

#[tokio::test]
async fn undo_default_is_last_user_turn_and_cli_uses_same_numbering() {
    let repo = Repo::with(&[("one.txt", "1\n"), ("two.txt", "2\n")]);
    let mock = MockProvider::new(vec![
        tools_step(vec![bash("b1", "printf 'one!\\n' > one.txt")]),
        text_step("first"),
        tools_step(vec![bash("b2", "printf 'two!\\n' > two.txt")]),
        text_step("second"),
    ]);
    let mut run = start(repo.builder(config(), &mock));
    run.turn_with("first").await;
    run.turn_with("second").await;
    let answer = run.undo(None, true).await;
    assert!(
        matches!(&answer, Event::Undone { turn: 2, restored, .. } if restored == &["two.txt"]),
        "{answer:?}"
    );
    let (id, path) = run.engine.current_session();
    run.stop().await;

    // The offline undo (`tautly undo [--turn N]`) counts the same turns.
    let reader = SessionReader::<SessionRecord>::open(&path).unwrap();
    let sb = repo.sandbox();
    let mut cp = Checkpointer::new(
        Arc::clone(&sb),
        repo.root(),
        id,
        CheckpointConfig::default(),
    )
    .unwrap();
    let last = undo(&reader, sb.as_ref(), Some(&mut cp), None, true)
        .await
        .unwrap();
    assert_eq!(last.turn, 2);
    assert_eq!(last.restored, [repo.path("two.txt")]);
    let first = undo(&reader, sb.as_ref(), Some(&mut cp), Some(1), false)
        .await
        .unwrap();
    assert_eq!(first.turn, 1);
    assert_eq!(first.restored, [repo.path("one.txt")]);
    assert!(first.backup_ref.is_some());
    assert_eq!(repo.read("one.txt").as_deref(), Some("1\n"));
    assert_eq!(repo.read("two.txt").as_deref(), Some("two!\n"));
    assert!(matches!(
        undo(&reader, sb.as_ref(), Some(&mut cp), Some(3), true).await,
        Err(UndoError::NoSuchTurn { turn: 3, turns: 2 })
    ));
}

// -------------------------------------------------------------- pruning and deletion

/// A commit dated `days_ago`, at `refname`.
fn dated_ref(repo: &Repo, refname: &str, days_ago: u64) {
    let tree = repo.git(&["write-tree"]);
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        - days_ago * 24 * 3600;
    let date = format!("{secs} +0000");
    let out = repo
        .git_cmd(&["commit-tree", &tree, "-m", "old"])
        .env("GIT_COMMITTER_DATE", &date)
        .env("GIT_AUTHOR_DATE", &date)
        .output()
        .unwrap();
    assert!(out.status.success());
    let commit = String::from_utf8(out.stdout).unwrap().trim().to_string();
    repo.git(&["update-ref", refname, &commit]);
}

#[tokio::test]
async fn refs_older_than_keep_days_pruned_at_engine_start() {
    let repo = Repo::with(&[("a.txt", "a\n")]);
    dated_ref(&repo, "refs/tautly/checkpoints/old/1", 30);
    dated_ref(&repo, "refs/tautly/checkpoints/recent/1", 2);
    dated_ref(&repo, "refs/heads/old-branch", 30);
    let mock = MockProvider::new(vec![text_step("hi")]);
    let mut run = start(repo.builder(config(), &mock));
    run.turn_with("hello").await;
    let end = Instant::now() + WAIT;
    while !repo.refs("refs/tautly/checkpoints/old/").is_empty() {
        assert!(Instant::now() < end, "the old ref was never pruned");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    run.stop().await;
    assert_eq!(
        repo.refs("refs/tautly/checkpoints/recent/"),
        ["refs/tautly/checkpoints/recent/1"]
    );
    assert_eq!(
        repo.refs("refs/heads/old-branch"),
        ["refs/heads/old-branch"]
    );

    // Directly: what is pruned is counted.
    dated_ref(&repo, "refs/tautly/checkpoints/old/2", 30);
    let sb = repo.sandbox();
    assert_eq!(
        checkpoint::prune(sb.as_ref(), &repo.root(), 14)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        checkpoint::prune(sb.as_ref(), &repo.root(), 1)
            .await
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn sessions_delete_removes_log_and_refs_keeps_shared_blobs() {
    let repo = Repo::with(&[("a.txt", "a\n")]);
    let mock = MockProvider::new(vec![
        tools_step(vec![write("w1", "a.txt", "edited\n")]),
        text_step("done"),
    ]);
    let mut run = start(repo.builder(config(), &mock));
    run.turn_with("edit").await;
    let (id, path) = run.engine.current_session();
    run.stop().await;
    let blobs = path.parent().unwrap().join("blobs");
    let count = |dir: &Path| walk(dir).len();
    let before = count(&blobs);
    assert!(before > 0);
    assert!(
        !repo
            .refs(&format!("refs/tautly/checkpoints/{id}/"))
            .is_empty()
    );

    let sb = repo.sandbox();
    let deleted = sessions::delete(repo.home.path(), sb.as_ref(), &id)
        .await
        .unwrap();
    assert_eq!(deleted.log, path);
    assert!(deleted.refs >= 2);
    assert!(!path.exists());
    assert!(
        repo.refs(&format!("refs/tautly/checkpoints/{id}/"))
            .is_empty()
    );
    assert_eq!(count(&blobs), before);
    // A second delete finds nothing.
    assert!(
        sessions::delete(repo.home.path(), sb.as_ref(), &id)
            .await
            .is_err()
    );
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = entry.path();
        if p.is_dir() {
            out.extend(walk(&p));
        } else {
            out.push(p);
        }
    }
    out
}

#[tokio::test]
async fn adopted_refs_survive_parent_delete() {
    let repo = Repo::with(&[("a.txt", "a\n")]);
    let mock = MockProvider::new(vec![
        tools_step(vec![bash("b1", "printf 'b\\n' > a.txt")]),
        text_step("done"),
    ]);
    let mut run = start(repo.builder(config(), &mock));
    run.turn_with("go").await;
    let (parent, _) = run.engine.current_session();
    let child = run.fork().await;
    let (_, child_path) = run.engine.current_session();
    run.stop().await;
    let parent_refs = repo.refs(&format!("refs/tautly/checkpoints/{parent}/"));
    assert!(parent_refs.len() >= 2, "{parent_refs:?}");

    let sb = repo.sandbox();
    sessions::delete(repo.home.path(), sb.as_ref(), &parent)
        .await
        .unwrap();
    assert!(
        repo.refs(&format!("refs/tautly/checkpoints/{parent}/"))
            .is_empty()
    );
    // Every checkpoint the branch copied has a ref of the branch's own.
    for (n, commit, _) in checkpoints(&records_at(&child_path)) {
        let name = format!("refs/tautly/checkpoints/{child}/{n}");
        assert_eq!(repo.git(&["rev-parse", &name]), commit);
        assert!(repo.git_ok(&["cat-file", "-e", &format!("{commit}^{{commit}}")]));
    }
}

#[tokio::test]
async fn fork_at_an_earlier_step_reuses_only_checkpoints_it_has_refs_for() {
    let repo = Repo::with(&[("a.txt", "a\n")]);
    let mock = MockProvider::new(vec![
        tools_step(vec![bash("b1", "printf '1\\n' > a.txt")]),
        text_step("first"),
        tools_step(vec![bash("b2", "printf '2\\n' > a.txt")]),
        text_step("second"),
        text_step("on the branch"),
    ]);
    let mut run = start(repo.builder(config(), &mock));
    run.turn_with("first").await;
    run.turn_with("second").await;
    let (_, parent_path) = run.engine.current_session();
    let recs = records_at(&parent_path);
    // The step of turn 1's last checkpoint (right before its `TurnEnd`).
    let end = recs
        .iter()
        .position(|r| matches!(r, SessionRecord::TurnEnd { .. }))
        .unwrap();
    let SessionRecord::Checkpoint { step, .. } = recs[end - 1] else {
        panic!("{:?}", recs[end - 1])
    };
    let child = run.fork_at(Some(step)).await;
    let (_, child_path) = run.engine.current_session();
    run.turn_with("on the branch").await;
    run.stop().await;
    let logged = checkpoints(&records_at(&child_path));
    assert!(!logged.is_empty());
    for (n, commit, _) in logged {
        let name = format!("refs/tautly/checkpoints/{child}/{n}");
        assert!(
            repo.git_ok(&["rev-parse", "-q", "--verify", &name]),
            "{name} is missing"
        );
        assert_eq!(repo.git(&["rev-parse", &name]), commit);
    }
}
