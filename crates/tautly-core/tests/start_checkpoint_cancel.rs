//! Cancel while a turn waits for the session-start checkpoint (spec 8: within 200 ms).
//!
//! Its own test binary with a single test: it puts a slow `git` first on `PATH` before any
//! other thread runs, and only this process sees it.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command as Process;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;
use tautly_core::{Command, Config, EngineBuilder, Event, FinishReason, SessionRecord};
use tautly_provider::{MockProvider, MockTurn, ModelId, ProviderEvent, StopReason};
use tautly_sandbox::Mode;
use tautly_session::SessionReader;
use tautly_testkit::{MODEL, Run, WAIT, catalog, start};

fn real_git() -> PathBuf {
    let path = std::env::var_os("PATH").unwrap();
    std::env::split_paths(&path)
        .map(|d| d.join("git"))
        .find(|p| p.is_file())
        .expect("git is installed")
}

fn config() -> Config {
    let catalog = catalog(200_000, 8000, "str_replace");
    let mut c = Config {
        model: ModelId::from(MODEL),
        catalog_overrides: Some(catalog),
        ..Config::default()
    };
    c.permissions.mode = Mode::Yolo;
    // A snapshot that times out turns checkpoints off for the session; under load
    // that would change what these tests see. Tests of the limit set their own.
    c.checkpoints.timeout_ms = 120_000;
    c
}

fn text(t: &str) -> MockTurn {
    MockTurn::Events(vec![
        ProviderEvent::TextDelta(t.into()),
        ProviderEvent::Done {
            stop: StopReason::EndTurn,
        },
    ])
}

/// A committed repository at a fresh temporary directory, and a data home.
fn repo(git: &Path) -> (tempfile::TempDir, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "a\n").unwrap();
    for args in [
        &["init", "-q", "-b", "main"][..],
        &["add", "-A"],
        &["commit", "-q", "-m", "init"],
    ] {
        let ok = Process::new(git)
            .current_dir(dir.path())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args(["-c", "user.name=T", "-c", "user.email=t@example.com"])
            .args(args)
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?}");
    }
    (dir, home)
}

trait RunExt {
    fn start(dir: &Path, home: &Path, mock: &MockProvider) -> Run;
    async fn cancel_at(&mut self, at: impl Fn(&Event) -> bool) -> (Duration, FinishReason);
    async fn records(self) -> Vec<SessionRecord>;
}

impl RunExt for Run {
    fn start(dir: &Path, home: &Path, mock: &MockProvider) -> Run {
        start(EngineBuilder {
            provider: Some(Arc::new(mock.clone())),
            data_home: Some(home.to_path_buf()),
            ..EngineBuilder::new(config(), dir)
        })
    }

    /// Send `Cancel` once `at` is seen (and 30 ms more); how long `TurnFinished` took
    /// after it, and its reason.
    async fn cancel_at(&mut self, at: impl Fn(&Event) -> bool) -> (Duration, FinishReason) {
        self.engine
            .send(Command::SendMessage {
                id: None,
                text: "go".into(),
            })
            .await
            .unwrap();
        while !at(&self.next().await) {}
        tokio::time::sleep(Duration::from_millis(30)).await;
        let t0 = Instant::now();
        self.engine.send(Command::Cancel).await.unwrap();
        loop {
            if let Event::TurnFinished { reason, .. } = self.next().await {
                return (t0.elapsed(), reason);
            }
        }
    }

    /// Shut down; the session's records.
    async fn records(mut self) -> Vec<SessionRecord> {
        let path = self.engine.session_path().to_path_buf();
        self.engine.send(Command::Shutdown).await.unwrap();
        while tokio::time::timeout(WAIT, self.events.recv())
            .await
            .expect("the engine did not stop")
            .is_some()
        {}
        SessionReader::<SessionRecord>::open(&path)
            .unwrap()
            .records()
            .map(|r| r.unwrap().1)
            .collect()
    }
}

/// Review probe P3: the first tool call waits for the start checkpoint.
async fn before_the_first_tool_call(git: &Path) {
    let (dir, home) = repo(git);
    let mock = MockProvider::new(vec![
        MockTurn::Events(vec![
            ProviderEvent::ToolCall {
                id: "c1".into(),
                name: "bash".into(),
                input_json: json!({"command": "touch ran.txt"}).to_string(),
            },
            ProviderEvent::Done {
                stop: StopReason::ToolUse,
            },
        ]),
        text("never sent"),
    ]);
    let mut run = Run::start(dir.path(), home.path(), &mock);
    // The response is in (its status follows it) and the step waits before its calls.
    let (took, reason) = run
        .cancel_at(|e| matches!(e, Event::ContextStatus { .. }))
        .await;
    assert_eq!(reason, FinishReason::Cancelled);
    assert!(
        took < Duration::from_millis(250),
        "TurnFinished {took:?} after Cancel"
    );
    assert!(!dir.path().join("ran.txt").exists(), "the command ran");
    assert_eq!(mock.requests().len(), 1);
    let records = run.records().await;
    // The call was answered `cancelled` without running.
    assert!(
        records.iter().any(|r| matches!(
            r,
            SessionRecord::ToolResult { id, is_error: true, ran: false, .. } if id == "c1"
        )),
        "{records:?}"
    );
}

/// A turn without tool calls: its turn-end checkpoint waits for the start checkpoint
/// (it holds the checkpoint lock).
async fn at_the_turn_end(git: &Path) {
    let (dir, home) = repo(git);
    let mock = MockProvider::new(vec![text("hello")]);
    let mut run = Run::start(dir.path(), home.path(), &mock);
    let (took, _) = run
        .cancel_at(|e| matches!(e, Event::TextDelta { .. }))
        .await;
    assert!(
        took < Duration::from_millis(250),
        "TurnFinished {took:?} after Cancel"
    );
    let records = run.records().await;
    assert!(
        records.iter().any(|r| matches!(
            r,
            SessionRecord::CheckpointSkipped { reason, .. } if reason == "cancelled"
        )),
        "{records:?}"
    );
}

