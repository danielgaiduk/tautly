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

/// Review N7: checkpoint git does not inherit the session's provider key variables (the
/// configured ones given with `with_env_remove`; `HOME` stands in for one, since setting a
/// real key in this process would race the other tests).
#[tokio::test]
async fn checkpoint_git_does_not_see_provider_key_variables() {
    let repo = Repo::with(&[("a.txt", "a\n")]);
    let log = repo.home.path().join("env.log");
    let shim = repo.home.path().join("git");
    executable(
        &shim,
        &format!(
            "#!/bin/sh\necho \"home=${{HOME-unset}}\" >> '{}'\nHOME=/tmp exec git \"$@\"\n",
            log.display()
        ),
    );
    let mut cp = repo
        .checkpointer(CheckpointConfig::default())
        .with_git(&shim)
        .with_env_remove(vec!["HOME".into()]);
    repo.write("b.txt", "b\n");
    cp.snapshot_now().await.unwrap();
    let seen = std::fs::read_to_string(&log).unwrap();
    assert!(!seen.is_empty());
    assert!(seen.lines().all(|l| l == "home=unset"), "{seen}");
}

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
    // The configuration read goes to the real git (one that never ends is another case:
    // `config_include_that_never_ends_turns_checkpoints_off_once`); the next call hangs.
    executable(
        &shim,
        &format!(
            "#!/bin/sh\ncase \" $* \" in *\" config \"*) exec git \"$@\";; esac\n\
             sleep 30 &\necho $! > '{}'\nwait\n",
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

    // Directly: what is pruned is counted, and the exclude rules kept for it go too.
    let store = run_store(&repo);
    let kept = entries(&store);
    assert!(!kept.is_empty(), "the session kept no exclude rules");
    dated_ref(&repo, "refs/tautly/checkpoints/old/2", 30);
    let old = repo.git(&["rev-parse", "refs/tautly/checkpoints/old/2"]);
    std::fs::copy(store.join(&kept[0]), store.join(&old)).unwrap();
    let sb = repo.sandbox();
    assert_eq!(
        checkpoint::prune(sb.as_ref(), &repo.root(), 14, Some(&store))
            .await
            .unwrap(),
        1
    );
    assert_eq!(entries(&store), kept);
    assert_eq!(
        checkpoint::prune(sb.as_ref(), &repo.root(), 1, Some(&store))
            .await
            .unwrap(),
        1
    );
}

/// The exclude-rules store of the project's sessions (the only project of `repo`'s home).
fn run_store(repo: &Repo) -> PathBuf {
    walk(repo.home.path())
        .into_iter()
        .find_map(|p| {
            p.ancestors()
                .find(|a| a.file_name().is_some_and(|n| n == checkpoint::EXCLUDES_DIR))
                .map(Path::to_path_buf)
        })
        .expect("an exclude-rules store")
}

/// The checkpoint entries (object ids) of the store at `dir`, sorted.
fn entries(dir: &Path) -> Vec<String> {
    let mut out: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| matches!(n.len(), 40 | 64) && n.bytes().all(|b| b.is_ascii_hexdigit()))
        .collect();
    out.sort();
    out
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

    let store = path.parent().unwrap().join(checkpoint::EXCLUDES_DIR);
    assert!(
        !entries(&store).is_empty(),
        "the session kept no exclude rules"
    );
    let sb = repo.sandbox();
    let deleted = sessions::delete(repo.home.path(), sb.as_ref(), &id)
        .await
        .unwrap();
    assert_eq!(deleted.log, path);
    assert!(deleted.refs >= 2);
    // The exclude rules kept for its checkpoints went with their refs.
    assert_eq!(entries(&store), Vec::<String>::new());
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
    // Every checkpoint the branch copied has a ref of the branch's own, and keeps the
    // exclude rules it was taken with.
    let store = child_path.parent().unwrap().join(checkpoint::EXCLUDES_DIR);
    let kept = entries(&store);
    for (n, commit, _) in checkpoints(&records_at(&child_path)) {
        let name = format!("refs/tautly/checkpoints/{child}/{n}");
        assert_eq!(repo.git(&["rev-parse", &name]), commit);
        assert!(repo.git_ok(&["cat-file", "-e", &format!("{commit}^{{commit}}")]));
        assert!(kept.contains(&commit), "{commit} lost its rules: {kept:?}");
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

// -------------------------------------------------- partial clones, excludes, .git links

/// The first `git` on `PATH`.
fn real_git() -> PathBuf {
    let path = std::env::var_os("PATH").unwrap();
    std::env::split_paths(&path)
        .map(|d| d.join("git"))
        .find(|p| p.is_file())
        .expect("git is installed")
}

/// A git that says it is `version` and, like git before 2.44, ignores
/// `GIT_NO_LAZY_FETCH` (and `GIT_ALLOW_PROTOCOL` as well, so only the `-c` settings
/// stand between a lazy fetch and the promisor remote).
fn old_git_shim(dir: &Path, version: &str) -> PathBuf {
    let shim = dir.join(format!("git-{version}"));
    executable(
        &shim,
        &format!(
            "#!/bin/sh\nfor last in \"$@\"; do :; done\n\
             case \"$last\" in version|--version) echo 'git version {version}'; exit 0;; esac\n\
             unset GIT_NO_LAZY_FETCH GIT_ALLOW_PROTOCOL\nexec '{}' \"$@\"\n",
            real_git().display()
        ),
    );
    shim
}

/// Make `repo` a partial clone whose promisor remote's URL runs `touch <marker>` (an
/// `ext::` transport the repository's own configuration allows); with `missing`, HEAD's
/// `sub` tree object is deleted, so reading HEAD needs a lazy fetch.
fn promisor(repo: &Repo, marker: &Path, missing: bool) {
    if missing {
        let sub = repo.git(&["rev-parse", "HEAD:sub"]);
        std::fs::remove_file(repo.path(&format!(".git/objects/{}/{}", &sub[..2], &sub[2..])))
            .unwrap();
    }
    let url = format!("ext::sh -c touch% {}", marker.display());
    for (key, value) in [
        ("core.repositoryformatversion", "1"),
        ("extensions.partialClone", "origin"),
        ("remote.origin.promisor", "true"),
        ("remote.origin.url", url.as_str()),
        ("protocol.ext.allow", "always"),
    ] {
        repo.git(&["config", key, value]);
    }
}

#[tokio::test]
async fn promisor_remote_never_runs_where_git_ignores_no_lazy_fetch() {
    let repo = Repo::with(&[("a.txt", "a\n"), ("sub/x.txt", "x\n")]);
    let marker = repo.home.path().join("lazy-fetch");
    promisor(&repo, &marker, true);
    // New enough to pass the version check, yet lazy fetching: only the per-protocol
    // overrides keep the `ext::` command from running.
    let shim = old_git_shim(repo.home.path(), "2.99.0");
    let mut cp = repo
        .checkpointer(CheckpointConfig::default())
        .with_git(&shim);
    let _ = cp.snapshot(0, true).await;
    let _ = cp.backup().await;
    assert!(!marker.exists(), "the promisor remote's command ran");

    // Control: the same git, with the repository's protocol setting, runs it.
    let index = repo.home.path().join("control-index");
    let ran = Process::new(&shim)
        .current_dir(repo.root())
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_INDEX_FILE", &index)
        .args(["-c", "protocol.allow=never", "read-tree", "HEAD"])
        .output()
        .unwrap();
    assert!(marker.exists(), "the lazy fetch was not set up: {ran:?}");
}

#[tokio::test]
async fn old_git_takes_no_checkpoint_of_a_partial_clone() {
    let repo = Repo::with(&[("a.txt", "a\n"), ("sub/x.txt", "x\n")]);
    let marker = repo.home.path().join("lazy-fetch");
    promisor(&repo, &marker, true);
    let old = old_git_shim(repo.home.path(), "2.43.0");
    let mut cp = repo
        .checkpointer(CheckpointConfig::default())
        .with_git(&old);
    assert_eq!(cp.snapshot(0, true).await, Err(Skip::UnsafePartialClone));
    assert_eq!(cp.backup().await, Err(Skip::UnsafePartialClone));
    assert!(!marker.exists(), "the promisor remote's command ran");
    assert!(repo.refs("refs/tautly/").is_empty());

    // A promisor remote alone (no `extensions.partialClone`) is a partial clone too.
    let promisor_only = Repo::with(&[("a.txt", "a\n")]);
    promisor_only.git(&["config", "remote.origin.promisor", "true"]);
    promisor_only.git(&["config", "remote.origin.url", "https://example.invalid/r"]);
    let mut cp = promisor_only
        .checkpointer(CheckpointConfig::default())
        .with_git(&old);
    assert_eq!(cp.snapshot(0, true).await, Err(Skip::UnsafePartialClone));

    // So is a remote with a partial-clone filter alone.
    let filter_only = Repo::with(&[("a.txt", "a\n")]);
    filter_only.git(&["config", "remote.origin.partialclonefilter", "blob:none"]);
    filter_only.git(&["config", "remote.origin.url", "https://example.invalid/r"]);
    let mut cp = filter_only
        .checkpointer(CheckpointConfig::default())
        .with_git(&old);
    assert_eq!(cp.snapshot(0, true).await, Err(Skip::UnsafePartialClone));

    // Git 2.44 honours GIT_NO_LAZY_FETCH: a partial clone gets its checkpoints.
    let complete = Repo::with(&[("a.txt", "a\n"), ("sub/x.txt", "x\n")]);
    promisor(&complete, &marker, false);
    let new = old_git_shim(complete.home.path(), "2.44.0");
    let mut cp = complete
        .checkpointer(CheckpointConfig::default())
        .with_git(&new);
    assert!(cp.snapshot(0, true).await.is_ok());
    // And a repository that is no partial clone, with the old git.
    let plain = Repo::with(&[("a.txt", "a\n")]);
    let mut cp = plain
        .checkpointer(CheckpointConfig::default())
        .with_git(&old);
    assert!(cp.snapshot(0, true).await.is_ok());
    assert!(!marker.exists());
}

#[tokio::test]
async fn undo_uses_the_excludes_of_the_turns_start() {
    let repo = Repo::with(&[("tracked.txt", "orig\n")]);
    repo.write(".env", "SECRET=1\n");
    // Ignored only by the repository's exclude file (the user's excludes file, from the
    // global configuration, is covered in `checkpoint_path.rs`).
    std::fs::write(repo.path(".git/info/exclude"), ".env\n").unwrap();
    // It is emptied while the turn's command runs (after its checkpoint).
    let mock = MockProvider::new(vec![
        tools_step(vec![bash(
            "b1",
            "touch ready && while [ ! -e go ]; do sleep 0.02; done \
             && printf 'changed\\n' > tracked.txt",
        )]),
        text_step("done"),
    ]);
    let mut run = start(repo.builder(config(), &mock));
    run.send(Command::SendMessage { text: "go".into() }).await;
    let began = Instant::now();
    while !repo.path("ready").exists() {
        assert!(began.elapsed() < WAIT, "the command did not start");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    std::fs::write(repo.path(".git/info/exclude"), "").unwrap();
    repo.write("go", "");
    run.until(|e| matches!(e, Event::TurnFinished { .. })).await;
    let answer = run.undo(None, false).await;
    run.stop().await;
    let Event::Undone {
        restored,
        mut deleted,
        note,
        ..
    } = answer
    else {
        panic!("{answer:?}")
    };
    deleted.sort();
    assert_eq!(restored, ["tracked.txt"]);
    assert_eq!(deleted, ["go", "ready"]);
    let note = note.unwrap();
    assert!(note.contains(".env"), "{note}");
    assert_eq!(repo.read(".env").as_deref(), Some("SECRET=1\n"));
    assert_eq!(repo.read("tracked.txt").as_deref(), Some("orig\n"));
    // The rules were kept in the data home (owner-only), not in the repository.
    assert!(!all_objects(repo.git_cmd(&[])).contains(&b"tautly-excludes"[..]));
    let kept: Vec<PathBuf> = walk(repo.home.path())
        .into_iter()
        .filter(|p| {
            p.components()
                .any(|c| c.as_os_str() == "checkpoint-excludes")
        })
        .collect();
    assert!(!kept.is_empty(), "no exclude rules were kept");
    for p in &kept {
        let mode = std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "{}", p.display());
    }
}

/// Every object, with its content, of the repository `git` (a git command without
/// arguments yet) runs in.
fn all_objects(mut git: Process) -> Contents {
    let out = git
        .args(["cat-file", "--batch-all-objects", "--batch"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    Contents(out.stdout)
}

/// Bytes searched for a needle.
struct Contents(Vec<u8>);

impl Contents {
    fn contains(&self, needle: &[u8]) -> bool {
        self.0.windows(needle.len()).any(|w| w == needle)
    }
}

/// A plain (`git`, no hardening) command on the bare repository at `dir`.
fn bare_git(dir: &Path) -> Process {
    let mut cmd = Process::new("git");
    cmd.env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .arg(format!("--git-dir={}", dir.display()));
    cmd
}

#[tokio::test]
async fn excludes_snapshot_never_copies_a_file_into_the_repository() {
    let secret = b"aws_secret_access_key = SUPERSECRET123";
    let hex: String = secret.iter().map(|b| format!("{b:02x}")).collect();
    for variant in ["repository core.excludesFile", "info/exclude symlink"] {
        let repo = Repo::with(&[("a.txt", "a\n")]);
        let outside = tempfile::tempdir().unwrap();
        let file = outside.path().join("credentials");
        std::fs::write(&file, [&secret[..], b"\n"].concat()).unwrap();
        if variant.starts_with("repository") {
            repo.git(&["config", "core.excludesFile", file.to_str().unwrap()]);
        } else {
            let exclude = repo.path(".git/info/exclude");
            let _ = std::fs::remove_file(&exclude);
            std::os::unix::fs::symlink(&file, &exclude).unwrap();
        }
        // The repository's own remote mirrors every ref.
        let bare = tempfile::tempdir().unwrap();
        let remote = bare.path().canonicalize().unwrap();
        assert!(
            bare_git(&remote)
                .args(["init", "-q", "--bare"])
                .status()
                .unwrap()
                .success()
        );
        repo.git(&["config", "remote.origin.url", remote.to_str().unwrap()]);
        repo.git(&["config", "remote.origin.mirror", "true"]);
        let mock = MockProvider::new(vec![
            tools_step(vec![bash("b1", "printf 'b\\n' > a.txt && touch new.txt")]),
            text_step("done"),
        ]);
        let mut run = start(repo.builder(config(), &mock));
        run.turn_with("go").await;
        let answer = run.undo(None, false).await;
        run.stop().await;
        assert!(
            matches!(answer, Event::Undone { .. }),
            "{variant}: {answer:?}"
        );
        assert!(
            !repo.refs("refs/tautly/").is_empty(),
            "{variant}: no checkpoint"
        );
        let objects = all_objects(repo.git_cmd(&[]));
        assert!(
            !objects.contains(secret),
            "{variant}: the file is in an object"
        );
        assert!(
            !objects.contains(hex.as_bytes()),
            "{variant}: the file is in an object"
        );
        // The user's ordinary push publishes the checkpoint refs, never the file.
        assert!(repo.git_ok(&["push", "-q", "origin"]));
        let pushed = all_objects(bare_git(&remote));
        assert!(
            !pushed.contains(secret) && !pushed.contains(hex.as_bytes()),
            "{variant}"
        );
        // Nor is it copied into the data home.
        for p in walk(repo.home.path()) {
            let bytes = Contents(std::fs::read(&p).unwrap());
            assert!(
                !bytes.contains(secret),
                "{variant}: copied to {}",
                p.display()
            );
        }
    }
}

#[tokio::test]
async fn a_remote_that_would_push_checkpoints_is_warned_about_at_session_start() {
    // The repository's settings, and the remote warned about.
    type Case<'a> = (&'a [(&'a str, &'a str)], Option<&'a str>);
    let cases: [Case; 5] = [
        (
            &[
                ("remote.origin.url", "/nowhere"),
                ("remote.origin.mirror", "true"),
            ],
            Some("origin"),
        ),
        (
            &[
                ("remote.backup.url", "/nowhere"),
                ("remote.backup.push", "+refs/*:refs/*"),
            ],
            Some("backup"),
        ),
        (
            &[
                ("remote.up.url", "/nowhere"),
                (
                    "remote.up.push",
                    "refs/tautly/checkpoints/*:refs/tautly/checkpoints/*",
                ),
            ],
            Some("up"),
        ),
        (
            &[
                ("remote.origin.url", "/nowhere"),
                ("remote.origin.mirror", "false"),
                ("remote.origin.push", "refs/heads/main:refs/heads/main"),
            ],
            None,
        ),
        (&[], None),
    ];
    for (settings, remote) in cases {
        let repo = Repo::with(&[("a.txt", "a\n")]);
        for (key, value) in settings {
            repo.git(&["config", key, value]);
        }
        let mock = MockProvider::new(vec![
            tools_step(vec![bash("b1", "printf 'b\\n' > a.txt")]),
            text_step("done"),
        ]);
        let mut run = start(repo.builder(config(), &mock));
        let mut events = run.turn_with("one").await;
        events.extend(run.turn_with("two").await);
        run.stop().await;
        let warned: Vec<String> = warnings(&events)
            .into_iter()
            .filter(|w| w.contains("refs/tautly/"))
            .collect();
        match remote {
            Some(name) => {
                assert_eq!(warned.len(), 1, "{settings:?}: {warned:?}");
                assert!(warned[0].contains(&format!("\"{name}\"")), "{warned:?}");
            }
            None => assert!(warned.is_empty(), "{settings:?}: {warned:?}"),
        }
    }
}

#[tokio::test]
async fn a_missing_object_turns_checkpoints_off_when_it_recurs() {
    let repo = Repo::with(&[("a.txt", "a\n"), ("sub/x.txt", "x\n")]);
    let sub = repo.git(&["rev-parse", "HEAD:sub"]);
    std::fs::remove_file(repo.path(&format!(".git/objects/{}/{}", &sub[..2], &sub[2..]))).unwrap();
    let mock = MockProvider::new(vec![
        tools_step(vec![bash("b1", "printf 'b\\n' > a.txt")]),
        text_step("first"),
        tools_step(vec![bash("b2", "printf 'c\\n' > a.txt")]),
        text_step("second"),
    ]);
    let mut run = start(repo.builder(config(), &mock));
    let mut events = run.turn_with("one").await;
    events.extend(run.turn_with("two").await);
    let (_, path) = run.engine.current_session();
    run.stop().await;
    let skipped: Vec<String> = warnings(&events)
        .into_iter()
        .filter(|w| w.starts_with("checkpoint skipped"))
        .collect();
    assert_eq!(skipped.len(), 1, "{skipped:?}");
    let recs = records_at(&path);
    let reasons: Vec<String> = recs
        .iter()
        .filter_map(|r| match r {
            SessionRecord::CheckpointSkipped { reason, .. } => Some(reason.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(reasons, ["git_failed", "git_failed"]);
    assert!(checkpoints(&recs).is_empty());
}

#[tokio::test]
async fn dot_git_symlink_takes_no_checkpoint_and_says_so() {
    let real = Repo::with(&[("a.txt", "a\n")]);
    let root = Repo::bare_dir();
    std::os::unix::fs::symlink(real.path(".git"), root.path(".git")).unwrap();
    root.write("a.txt", "a\n");
    assert!(
        Checkpointer::new(
            root.sandbox(),
            root.root(),
            "s".into(),
            CheckpointConfig::default()
        )
        .is_none()
    );
    let mock = MockProvider::new(vec![
        tools_step(vec![bash("b1", "printf 'b\\n' > a.txt")]),
        text_step("first"),
        tools_step(vec![bash("b2", "printf 'c\\n' > a.txt")]),
        text_step("second"),
    ]);
    let mut run = start(root.builder(config(), &mock));
    let mut events = run.turn_with("one").await;
    events.extend(run.turn_with("two").await);
    let (_, path) = run.engine.current_session();
    run.stop().await;
    let skipped: Vec<String> = warnings(&events)
        .into_iter()
        .filter(|w| w.starts_with("checkpoint skipped"))
        .collect();
    assert_eq!(skipped, [skip_warning(&Skip::GitDirSymlink)]);
    let reasons: Vec<String> = records_at(&path)
        .into_iter()
        .filter_map(|r| match r {
            SessionRecord::CheckpointSkipped { reason, .. } => Some(reason),
            _ => None,
        })
        .collect();
    assert_eq!(reasons, ["git_dir_symlink"]);
    // Nothing was written to the repository the link points at.
    assert!(real.refs("refs/tautly/").is_empty());
}

/// Review B-I2, probe P2: a checkout whose `.git` names another repository's git
/// directories (a `commondir` in a `.git` directory, or a `.git` file) without git's
/// back-link takes no checkpoints there, and engine-start pruning does not touch that
/// repository's refs.
#[tokio::test]
async fn git_dir_another_repository_does_not_name_back_takes_no_checkpoint() {
    let victim = Repo::with(&[("a.txt", "a\n")]);
    let head = victim.git(&["rev-parse", "HEAD"]);
    victim.git(&["update-ref", "refs/tautly/checkpoints/old/1", &head]);
    let victim_git = victim.path(".git");
    let p2 = Repo::bare_dir();
    p2.write(".git/HEAD", "ref: refs/heads/main\n");
    p2.write(".git/commondir", &format!("{}\n", victim_git.display()));
    let file = Repo::bare_dir();
    file.write(".git", &format!("gitdir: {}\n", victim_git.display()));
    for hostile in [&p2, &file] {
        hostile.write("a.txt", "a\n");
        assert!(
            Checkpointer::new(
                hostile.sandbox(),
                hostile.root(),
                "s".into(),
                CheckpointConfig::default()
            )
            .is_none()
        );
        let sb = hostile.sandbox();
        assert!(
            checkpoint::prune(sb.as_ref(), &hostile.root(), 0, None)
                .await
                .is_err()
        );
        let mock = MockProvider::new(vec![
            tools_step(vec![bash("b1", "printf 'b\\n' > a.txt")]),
            text_step("first"),
        ]);
        let mut run = start(hostile.builder(config(), &mock));
        let events = run.turn_with("one").await;
        let (_, path) = run.engine.current_session();
        run.stop().await;
        let skipped: Vec<String> = warnings(&events)
            .into_iter()
            .filter(|w| w.starts_with("checkpoint skipped"))
            .collect();
        assert_eq!(skipped, [skip_warning(&Skip::GitDirUnverified)]);
        let reasons: Vec<String> = records_at(&path)
            .into_iter()
            .filter_map(|r| match r {
                SessionRecord::CheckpointSkipped { reason, .. } => Some(reason),
                _ => None,
            })
            .collect();
        assert_eq!(reasons, ["git_dir_unverified"]);
    }
    // The victim's refs are as they were: nothing added, nothing pruned.
    assert_eq!(
        victim.refs("refs/tautly/"),
        ["refs/tautly/checkpoints/old/1"]
    );
}

/// Review B-M1, probe P5: the spill store never enters a checkpoint, even when the
/// repository committed a `.tautly/tmp/.gitignore` that un-ignores it (or tracks files
/// there). The rest of `.tautly/` (a tracked config) is still snapshotted.
#[tokio::test]
async fn spill_store_never_enters_a_checkpoint() {
    let repo = Repo::with(&[
        ("a.txt", "a\n"),
        (".tautly/tmp/.gitignore", ""),
        (".tautly/tmp/tracked.txt", "committed\n"),
        (".tautly/config.toml", "max_turns = 10\n"),
    ]);
    repo.write(
        ".tautly/tmp/spill/0123456789abcdef.txt",
        "OPENAI_API_KEY=sk-x\n",
    );
    repo.write(".tautly/tmp/tracked.txt", "changed\n");
    repo.write(".tautly/config.toml", "max_turns = 5\n");
    let mut cp = repo.checkpointer(CheckpointConfig::default());
    let made = cp.snapshot_now().await.unwrap();
    let files = repo.git(&["ls-tree", "-r", "--name-only", &made.commit]);
    assert!(!files.contains(".tautly/tmp/"), "{files}");
    assert!(files.contains(".tautly/config.toml"), "{files}");
    let config = repo.git(&["show", &format!("{}:.tautly/config.toml", made.commit)]);
    assert_eq!(config, "max_turns = 5");
}

/// Review B-M3, probe P4: a repository `include.path` naming a FIFO blocks every git
/// call. The configuration read gives up within its own cap (not the operation's
/// deadline). The first time is an ordinary timeout (a slow network filesystem may
/// recover, review N6); the second in a row turns checkpoints off for the session, so
/// later commands do not each wait out the timeout.
#[tokio::test]
async fn config_include_that_never_ends_turns_checkpoints_off_once() {
    let repo = Repo::with(&[("a.txt", "a\n")]);
    let fifo = repo.home.path().join("never.fifo");
    let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
    // SAFETY: mkfifo with a valid NUL-terminated path.
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o644) }, 0);
    let mut git_config = std::fs::read_to_string(repo.path(".git/config")).unwrap();
    git_config.push_str(&format!("[include]\n\tpath = {}\n", fifo.display()));
    std::fs::write(repo.path(".git/config"), git_config).unwrap();

    let mut cp = repo.checkpointer(CheckpointConfig::default());
    let began = Instant::now();
    assert_eq!(cp.snapshot_now().await, Err(Skip::Timeout));
    assert_eq!(cp.snapshot_now().await, Err(Skip::ConfigTimeout));
    assert!(
        began.elapsed() < Duration::from_millis(5000),
        "{:?}",
        began.elapsed()
    );

    let mock = MockProvider::new(vec![
        tools_step(vec![bash("b1", "true")]),
        tools_step(vec![bash("b2", "true")]),
        tools_step(vec![bash("b3", "true")]),
        text_step("done"),
    ]);
    let began = Instant::now();
    let mut run = start(repo.builder(config(), &mock));
    let events = run.turn_with("go").await;
    let elapsed = began.elapsed();
    let (_, path) = run.engine.current_session();
    run.stop().await;
    let skipped: Vec<String> = warnings(&events)
        .into_iter()
        .filter(|w| w.starts_with("checkpoint skipped"))
        .collect();
    assert_eq!(
        skipped,
        [
            skip_warning(&Skip::Timeout),
            skip_warning(&Skip::ConfigTimeout)
        ]
    );
    let reasons: Vec<String> = records_at(&path)
        .into_iter()
        .filter_map(|r| match r {
            SessionRecord::CheckpointSkipped { reason, .. } => Some(reason),
            _ => None,
        })
        .collect();
    assert_eq!(reasons, ["timeout", "config_timeout"]);
    // Three commands and a turn end did not each wait for git: about 2 s each for the
    // engine-start prune, the session-start checkpoint and the first command (with the old
    // per-snapshot timeout it was 3 s for each of the five checkpoints, plus 60 s of prune).
    assert!(elapsed < Duration::from_secs(13), "{elapsed:?}");

    assert!(!repo.path(".git/refs/tautly").exists());
}

/// Review N3: a `.git` directory whose `refs` (or `refs/tautly`, `refs/tautly/checkpoints`,
/// `objects`) is a symbolic link would make checkpoints write and delete refs in the
/// repository it points at: no checkpoints, `git_dir_unverified`.
#[tokio::test]
async fn git_dir_with_linked_refs_or_objects_takes_no_checkpoint() {
    let victim = Repo::with(&[("a.txt", "a\n")]);
    let head = victim.git(&["rev-parse", "HEAD"]);
    victim.git(&["update-ref", "refs/tautly/checkpoints/old/1", &head]);
    let vgit = victim.path(".git");
    let link = |from: PathBuf, to: PathBuf| std::os::unix::fs::symlink(to, from).unwrap();
    type Layout<'a> = (&'a str, Box<dyn Fn(&Repo) + 'a>);
    let layouts: Vec<Layout> = vec![
        (
            "refs and objects",
            Box::new(|r: &Repo| {
                link(r.path(".git/refs"), vgit.join("refs"));
                link(r.path(".git/objects"), vgit.join("objects"));
            }),
        ),
        (
            "refs/tautly",
            Box::new(|r: &Repo| link(r.path(".git/refs/tautly"), vgit.join("refs/tautly"))),
        ),
        (
            "refs/tautly/checkpoints",
            Box::new(|r: &Repo| {
                std::fs::create_dir_all(r.path(".git/refs/tautly")).unwrap();
                link(
                    r.path(".git/refs/tautly/checkpoints"),
                    vgit.join("refs/tautly/checkpoints"),
                );
            }),
        ),
        (
            "objects",
            Box::new(|r: &Repo| {
                std::fs::rename(r.path(".git/objects"), r.path("own-objects")).unwrap();
                link(r.path(".git/objects"), vgit.join("objects"));
            }),
        ),
    ];
    for (name, make) in layouts {
        let hostile = Repo::with(&[("a.txt", "a\n")]);
        if name == "refs and objects" {
            std::fs::remove_dir_all(hostile.path(".git/refs")).unwrap();
            std::fs::remove_dir_all(hostile.path(".git/objects")).unwrap();
        }
        make(&hostile);
        assert!(
            Checkpointer::new(
                hostile.sandbox(),
                hostile.root(),
                "s".into(),
                CheckpointConfig::default()
            )
            .is_none(),
            "{name}"
        );
        let mock = MockProvider::new(vec![
            tools_step(vec![bash("b1", "printf 'b\\n' > a.txt")]),
            text_step("first"),
        ]);
        let mut run = start(hostile.builder(config(), &mock));
        let events = run.turn_with("one").await;
        run.stop().await;
        let skipped: Vec<String> = warnings(&events)
            .into_iter()
            .filter(|w| w.starts_with("checkpoint skipped"))
            .collect();
        assert_eq!(skipped, [skip_warning(&Skip::GitDirUnverified)], "{name}");
    }
    assert_eq!(
        victim.refs("refs/tautly/"),
        ["refs/tautly/checkpoints/old/1"]
    );
}

/// A real linked worktree keeps its checkpoints (in the shared repository).
#[tokio::test]
async fn linked_worktree_still_takes_checkpoints() {
    let main = Repo::with(&[("a.txt", "a\n")]);
    let top = tempfile::tempdir().unwrap();
    let wt = top.path().canonicalize().unwrap().join("wt");
    main.git(&["worktree", "add", "-q", "--detach", wt.to_str().unwrap()]);
    let sb: Arc<dyn Sandbox> = Arc::new(LocalSandbox::new(wt.clone()).unwrap());
    let mut cp = Checkpointer::new(sb, wt.clone(), "w".into(), CheckpointConfig::default())
        .expect("a linked worktree has a checkpointer");
    std::fs::write(wt.join("b.txt"), "b\n").unwrap();
    cp.snapshot_now().await.unwrap();
    assert_eq!(main.refs("refs/tautly/"), ["refs/tautly/checkpoints/w/1"]);
}
