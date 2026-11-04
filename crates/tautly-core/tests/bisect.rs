//! Session bisect (spec 7.12.7, Task 53): the first step boundary whose files fail a
//! check, found by materializing past states in one temporary worktree while the user's
//! work tree, index and HEAD stay as they were. Against real git repositories with
//! scripted providers.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command as Process;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{Value, json};
use tautly_core::bisect::{self, BashRunner, BisectError, Endpoint, TestRunner, Verdict};
use tautly_core::{Command, Config, EngineBuilder, Event, SessionRecord};
use tautly_provider::{Catalog, MockProvider, MockTurn, ModelId, ProviderEvent, StopReason};
use tautly_sandbox::{LocalSandbox, Mode};
use tautly_session::SessionReader;
use tempfile::TempDir;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const MODEL: &str = "mock/test";
const WAIT: Duration = Duration::from_secs(30);
const TIMEOUT: Duration = Duration::from_secs(60);
/// Passes while `a.txt` says `good`.
const CHECK: &str = "#!/bin/sh\ngrep -q good a.txt\n";

// ------------------------------------------------------------------------ fixtures

struct Repo {
    dir: TempDir,
    home: TempDir,
}

impl Repo {
    /// A git repository with `a.txt` (`good`), an executable `check.sh` and a
    /// `.gitignore` of `cache/` committed.
    fn new() -> Repo {
        let repo = Repo {
            dir: tempfile::tempdir().unwrap(),
            home: tempfile::tempdir().unwrap(),
        };
        repo.git(&["init", "-q", "-b", "main"]);
        repo.write("a.txt", "good\n");
        repo.write("check.sh", CHECK);
        repo.write(".gitignore", "cache/\n");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            repo.root().join("check.sh"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        repo.git(&["add", "-A"]);
        repo.git(&["commit", "-q", "-m", "init"]);
        repo
    }

    fn root(&self) -> PathBuf {
        self.dir.path().canonicalize().unwrap()
    }

    fn home(&self) -> PathBuf {
        self.home.path().canonicalize().unwrap()
    }

    fn write(&self, rel: &str, text: &str) {
        let p = self.root().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    fn git(&self, args: &[&str]) -> Option<String> {
        git_in(&self.root(), args)
    }

    fn sandbox(&self) -> LocalSandbox {
        LocalSandbox::new(self.root()).unwrap()
    }

    fn builder(&self, mock: &MockProvider) -> EngineBuilder {
        EngineBuilder {
            provider: Some(Arc::new(mock.clone())),
            data_home: Some(self.home()),
            ..EngineBuilder::new(config(), self.dir.path())
        }
    }

    /// A finished one-turn session in this repository running `steps`: its log.
    async fn session(&self, steps: Vec<MockTurn>) -> PathBuf {
        let mut run = Run::start(self.builder(&MockProvider::new(steps)));
        run.turn_with("go").await;
        let (_, log) = run.engine.current_session();
        run.stop().await;
        log
    }

    /// The state the user's tree is in: files, index, HEAD and status.
    fn snapshot(&self) -> (BTreeMap<String, String>, Vec<u8>, String, String) {
        (
            files(&self.root()),
            std::fs::read(self.root().join(".git/index")).unwrap(),
            self.git(&["rev-parse", "HEAD"]).unwrap(),
            self.git(&["status", "--porcelain", "--ignored"]).unwrap(),
        )
    }

    /// Every worktree git knows of besides the main one, and every directory under
    /// `<data home>/worktrees`.
    fn leftovers(&self) -> (Vec<String>, Vec<PathBuf>) {
        let admin = match std::fs::read_dir(self.root().join(".git/worktrees")) {
            Ok(entries) => entries
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect(),
            Err(_) => Vec::new(),
        };
        let mut dirs = Vec::new();
        if let Ok(projects) = std::fs::read_dir(self.home().join("worktrees")) {
            for p in projects {
                for e in std::fs::read_dir(p.unwrap().path()).unwrap() {
                    dirs.push(e.unwrap().path());
                }
            }
        }
        (admin, dirs)
    }

    fn assert_no_leftovers(&self) {
        let (admin, dirs) = self.leftovers();
        assert!(admin.is_empty(), "git still knows worktrees: {admin:?}");
        assert!(dirs.is_empty(), "left behind: {dirs:?}");
        let list = self.git(&["worktree", "list", "--porcelain"]).unwrap();
        assert_eq!(list.matches("worktree ").count(), 1, "{list}");
    }
}

fn git_in(dir: &Path, args: &[&str]) -> Option<String> {
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
    let mut cfg = Config {
        model: ModelId::from(MODEL),
        catalog_overrides: Some(catalog),
        ..Config::default()
    };
    cfg.permissions.mode = Mode::Yolo;
    cfg
}

/// Every file of the tree at `root` (outside `.git`) and its content.
fn files(root: &Path) -> BTreeMap<String, String> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, String>) {
        for e in std::fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            let rel = p.strip_prefix(root).unwrap().to_string_lossy().into_owned();
            if rel == ".git" {
                continue;
            }
            if p.is_dir() {
                walk(root, &p, out);
            } else {
                out.insert(rel, std::fs::read_to_string(&p).unwrap_or_default());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

fn call(id: &str, name: &str, args: Value) -> ProviderEvent {
    ProviderEvent::ToolCall {
        id: id.into(),
        name: name.into(),
        input_json: args.to_string(),
    }
}

fn write(id: &str, path: &str, content: &str) -> ProviderEvent {
    call(id, "write", json!({ "path": path, "content": content }))
}

fn bash(id: &str, command: &str) -> ProviderEvent {
    call(id, "bash", json!({ "command": command }))
}

fn tools_step(calls: Vec<ProviderEvent>) -> MockTurn {
    let mut evs = calls;
    evs.push(ProviderEvent::Done {
        stop: StopReason::ToolUse,
    });
    MockTurn::Events(evs)
}

fn said_step(text: &str, calls: Vec<ProviderEvent>) -> MockTurn {
    let mut evs = vec![ProviderEvent::TextDelta(text.into())];
    evs.extend(calls);
    evs.push(ProviderEvent::Done {
        stop: StopReason::ToolUse,
    });
    MockTurn::Events(evs)
}

fn text_step(text: &str) -> MockTurn {
    MockTurn::Events(vec![
        ProviderEvent::TextDelta(text.into()),
        ProviderEvent::Done {
            stop: StopReason::EndTurn,
        },
    ])
}

struct Run {
    engine: tautly_core::Engine,
    events: mpsc::Receiver<Event>,
}

impl Run {
    fn start(b: EngineBuilder) -> Run {
        let (engine, events) = b.build().unwrap();
        Run { engine, events }
    }

    async fn next(&mut self) -> Event {
        tokio::time::timeout(WAIT, self.events.recv())
            .await
            .expect("timed out waiting for an event")
            .expect("event channel closed")
    }

    async fn turn_with(&mut self, text: &str) {
        self.engine
            .send(Command::SendMessage { text: text.into() })
            .await
            .unwrap();
        while !matches!(self.next().await, Event::TurnFinished { .. }) {}
    }

    async fn stop(mut self) {
        self.engine.send(Command::Shutdown).await.unwrap();
        while tokio::time::timeout(WAIT, self.events.recv())
            .await
            .expect("the engine did not stop")
            .is_some()
        {}
    }
}

fn reader(path: &Path) -> SessionReader<SessionRecord> {
    SessionReader::<SessionRecord>::open(path).unwrap()
}

/// Steps 1 user, 2 write x.txt, 3 (saying so) break a.txt, 4 write y.txt, 5 text.
fn edit_breaks_at_3() -> Vec<MockTurn> {
    vec![
        tools_step(vec![write("w1", "x.txt", "x\n")]),
        said_step(
            "Now I change a.txt to say bad.",
            vec![write("w2", "a.txt", "bad\n")],
        ),
        tools_step(vec![write("w3", "y.txt", "y\n")]),
        text_step("done"),
    ]
}

/// A runner that judges a state by its `a.txt` without running anything, and records
/// where each run was and what it found there.
#[derive(Default)]
struct Probe {
    runs: Mutex<Vec<(PathBuf, String)>>,
    cancel_after: Option<usize>,
    /// Leave an ignored cache file and an untracked junk file behind each run.
    litter: bool,
    /// What each run found of the litter of the run before: (cache there, junk there).
    found: Mutex<Vec<(bool, bool)>>,
}

#[async_trait]
impl TestRunner for Probe {
    async fn run(&self, dir: &Path, _cmd: &str, _timeout: Duration) -> Verdict {
        let a = std::fs::read_to_string(dir.join("a.txt")).unwrap_or_default();
        self.runs
            .lock()
            .unwrap()
            .push((dir.to_path_buf(), a.clone()));
        if self.litter {
            self.found.lock().unwrap().push((
                dir.join("cache/hit").exists(),
                dir.join("junk.txt").exists(),
            ));
            std::fs::create_dir_all(dir.join("cache")).unwrap();
            std::fs::write(dir.join("cache/hit"), "1").unwrap();
            std::fs::write(dir.join("junk.txt"), "junk").unwrap();
        }
        if a.contains("good") {
            Verdict::Good
        } else {
            Verdict::Bad
        }
    }

    fn cancelled(&self) -> bool {
        self.cancel_after
            .is_some_and(|n| self.runs.lock().unwrap().len() >= n)
    }
}

/// The log at `log` with a `CheckpointSkipped` record inserted right after the last
/// record of step `after` (offsets renumbered).
fn with_skip_after(log: &Path, after: u32) {
    let text = std::fs::read_to_string(log).unwrap();
    let mut lines: Vec<Value> = text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    // The last record of step `after`: the last line before step after + 1 starts (its
    // `Request`).
    let next = lines
        .iter()
        .position(|l| {
            l["type"] == "record"
                && l["data"]["type"] == "request"
                && l["data"]["data"]["step"] == after + 1
        })
        .expect("no such step");
    let skip = serde_json::to_value(SessionRecord::CheckpointSkipped {
        reason: "timeout".into(),
        turn: 1,
        step: after + 1,
        point: None,
    })
    .unwrap();
    let t = lines[next]["t"].clone();
    lines.insert(
        next,
        json!({"type": "record", "offset": 0, "t": t, "data": skip}),
    );
    let mut offset = 0;
    for l in lines.iter_mut() {
        if l["type"] == "record" {
            l["offset"] = json!(offset);
            offset += 1;
        }
    }
    let out: String = lines.iter().map(|l| format!("{l}\n")).collect();
    std::fs::write(log, out).unwrap();
}

// --------------------------------------------------------------------------- tests

#[tokio::test]
async fn bisect_finds_first_failing_step() {
    let repo = Repo::new();
    let log = repo.session(edit_breaks_at_3()).await;
    let plan = bisect::plan(&reader(&log), "./check.sh", None, None).unwrap();
    assert_eq!((plan.good, plan.bad), (0, 5));
    assert_eq!(plan.candidates, [1, 2, 3, 4]);
    assert!(plan.skipped.is_empty(), "{:?}", plan.skipped);

    let runner = BashRunner::new(Vec::new(), CancellationToken::new());
    let out = bisect::run(&plan, &repo.sandbox(), &runner, TIMEOUT)
        .await
        .unwrap();
    assert_eq!(out.first_bad, Some(3));
    assert_eq!(out.last_good, 2);
    assert_eq!(out.turn, Some(1));
    assert_eq!(out.calls, ["write: a.txt"]);
    assert_eq!(out.text_excerpt, "Now I change a.txt to say bad.");
    assert!(out.uncertain.is_empty(), "{:?}", out.uncertain);
    assert!(out.runs <= plan.max_runs() && out.runs >= 3, "{}", out.runs);
    repo.assert_no_leftovers();
}

#[tokio::test]
async fn bisect_covers_bash_side_effects() {
    let repo = Repo::new();
    // 1 user, 2 write x, 3 bash (harmless), 4 write y, 5 bash sed -i (the bug), 6 write
    // z, 7 text.
    let log = repo
        .session(vec![
            tools_step(vec![write("w1", "x.txt", "x\n")]),
            tools_step(vec![bash("b1", "echo hi > hi.txt")]),
            tools_step(vec![write("w2", "y.txt", "y\n")]),
            said_step(
                "Renaming the word.",
                vec![bash(
                    "b2",
                    "sed -i.orig 's/good/bad/' a.txt && rm a.txt.orig",
                )],
            ),
            tools_step(vec![write("w3", "z.txt", "z\n")]),
            text_step("done"),
        ])
        .await;
    let plan = bisect::plan(&reader(&log), "./check.sh", None, None).unwrap();
    assert_eq!((plan.good, plan.bad), (0, 7));
    let runner = BashRunner::new(Vec::new(), CancellationToken::new());
    let out = bisect::run(&plan, &repo.sandbox(), &runner, TIMEOUT)
        .await
        .unwrap();
    assert_eq!(out.first_bad, Some(5));
    assert_eq!(out.last_good, 4);
    assert_eq!(
        out.calls,
        ["bash: sed -i.orig 's/good/bad/' a.txt && rm a.txt.orig"]
    );
    assert_eq!(out.text_excerpt, "Renaming the word.");
    // The agent's own tree still says bad: the bisect never touched it.
    assert_eq!(
        std::fs::read_to_string(repo.root().join("a.txt")).unwrap(),
        "bad\n"
    );
    repo.assert_no_leftovers();
}

#[tokio::test]
async fn partial_boundaries_skipped() {
    let repo = Repo::new();
    let log = repo.session(edit_breaks_at_3()).await;
    // A checkpoint skipped after step 2: its state is partial, never used.
    with_skip_after(&log, 2);
    let plan = bisect::plan(&reader(&log), "./check.sh", None, None).unwrap();
    // Step 1 is partial too when the background session-start checkpoint was logged
    // after step 2's records (it then lies after the skip).
    let early: Vec<u32> = plan.skipped.iter().copied().filter(|s| *s < 2).collect();
    assert!(early.is_empty() || early == [1], "{:?}", plan.skipped);
    let mut want_skipped = early.clone();
    want_skipped.push(2);
    assert_eq!(plan.skipped, want_skipped);
    let want_candidates: Vec<u32> = [1, 3, 4]
        .into_iter()
        .filter(|s| !early.contains(s))
        .collect();
    assert_eq!(plan.candidates, want_candidates);

    /// Judges by `a.txt`, and counts runs of step 2's state (x.txt there, a.txt good).
    #[derive(Default)]
    struct NeverTwo(Mutex<u32>);
    #[async_trait]
    impl TestRunner for NeverTwo {
        async fn run(&self, dir: &Path, _: &str, _: Duration) -> Verdict {
            let a = std::fs::read_to_string(dir.join("a.txt")).unwrap();
            if a.contains("bad") {
                return Verdict::Bad;
            }
            if dir.join("x.txt").exists() {
                *self.0.lock().unwrap() += 1;
            }
            Verdict::Good
        }
    }
    let runner = NeverTwo::default();
    let out = bisect::run(&plan, &repo.sandbox(), &runner, TIMEOUT)
        .await
        .unwrap();
    // Step 2 cannot be told apart from 3: the change is in one of them.
    assert_eq!(out.first_bad, Some(3));
    assert_eq!(out.last_good, if early.is_empty() { 1 } else { 0 });
    assert_eq!(out.uncertain, want_skipped);
    assert_eq!(out.skipped, want_skipped);
    // No run saw step 2's state.
    assert_eq!(*runner.0.lock().unwrap(), 0);
    assert!(out.runs <= 2 + 3);

    // A partial endpoint is refused.
    let e = bisect::plan(&reader(&log), "./check.sh", Some(2), None).unwrap_err();
    assert!(matches!(e, BisectError::Partial { step: 2, .. }), "{e:?}");
    repo.assert_no_leftovers();
}

#[tokio::test]
async fn exit_125_and_timeout_skip() {
    assert_eq!(Verdict::from_exit(Some(0)), Verdict::Good);
    assert_eq!(Verdict::from_exit(Some(1)), Verdict::Bad);
    assert_eq!(Verdict::from_exit(Some(127)), Verdict::Bad);
    assert_eq!(Verdict::from_exit(Some(125)), Verdict::Skip);
    assert_eq!(Verdict::from_exit(None), Verdict::Skip);

    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path().canonicalize().unwrap();
    let runner = BashRunner::new(Vec::new(), CancellationToken::new());
    let t = Duration::from_secs(5);
    assert_eq!(runner.run(&dir, "exit 0", t).await, Verdict::Good);
    assert_eq!(runner.run(&dir, "exit 3", t).await, Verdict::Bad);
    assert_eq!(runner.run(&dir, "exit 125", t).await, Verdict::Skip);
    // A timeout is a skip, and the whole group (a background child too) is killed.
    let started = Instant::now();
    let v = runner
        .run(
            &dir,
            "(sleep 30; touch late) & sleep 30",
            Duration::from_secs(1),
        )
        .await;
    assert_eq!(v, Verdict::Skip);
    assert!(started.elapsed() < Duration::from_secs(10));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!dir.join("late").exists());

    // A provider key is not in the check's environment.
    // SAFETY: the variable is only read by the child below.
    unsafe { std::env::set_var("TAUTLY_BISECT_TEST_KEY", "secret") };
    let runner = BashRunner::new(
        vec!["TAUTLY_BISECT_TEST_KEY".into()],
        CancellationToken::new(),
    );
    assert_eq!(
        runner
            .run(&dir, "test -z \"$TAUTLY_BISECT_TEST_KEY\"", t)
            .await,
        Verdict::Good
    );

    // In a bisect, a skipped state is reported and passed over.
    let repo = Repo::new();
    let log = repo.session(edit_breaks_at_3()).await;
    let plan = bisect::plan(&reader(&log), "./check.sh", None, None).unwrap();
    struct SkipsTwo;
    #[async_trait]
    impl TestRunner for SkipsTwo {
        async fn run(&self, dir: &Path, _: &str, _: Duration) -> Verdict {
            let a = std::fs::read_to_string(dir.join("a.txt")).unwrap();
            if a.contains("bad") {
                Verdict::Bad
            } else if dir.join("x.txt").exists() {
                // Step 2's state: x.txt, a.txt still good.
                Verdict::Skip
            } else {
                Verdict::Good
            }
        }
    }
    let out = bisect::run(&plan, &repo.sandbox(), &SkipsTwo, TIMEOUT)
        .await
        .unwrap();
    assert_eq!(out.first_bad, Some(3));
    assert_eq!(out.last_good, 1);
    assert_eq!(out.skipped, [2]);
    assert_eq!(out.uncertain, [2]);
    repo.assert_no_leftovers();
}

#[tokio::test]
async fn endpoint_mismatch_exits_2() {
    // The core side: an endpoint that does not verify is an error naming it; the CLI
    // maps it to exit 2 (crates/tautly-cli/tests/bisect.rs).
    let repo = Repo::new();
    let log = repo.session(edit_breaks_at_3()).await;
    let plan = bisect::plan(&reader(&log), "./check.sh", None, None).unwrap();
    struct Always(Verdict);
    #[async_trait]
    impl TestRunner for Always {
        async fn run(&self, _: &Path, _: &str, _: Duration) -> Verdict {
            self.0
        }
    }
    let e = bisect::run(&plan, &repo.sandbox(), &Always(Verdict::Bad), TIMEOUT)
        .await
        .unwrap_err();
    assert!(
        matches!(
            e,
            BisectError::EndpointMismatch {
                step: 0,
                expected: Verdict::Good,
                got: Verdict::Bad,
                from: Endpoint::Default,
                ..
            }
        ),
        "{e:?}"
    );
    let e = bisect::run(&plan, &repo.sandbox(), &Always(Verdict::Good), TIMEOUT)
        .await
        .unwrap_err();
    assert!(
        matches!(
            e,
            BisectError::EndpointMismatch {
                step: 5,
                expected: Verdict::Bad,
                got: Verdict::Good,
                from: Endpoint::Default,
                ..
            }
        ),
        "{e:?}"
    );
    // The error names the endpoint's origin and carries the run's last lines.
    let runner = BashRunner::new(Vec::new(), CancellationToken::new());
    let mut noisy = plan.clone();
    noisy.test = "for i in 1 2 3 4 5 6 7 8 9 10 11 12; do echo line $i; done; echo boom; \
                  exit 1"
        .into();
    let e = bisect::run(&noisy, &repo.sandbox(), &runner, TIMEOUT)
        .await
        .unwrap_err();
    assert!(
        e.to_string().contains("step 0 (good step, the default)"),
        "{e}"
    );
    let BisectError::EndpointMismatch {
        output: Some(output),
        ..
    } = &e
    else {
        panic!("{e:?}")
    };
    assert!(output.contains("boom"), "{output}");
    assert!(output.contains("exit code: 1"), "{output}");
    assert!(
        !output.contains("line 1\n"),
        "only the last lines: {output}"
    );
    // A check that is not there fails (exit 127), and the output says why.
    let mut absent = plan.clone();
    absent.test = "./no-such-check.sh".into();
    let e = bisect::run(&absent, &repo.sandbox(), &runner, TIMEOUT)
        .await
        .unwrap_err();
    let BisectError::EndpointMismatch {
        got: Verdict::Bad,
        output: Some(output),
        ..
    } = &e
    else {
        panic!("{e:?}")
    };
    assert!(output.contains("no-such-check.sh"), "{output}");
    repo.assert_no_leftovers();
}

#[tokio::test]
async fn prepass_narrows_range_with_fewer_runs() {
    let repo = Repo::new();
    // 1 user, 2 write p, 3 write q, 4 check (passes: boundary 3), 5 break a.txt, 6 check
    // (fails: boundary 5), 7..=12 writes, 13 text.
    let mut steps = vec![
        tools_step(vec![write("w1", "p.txt", "p\n")]),
        tools_step(vec![write("w2", "q.txt", "q\n")]),
        tools_step(vec![bash("b1", "./check.sh")]),
        tools_step(vec![write("w3", "a.txt", "bad\n")]),
        tools_step(vec![bash("b2", "  ./check.sh ")]),
    ];
    for i in 0..6 {
        steps.push(tools_step(vec![write(
            &format!("v{i}"),
            &format!("v{i}.txt"),
            "v\n",
        )]));
    }
    steps.push(text_step("done"));
    let log = repo.session(steps).await;
    let r = reader(&log);
    assert_eq!(bisect::prepass(&r, "./check.sh"), Some((3, 5)));
    // Another command was never logged.
    assert_eq!(bisect::prepass(&r, "sh ./check.sh"), None);

    let narrow = bisect::plan(&r, "./check.sh", None, None).unwrap();
    assert_eq!((narrow.good, narrow.bad), (3, 5));
    assert_eq!(narrow.candidates, [4]);
    let wide = bisect::plan(&r, "sh ./check.sh", None, None).unwrap();
    assert_eq!((wide.good, wide.bad), (0, 13));
    // Endpoints given explicitly are kept.
    let given = bisect::plan(&r, "./check.sh", Some(0), Some(13)).unwrap();
    assert_eq!((given.good, given.bad), (0, 13));

    let runner = BashRunner::new(Vec::new(), CancellationToken::new());
    let a = bisect::run(&narrow, &repo.sandbox(), &runner, TIMEOUT)
        .await
        .unwrap();
    let b = bisect::run(&wide, &repo.sandbox(), &runner, TIMEOUT)
        .await
        .unwrap();
    assert_eq!(a.first_bad, Some(5));
    assert_eq!(b.first_bad, Some(5));
    assert!(a.runs < b.runs, "{} vs {}", a.runs, b.runs);
    assert_eq!(a.runs, 3);
    repo.assert_no_leftovers();
}

#[tokio::test]
async fn runs_in_temp_worktree_original_untouched() {
    let repo = Repo::new();
    let log = repo.session(edit_breaks_at_3()).await;
    // A human edit and an ignored file in the user's tree.
    repo.write("a.txt", "human\n");
    repo.write("cache/mine", "keep\n");
    let before = repo.snapshot();
    let project = reader(&log).header().project.clone();

    let plan = bisect::plan(&reader(&log), "./check.sh", None, None).unwrap();
    let probe = Probe {
        litter: true,
        ..Probe::default()
    };
    let out = bisect::run(&plan, &repo.sandbox(), &probe, TIMEOUT)
        .await
        .unwrap();
    assert_eq!(out.first_bad, Some(3));

    let runs = probe.runs.lock().unwrap().clone();
    assert_eq!(runs.len() as u32, out.runs);
    let base = repo.home().join("worktrees").join(&project);
    let dir = &runs[0].0;
    for (d, _) in &runs {
        // One worktree, reused.
        assert_eq!(d, dir);
    }
    assert!(dir.starts_with(&base), "{dir:?}");
    let name = dir.file_name().unwrap().to_string_lossy().into_owned();
    assert!(name.starts_with("bisect-"), "{name}");
    assert_ne!(dir, &repo.root());
    // Each run saw a past state, never the human edit.
    assert!(runs.iter().all(|(_, a)| a != "human\n"), "{runs:?}");
    // Ignored files are kept between runs; other leftovers are cleaned.
    let found = probe.found.lock().unwrap().clone();
    assert_eq!(found[0], (false, false));
    for f in &found[1..] {
        assert_eq!(*f, (true, false), "{found:?}");
    }

    assert_eq!(repo.snapshot(), before);
    repo.assert_no_leftovers();
}

#[tokio::test]
async fn worktree_removed_on_success_and_error() {
    let repo = Repo::new();
    let log = repo.session(edit_breaks_at_3()).await;
    let plan = bisect::plan(&reader(&log), "./check.sh", None, None).unwrap();

    // Success.
    let out = bisect::run(&plan, &repo.sandbox(), &Probe::default(), TIMEOUT).await;
    assert!(out.is_ok(), "{out:?}");
    repo.assert_no_leftovers();

    // An error (a cancel after the first run): removed too.
    let probe = Probe {
        cancel_after: Some(1),
        litter: true,
        ..Probe::default()
    };
    let e = bisect::run(&plan, &repo.sandbox(), &probe, TIMEOUT)
        .await
        .unwrap_err();
    assert!(matches!(e, BisectError::Cancelled), "{e:?}");
    assert_eq!(probe.runs.lock().unwrap().len(), 1);
    repo.assert_no_leftovers();

    // A panicking runner: the worktree goes too, and the panic goes on.
    struct Panics;
    #[async_trait]
    impl TestRunner for Panics {
        async fn run(&self, _: &Path, _: &str, _: Duration) -> Verdict {
            panic!("the runner broke");
        }
    }
    let (sb, p) = (repo.sandbox(), plan.clone());
    let joined = tokio::spawn(async move { bisect::run(&p, &sb, &Panics, TIMEOUT).await }).await;
    assert!(joined.unwrap_err().is_panic());
    repo.assert_no_leftovers();

    // What a crashed bisect left (its marked worktree, its lock nobody holds) goes at the
    // next one.
    let base = repo
        .home()
        .join("worktrees")
        .join(reader(&log).header().project.clone());
    let crashed = crashed_bisect(&repo, &base, "01JBXZ3QK8R9T0V5W6X7Y8Z9AB");
    std::fs::write(crashed.join("junk.txt"), "x").unwrap();
    assert_eq!(repo.leftovers().0.len(), 1);
    let out = bisect::run(&plan, &repo.sandbox(), &Probe::default(), TIMEOUT).await;
    assert!(out.is_ok(), "{out:?}");
    repo.assert_no_leftovers();

    // An orphaned lock (a crash before the worktree was made) and a marked entry whose
    // directory is already gone go too.
    std::fs::write(base.join("bisect-01JBXZ3QK8R9T0V5W6X7Y8Z9AC.lock"), "").unwrap();
    let gone = crashed_bisect(&repo, &base, "01JBXZ3QK8R9T0V5W6X7Y8Z9AD");
    std::fs::remove_dir_all(&gone).unwrap();
    assert_eq!(repo.leftovers().0.len(), 1);
    let out = bisect::run(&plan, &repo.sandbox(), &Probe::default(), TIMEOUT).await;
    assert!(out.is_ok(), "{out:?}");
    repo.assert_no_leftovers();
}

/// What a bisect that crashed mid-run leaves under `base`: its worktree `bisect-<ulid>`
/// (with the marker in its administrative directory) and its lock file, held by nobody.
fn crashed_bisect(repo: &Repo, base: &Path, ulid: &str) -> PathBuf {
    let dir = base.join(format!("bisect-{ulid}"));
    std::fs::create_dir_all(base).unwrap();
    std::fs::write(base.join(format!("bisect-{ulid}.lock")), "").unwrap();
    repo.git(&[
        "worktree",
        "add",
        "-q",
        "--detach",
        dir.to_str().unwrap(),
        "HEAD",
    ])
    .unwrap();
    let admin = std::fs::read_to_string(dir.join(".git")).unwrap();
    let admin = PathBuf::from(admin.trim().strip_prefix("gitdir: ").unwrap());
    std::fs::write(admin.join(bisect::MARKER), "").unwrap();
    dir
}

#[tokio::test]
async fn sweep_leaves_worktrees_it_did_not_make() {
    let repo = Repo::new();
    let log = repo.session(edit_breaks_at_3()).await;
    let plan = bisect::plan(&reader(&log), "./check.sh", None, None).unwrap();
    let base = repo
        .home()
        .join("worktrees")
        .join(reader(&log).header().project.clone());
    std::fs::create_dir_all(&base).unwrap();

    // The user's own worktree named like a bisect's, with work in it.
    let mine = base.join("bisect-mine");
    repo.git(&[
        "worktree",
        "add",
        "-q",
        "--detach",
        mine.to_str().unwrap(),
        "HEAD",
    ])
    .unwrap();
    std::fs::write(mine.join("work.txt"), "mine").unwrap();
    // A fork (Task 50) placed there with --dir, with work in it.
    let fork = tautly_core::worktree::fork_to_worktree(
        Arc::new(repo.sandbox()),
        &repo.home(),
        &log,
        tautly_core::worktree::ForkRequest {
            at: Some(2),
            dir: Some(base.join("bisect-fork")),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    std::fs::write(fork.worktree.join("work.txt"), "fork").unwrap();
    // A worktree with a bisect's name and a lock nobody holds, but no marker: not
    // provably a bisect's, so it stays with its lock.
    let unmarked = base.join("bisect-01JBXZ3QK8R9T0V5W6X7Y8Z9AE");
    std::fs::write(base.join("bisect-01JBXZ3QK8R9T0V5W6X7Y8Z9AE.lock"), "").unwrap();
    repo.git(&[
        "worktree",
        "add",
        "-q",
        "--detach",
        unmarked.to_str().unwrap(),
        "HEAD",
    ])
    .unwrap();
    // A marked bisect worktree whose lock is held: a live bisect's.
    let live = crashed_bisect(&repo, &base, "01JBXZ3QK8R9T0V5W6X7Y8Z9AF");
    let held = std::fs::OpenOptions::new()
        .write(true)
        .open(base.join("bisect-01JBXZ3QK8R9T0V5W6X7Y8Z9AF.lock"))
        .unwrap();
    // SAFETY: flock on a descriptor this test owns.
    assert_eq!(
        unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&held), libc::LOCK_EX) },
        0
    );

    let out = bisect::run(&plan, &repo.sandbox(), &Probe::default(), TIMEOUT).await;
    assert!(out.is_ok(), "{out:?}");

    assert_eq!(
        std::fs::read_to_string(mine.join("work.txt")).unwrap(),
        "mine"
    );
    assert_eq!(
        std::fs::read_to_string(fork.worktree.join("work.txt")).unwrap(),
        "fork"
    );
    assert!(unmarked.join(".git").exists());
    assert!(base.join("bisect-01JBXZ3QK8R9T0V5W6X7Y8Z9AE.lock").exists());
    assert!(live.join(".git").exists());
    // Git still knows all four.
    assert_eq!(repo.leftovers().0.len(), 4, "{:?}", repo.leftovers());
    drop(held);
}

#[tokio::test]
async fn a_logged_endpoint_that_does_not_hold_falls_back() {
    let repo = Repo::new();
    // As in the pre-pass test: logged runs pass at boundary 3 and fail at 5.
    let log = repo
        .session(vec![
            tools_step(vec![write("w1", "p.txt", "p\n")]),
            tools_step(vec![write("w2", "q.txt", "q\n")]),
            tools_step(vec![bash("b1", "./check.sh")]),
            tools_step(vec![write("w3", "a.txt", "bad\n")]),
            tools_step(vec![bash("b2", "./check.sh")]),
            tools_step(vec![write("w4", "r.txt", "r\n")]),
            text_step("done"),
        ])
        .await;
    let plan = bisect::plan(&reader(&log), "./check.sh", None, None).unwrap();
    assert_eq!((plan.good, plan.bad), (3, 5));
    assert_eq!(
        (plan.good_from, plan.bad_from),
        (Endpoint::Logged, Endpoint::Logged)
    );
    // At most every exact boundary from the defaults, each once.
    assert_eq!(plan.max_runs(), 9);
    // Now the check also fails once q.txt is there (step 3): the logged good step 3 does
    // not hold, and the default 0 is used.
    struct Strict;
    #[async_trait]
    impl TestRunner for Strict {
        async fn run(&self, dir: &Path, _: &str, _: Duration) -> Verdict {
            let a = std::fs::read_to_string(dir.join("a.txt")).unwrap();
            if a.contains("bad") || dir.join("q.txt").exists() {
                Verdict::Bad
            } else {
                Verdict::Good
            }
        }
    }
    let out = bisect::run(&plan, &repo.sandbox(), &Strict, TIMEOUT)
        .await
        .unwrap();
    assert_eq!(out.first_bad, Some(3));
    assert_eq!(out.last_good, 2);
    assert_eq!(out.notes.len(), 1, "{:?}", out.notes);
    assert!(out.notes[0].contains("good step 3"), "{:?}", out.notes);
    assert!(out.notes[0].contains("step 0"), "{:?}", out.notes);
    repo.assert_no_leftovers();
}

#[tokio::test]
async fn a_checkpoint_without_a_known_point_is_partial() {
    for point in [Value::Null, json!("from_a_newer_version")] {
        let repo = Repo::new();
        let log = repo.session(edit_breaks_at_3()).await;
        // The turn's end checkpoint, as a build without `point` (or a newer one) wrote it.
        let text = std::fs::read_to_string(&log).unwrap();
        let mut lines: Vec<Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let end = lines
            .iter_mut()
            .rev()
            .find(|l| l["data"]["type"] == "checkpoint")
            .unwrap();
        assert_eq!(end["data"]["data"]["point"], "turn_end");
        end["data"]["data"]["point"] = point.clone();
        let out: String = lines.iter().map(|l| format!("{l}\n")).collect();
        std::fs::write(&log, out).unwrap();
        // Every boundary after step 1 reads that checkpoint: the tip is partial.
        let e = bisect::plan(&reader(&log), "./check.sh", None, None).unwrap_err();
        assert!(
            matches!(e, BisectError::Partial { step: 5, .. }),
            "{point}: {e:?}"
        );
        let e = bisect::plan(&reader(&log), "./check.sh", Some(0), Some(2)).unwrap_err();
        assert!(
            matches!(e, BisectError::Partial { step: 2, .. }),
            "{point}: {e:?}"
        );
    }
}
