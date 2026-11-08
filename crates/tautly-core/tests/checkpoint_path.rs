//! Git checkpoints when `git` on `PATH` is missing or slow (spec 7.11.11). These tests
//! change the process's `PATH`, so they live in a binary of their own and run one at a
//! time (under [`PATH_LOCK`], each on its own runtime).

use std::ffi::OsStr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command as Process;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tautly_core::{Command, Config, Engine, EngineBuilder, Event, SessionRecord};
use tautly_provider::mock::text_step;
use tautly_provider::{MockProvider, MockTurn, ModelId, ProviderEvent, StopReason};
use tautly_sandbox::Mode;
use tautly_session::SessionReader;
use tautly_testkit::catalog;
use tokio::sync::mpsc;

const MODEL: &str = "mock/test";
const WAIT: Duration = Duration::from_secs(30);
static PATH_LOCK: Mutex<()> = Mutex::new(());

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
    with_env("PATH", path.as_os_str(), body)
}

/// Run `body` with the environment variable `key` set to `value`, alone, on a runtime of
/// its own.
fn with_env<T>(key: &str, value: &OsStr, body: impl std::future::Future<Output = T>) -> T {
    let _alone = PATH_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let saved = std::env::var_os(key);
    // SAFETY: the tests of this binary hold PATH_LOCK while they read or change the
    // environment, and no other thread of this process uses it meanwhile.
    unsafe { std::env::set_var(key, value) };
    let out = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(body);
    // SAFETY: as above.
    unsafe {
        match saved {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
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
            .send(Command::SendMessage {
                id: None,
                text: "hi".into(),
            })
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
            .send(Command::SendMessage {
                id: None,
                text: "hi".into(),
            })
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

fn bash_step(id: &str, command: &str) -> MockTurn {
    MockTurn::Events(vec![
        ProviderEvent::ToolCall {
            id: id.into(),
            name: "bash".into(),
            input_json: serde_json::json!({ "command": command }).to_string(),
        },
        ProviderEvent::Done {
            stop: StopReason::ToolUse,
        },
    ])
}

#[test]
fn old_git_in_a_partial_clone_warns_once_and_logs_one_skip() {
    let git = real_git();
    let repo = repo(&git);
    let home = tempfile::tempdir().unwrap();
    let marker = home.path().join("lazy-fetch");
    let url = format!("ext::sh -c touch% {}", marker.display());
    for (key, value) in [
        ("core.repositoryformatversion", "1"),
        ("extensions.partialClone", "origin"),
        ("remote.origin.promisor", "true"),
        ("remote.origin.url", url.as_str()),
        ("protocol.ext.allow", "always"),
    ] {
        let ok = Process::new(&git)
            .current_dir(repo.path())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .args(["config", key, value])
            .status()
            .unwrap()
            .success();
        assert!(ok);
    }
    // Git 2.43, which ignores GIT_NO_LAZY_FETCH.
    let shim_dir = tempfile::tempdir().unwrap();
    let shim = shim_dir.path().join("git");
    std::fs::write(
        &shim,
        format!(
            "#!/bin/sh\nfor last in \"$@\"; do :; done\n\
             case \"$last\" in version|--version) echo 'git version 2.43.0'; exit 0;; esac\n\
             unset GIT_NO_LAZY_FETCH\nexec '{}' \"$@\"\n",
            git.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!("{}:/bin:/usr/bin", shim_dir.path().display());
    let (events, log) = with_path(Path::new(&path), async {
        let (engine, mut events) = engine(
            repo.path(),
            home.path(),
            vec![
                bash_step("b1", "printf 'b\\n' > a.txt"),
                text_step("first"),
                bash_step("b2", "printf 'c\\n' > a.txt"),
                text_step("second"),
            ],
        );
        let mut seen = Vec::new();
        for text in ["one", "two"] {
            engine
                .send(Command::SendMessage {
                    id: None,
                    text: text.into(),
                })
                .await
                .unwrap();
            loop {
                let ev = next(&mut events).await;
                let done = matches!(ev, Event::TurnFinished { .. });
                seen.push(ev);
                if done {
                    break;
                }
            }
        }
        let log = engine.session_path().to_path_buf();
        seen.extend(stop(engine, events).await);
        (seen, log)
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
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("partial clone"), "{warnings:?}");
    let reader = SessionReader::<SessionRecord>::open(&log).unwrap();
    let mut skips = Vec::new();
    let mut taken = 0;
    for r in reader.records() {
        match r.unwrap().1 {
            SessionRecord::CheckpointSkipped { reason, .. } => skips.push(reason),
            SessionRecord::Checkpoint { .. } => taken += 1,
            _ => {}
        }
    }
    assert_eq!(skips, ["unsafe_partial_clone"]);
    assert_eq!(taken, 0);
    assert!(!marker.exists(), "the promisor remote's command ran");
}

/// The user's excludes file (`core.excludesFile` of the global configuration) as it was
/// at the turn's start decides what undo keeps, and the rules are kept in the data home,
/// never in the repository.
#[test]
fn undo_uses_the_global_excludes_file_of_the_turns_start() {
    undo_with_global_excludes(false);
}

/// The same when the excludes file is a symbolic link (as a dotfiles manager makes it):
/// the user's own configuration names it, so it is followed.
#[test]
fn undo_follows_a_symlinked_global_excludes_file() {
    undo_with_global_excludes(true);
}

fn undo_with_global_excludes(symlinked: bool) {
    let git = real_git();
    let repo = repo(&git);
    let root = repo.path().canonicalize().unwrap();
    std::fs::write(root.join(".secret"), "S\n").unwrap();
    let home = tempfile::tempdir().unwrap();
    let user = tempfile::tempdir().unwrap();
    let ignore = user.path().join("ignore");
    std::fs::write(&ignore, ".secret\n").unwrap();
    let named = if symlinked {
        let link = user.path().join("ignore-link");
        std::os::unix::fs::symlink(&ignore, &link).unwrap();
        link
    } else {
        ignore.clone()
    };
    let global = user.path().join("gitconfig");
    std::fs::write(
        &global,
        format!("[core]\n\texcludesFile = {}\n", named.display()),
    )
    .unwrap();
    let answer = with_env("GIT_CONFIG_GLOBAL", global.as_os_str(), async {
        let (engine, mut events) = engine(
            &root,
            home.path(),
            vec![
                bash_step(
                    "b1",
                    "touch ready && while [ ! -e go ]; do sleep 0.02; done \
                     && printf 'changed\\n' > a.txt",
                ),
                text_step("done"),
            ],
        );
        engine
            .send(Command::SendMessage {
                id: None,
                text: "go".into(),
            })
            .await
            .unwrap();
        let began = Instant::now();
        while !root.join("ready").exists() {
            assert!(began.elapsed() < WAIT, "the command did not start");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // Emptied while the command runs (after its checkpoint).
        std::fs::write(&ignore, "").unwrap();
        std::fs::write(root.join("go"), "").unwrap();
        while !matches!(next(&mut events).await, Event::TurnFinished { .. }) {}
        engine
            .send(Command::Undo {
                id: None,
                turn: None,
                dry_run: false,
            })
            .await
            .unwrap();
        let answer = loop {
            let ev = next(&mut events).await;
            if matches!(ev, Event::Undone { .. } | Event::Error { .. }) {
                break ev;
            }
        };
        stop(engine, events).await;
        answer
    });
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
    assert_eq!(restored, ["a.txt"]);
    assert_eq!(deleted, ["go", "ready"]);
    assert!(note.unwrap_or_default().contains(".secret"));
    assert_eq!(
        std::fs::read_to_string(root.join(".secret"))
            .ok()
            .as_deref(),
        Some("S\n")
    );
    // No checkpoint commit carries the rules.
    let objects = Process::new(&git)
        .current_dir(&root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .args(["cat-file", "--batch-all-objects", "--batch"])
        .output()
        .unwrap()
        .stdout;
    let has = |needle: &[u8]| objects.windows(needle.len()).any(|w| w == needle);
    assert!(!has(b"tautly-excludes") && !has(b"2e7365637265740a"));
}

fn write_step(id: &str, path: &str, content: &str) -> MockTurn {
    MockTurn::Events(vec![
        ProviderEvent::ToolCall {
            id: id.into(),
            name: "write".into(),
            input_json: serde_json::json!({ "path": path, "content": content }).to_string(),
        },
        ProviderEvent::Done {
            stop: StopReason::ToolUse,
        },
    ])
}

/// Send `text` and wait for the turn's end.
async fn turn(engine: &Engine, events: &mut mpsc::Receiver<Event>, text: &str) {
    engine
        .send(Command::SendMessage {
            id: None,
            text: text.into(),
        })
        .await
        .unwrap();
    while !matches!(next(events).await, Event::TurnFinished { .. }) {}
}

/// CP5 review I2: an absolute `PATH` entry inside the repository (direnv's `PATH_add bin`,
/// an activated `.venv/bin`) is skipped for git run in a linked worktree of it too: the
/// fork's checkout, its own session's checkpoints, a bisect's moves and `sessions delete`.
#[test]
fn an_in_repo_path_entry_is_skipped_for_worktree_git() {
    let git = real_git();
    let repo = repo(&git);
    let root = repo.path().canonicalize().unwrap();
    let home = tempfile::tempdir().unwrap();
    let home = home.path().canonicalize().unwrap();
    let marker = home.join("repo-git-ran");
    std::fs::create_dir(root.join("bin")).unwrap();
    std::fs::write(
        root.join("bin/git"),
        format!(
            "#!/bin/sh\necho \"$*\" >> '{}'\nexec '{}' \"$@\"\n",
            marker.display(),
            git.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(root.join("bin/git"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!(
        "{}:{}:/bin:/usr/bin",
        root.join("bin").display(),
        git.parent().unwrap().display()
    );
    with_path(Path::new(&path), async {
        let (main, mut events) = engine(
            &root,
            &home,
            vec![
                write_step("w1", "a.txt", "one\n"),
                text_step("first"),
                write_step("w2", "a.txt", "two\n"),
                text_step("second"),
            ],
        );
        turn(&main, &mut events, "one").await;
        turn(&main, &mut events, "two").await;
        let log = main.session_path().to_path_buf();
        stop(main, events).await;
        let sb: Arc<dyn tautly_sandbox::Sandbox> =
            Arc::new(tautly_sandbox::LocalSandbox::new(root.clone()).unwrap());
        let made = tautly_core::worktree::fork_to_worktree(
            Arc::clone(&sb),
            &home,
            &log,
            tautly_core::worktree::ForkRequest {
                at: Some(3),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(made.worktree.join("a.txt")).unwrap(),
            "one\n"
        );
        // The fork's own session (its checkpoints are taken in the worktree).
        let (fork, mut fork_events) = engine(&made.worktree, &home, vec![text_step("hi")]);
        turn(&fork, &mut fork_events, "hi").await;
        stop(fork, fork_events).await;
        // A bisect moves its worktree from state to state.
        let reader = SessionReader::<SessionRecord>::open(&log).unwrap();
        let plan = tautly_core::bisect::plan(&reader, "! grep -q two a.txt", None, None).unwrap();
        let runner = tautly_core::bisect::BashRunner::new(
            Vec::new(),
            tokio_util::sync::CancellationToken::new(),
        );
        let out = tautly_core::bisect::run(&plan, sb.as_ref(), &runner, WAIT)
            .await
            .unwrap();
        assert_eq!(out.first_bad, Some(5));
        tautly_core::sessions::delete_with(&home, sb.as_ref(), &made.session_id, false)
            .await
            .unwrap();
        assert!(!made.worktree.exists());
    });
    let ran = std::fs::read_to_string(&marker).unwrap_or_default();
    assert!(
        ran.is_empty(),
        "the repository's git ran: {}",
        ran.lines().next().unwrap_or("")
    );
}
