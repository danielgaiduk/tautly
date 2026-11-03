//! Git checkpoints when `git` on `PATH` is missing or slow (spec 7.11.11). These tests
//! change the process's `PATH`, so they live in a binary of their own and run one at a
//! time (under [`PATH_LOCK`], each on its own runtime).

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command as Process;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tautly_core::{Command, Config, Engine, EngineBuilder, Event, SessionRecord};
use tautly_provider::{
    Catalog, MockProvider, MockTurn, ModelId, ProviderEvent, RawUsage, StopReason,
};
use tautly_sandbox::Mode;
use tautly_session::SessionReader;
use tokio::sync::mpsc;

const MODEL: &str = "mock/test";
const WAIT: Duration = Duration::from_secs(30);
static PATH_LOCK: Mutex<()> = Mutex::new(());

fn config() -> Config {
    let catalog = Catalog::from_toml(&format!(
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
    .unwrap();
    let mut c = Config {
        model: ModelId::from(MODEL),
        catalog_overrides: Some(catalog),
        ..Config::default()
    };
    c.permissions.mode = Mode::Yolo;
    c
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

/// The first `git` on `PATH`.
fn real_git() -> PathBuf {
    let path = std::env::var_os("PATH").unwrap();
    std::env::split_paths(&path)
        .map(|d| d.join("git"))
        .find(|p| p.is_file())
        .expect("git is installed")
}

/// A committed repository at a fresh temporary directory (git run before `PATH` changes).
fn repo(git: &Path) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "a\n").unwrap();
    for args in [
        &["init", "-q", "-b", "main"][..],
        &["add", "-A"],
        &[
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-q",
            "-m",
            "i",
        ],
    ] {
        let ok = Process::new(git)
            .current_dir(dir.path())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args(args)
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?}");
    }
    dir
}

/// Run `body` with `PATH` set to `path`, alone, on a runtime of its own.
fn with_path<T>(path: &Path, body: impl std::future::Future<Output = T>) -> T {
    let _alone = PATH_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let saved = std::env::var_os("PATH");
    // SAFETY: the tests of this binary hold PATH_LOCK while they read or change the
    // environment, and no other thread of this process uses it meanwhile.
    unsafe { std::env::set_var("PATH", path) };
    let out = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(body);
    // SAFETY: as above.
    unsafe {
        match saved {
            Some(p) => std::env::set_var("PATH", p),
            None => std::env::remove_var("PATH"),
        }
    }
    out
}

fn engine(root: &Path, home: &Path, turns: Vec<MockTurn>) -> (Engine, mpsc::Receiver<Event>) {
    EngineBuilder {
        provider: Some(Arc::new(MockProvider::new(turns))),
        data_home: Some(home.to_path_buf()),
        ..EngineBuilder::new(config(), root)
    }
    .build()
    .unwrap()
}

async fn next(events: &mut mpsc::Receiver<Event>) -> Event {
    tokio::time::timeout(WAIT, events.recv())
        .await
        .expect("timed out waiting for an event")
        .expect("event channel closed")
}

async fn stop(engine: Engine, mut events: mpsc::Receiver<Event>) -> Vec<Event> {
    engine.send(Command::Shutdown).await.unwrap();
    let mut rest = Vec::new();
    while let Ok(Some(ev)) = tokio::time::timeout(WAIT, events.recv()).await {
        rest.push(ev);
    }
    rest
}

#[test]
fn missing_git_warns_once_and_logs_a_skip() {
    let git = real_git();
    let repo = repo(&git);
    let home = tempfile::tempdir().unwrap();
    let empty = tempfile::tempdir().unwrap();
    let (events, path) = with_path(empty.path(), async {
        let (engine, mut events) = engine(repo.path(), home.path(), vec![text_step("hi")]);
        engine
            .send(Command::SendMessage { text: "hi".into() })
            .await
            .unwrap();
        let mut seen = Vec::new();
        loop {
            let ev = next(&mut events).await;
            let done = matches!(ev, Event::TurnFinished { .. });
            seen.push(ev);
            if done {
                break;
            }
        }
        let path = engine.session_path().to_path_buf();
        seen.extend(stop(engine, events).await);
        (seen, path)
    });
    let warnings: Vec<&String> = events
        .iter()
        .filter_map(|e| match e {
            Event::Warning { message } if message.starts_with("checkpoint skipped") => {
                Some(message)
            }
            _ => None,
        })
        .collect();
    assert_eq!(warnings, ["checkpoint skipped: git is not available"]);
    let reader = SessionReader::<SessionRecord>::open(&path).unwrap();
    let skips: Vec<String> = reader
        .records()
        .filter_map(|r| match r.unwrap().1 {
            SessionRecord::CheckpointSkipped { reason, .. } => Some(reason),
            _ => None,
        })
        .collect();
    assert_eq!(skips, ["no_git"]);
}

#[test]
fn session_start_checkpoint_does_not_delay_the_first_command() {
    let git = real_git();
    let repo = repo(&git);
    let home = tempfile::tempdir().unwrap();
    // A git that takes 1.5 s to say where HEAD is (the first call of every snapshot).
    let shim_dir = tempfile::tempdir().unwrap();
    let shim = shim_dir.path().join("git");
    std::fs::write(
        &shim,
        format!(
            "#!/bin/sh\ncase \"$*\" in *rev-parse*) sleep 1.5;; esac\nexec '{}' \"$@\"\n",
            git.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!("{}:/bin:/usr/bin", shim_dir.path().display());
    with_path(Path::new(&path), async {
        let (engine, mut events) = engine(repo.path(), home.path(), vec![text_step("hi")]);
        let began = Instant::now();
        engine
            .send(Command::SendMessage { text: "hi".into() })
            .await
            .unwrap();
        while !matches!(next(&mut events).await, Event::TurnStarted) {}
        let waited = began.elapsed();
        assert!(
            waited < Duration::from_millis(1000),
            "the turn waited {waited:?} for the session-start checkpoint"
        );
        while !matches!(next(&mut events).await, Event::TurnFinished { .. }) {}
        let path = engine.session_path().to_path_buf();
        stop(engine, events).await;
        // It was still taken, before the turn-end one.
        let reader = SessionReader::<SessionRecord>::open(&path).unwrap();
        let taken: Vec<(u32, u32)> = reader
            .records()
            .filter_map(|r| match r.unwrap().1 {
                SessionRecord::Checkpoint { turn, step, .. } => Some((turn, step)),
                _ => None,
            })
            .collect();
        assert_eq!(taken.first(), Some(&(0, 0)), "{taken:?}");
        assert!(taken.len() >= 2, "{taken:?}");
    });
}