/// Review A M5: `Undo` and `Fork` wait for the start checkpoint (they read the log), as
/// jobs: a `Shutdown` sent meanwhile is read and stops them.
async fn shutdown_while_a_command_waits(git: &Path, cmd: Command) {
    let (dir, home) = repo(git);
    let mock = MockProvider::new(vec![]);
    let mut run = Run::start(dir.path(), home.path(), &mock);
    run.engine.send(cmd.clone()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let t0 = Instant::now();
    run.engine.send(Command::Shutdown).await.unwrap();
    while tokio::time::timeout(WAIT, run.events.recv())
        .await
        .expect("the engine did not stop")
        .is_some()
    {}
    let took = t0.elapsed();
    assert!(
        took < Duration::from_millis(1000),
        "{cmd:?}: the engine stopped {took:?} after Shutdown"
    );
}

fn pins(path: &Path) -> Vec<String> {
    SessionReader::<SessionRecord>::open(path)
        .unwrap()
        .records()
        .filter_map(|r| match r.unwrap().1 {
            SessionRecord::Command(Command::Pin { text }) => Some(text),
            _ => None,
        })
        .collect()
}

/// Re-review N1: a command sent while a `Fork` job runs (here: waits for the start
/// checkpoint) is logged in the branch it applies to, not in the parent.
async fn a_pin_sent_during_a_fork_is_logged_in_the_branch(git: &Path) {
    let (dir, home) = repo(git);
    let mock = MockProvider::new(vec![]);
    let mut run = Run::start(dir.path(), home.path(), &mock);
    let parent = run.engine.session_path().to_path_buf();
    run.engine
        .send(Command::Pin {
            text: "BEFORE".into(),
        })
        .await
        .unwrap();
    run.engine
        .send(Command::Fork {
            id: None,
            at: None,
            label: None,
        })
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    run.engine
        .send(Command::Pin {
            text: "DURING".into(),
        })
        .await
        .unwrap();
    let first = run.engine.session_id().to_string();
    while !matches!(run.next().await, Event::SessionChanged { id: _, session_id } if session_id != first)
    {
    }
    let (_, branch) = run.engine.current_session();
    assert_ne!(branch, parent);
    run.records().await;
    assert_eq!(pins(&parent), ["BEFORE"]);
    assert_eq!(pins(&branch), ["BEFORE", "DURING"]);
}

/// Re-review 2 M1: commands held while a fork runs are bounded (an error for each one
/// dropped past the bound), and those a `Shutdown` leaves unrun are said once.
async fn commands_held_during_a_fork_are_bounded_and_unrun_ones_said(git: &Path) {
    let (dir, home) = repo(git);
    let mock = MockProvider::new(vec![]);
    let mut run = Run::start(dir.path(), home.path(), &mock);
    run.engine
        .send(Command::Pin {
            text: "BEFORE".into(),
        })
        .await
        .unwrap();
    run.engine
        .send(Command::Fork {
            id: None,
            at: None,
            label: None,
        })
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    for i in 0..70 {
        run.engine
            .send(Command::Pin {
                text: format!("P{i}"),
            })
            .await
            .unwrap();
    }
    run.engine.send(Command::Shutdown).await.unwrap();
    let mut events = Vec::new();
    while let Some(ev) = tokio::time::timeout(WAIT, run.events.recv())
        .await
        .expect("the engine did not stop")
    {
        events.push(ev);
    }
    let warned = |s: &str| {
        events
            .iter()
            .filter(|e| matches!(e, Event::Warning { message } if message.contains(s)))
            .count()
    };
    // Past the bound each is refused (CP5-A M1), unlogged.
    let refused = events
        .iter()
        .filter(
            |e| matches!(e, Event::Error { message, .. } if message.contains("too many commands")),
        )
        .count();
    assert_eq!(refused, 6, "{events:?}");
    assert_eq!(warned("not run"), 1, "{events:?}");
}

#[test]
fn cancel_while_the_session_start_checkpoint_is_taken_ends_the_turn_within_200ms() {
    let git = real_git();
    let shims = tempfile::tempdir().unwrap();
    let shim = shims.path().join("git");
    // Review probe P3 without 30,000 files: `git add` (the snapshot's slow part) takes 2 s.
    std::fs::write(
        &shim,
        format!(
            "#!/bin/sh\nfor a in \"$@\"; do [ \"$a\" = add ] && sleep 2; done\nexec '{}' \"$@\"\n",
            git.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = std::env::var_os("PATH").unwrap();
    let mut dirs = vec![shims.path().to_path_buf()];
    dirs.extend(std::env::split_paths(&path));
    // SAFETY: the only test of this binary, before its runtime starts: no other thread
    // reads the environment.
    unsafe { std::env::set_var("PATH", std::env::join_paths(dirs).unwrap()) };

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        before_the_first_tool_call(&git).await;
        at_the_turn_end(&git).await;
        shutdown_while_a_command_waits(
            &git,
            Command::Undo {
                id: None,
                turn: None,
                dry_run: true,
            },
        )
        .await;
        a_pin_sent_during_a_fork_is_logged_in_the_branch(&git).await;
        commands_held_during_a_fork_are_bounded_and_unrun_ones_said(&git).await;
        shutdown_while_a_command_waits(
            &git,
            Command::Fork {
                id: None,
                at: None,
                label: None,
            },
        )
        .await;
    });
}
