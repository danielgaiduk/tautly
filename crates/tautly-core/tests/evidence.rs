//! The evidence footer (spec 7.11.7): test-run detection, the tracker's staleness rules,
//! the `[evidence]` config layering and `TurnFinished` through a real engine turn.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tautly_core::{
    CliOverrides, Command, Config, DEFAULT_TEST_COMMANDS, EngineBuilder, EngineError, Event,
    EvidenceConfig, EvidenceTracker, FinishReason, TestEvidence, TestMatcher,
};
use tautly_provider::{
    Catalog, MockProvider, MockTurn, ModelId, ProviderEvent, RawUsage, StopReason,
};
use tautly_sandbox::Mode;
use tautly_tools::{AppliedEffect, TestCounts, ToolMeta, ToolResult};
use tempfile::TempDir;

// ---- test-run detection ----

fn matcher(cfg: &EvidenceConfig) -> TestMatcher {
    TestMatcher::new(cfg).unwrap()
}

#[test]
fn default_patterns_detect_common_test_commands() {
    let m = matcher(&EvidenceConfig::default());
    for cmd in [
        "cargo test -p x",
        "cd a && pytest -q",
        "npm run test -- --watch=false",
        "go test ./...",
        "cargo nextest run",
        "pnpm test",
        "yarn test --ci",
        "python3 -m pytest tests/",
        "uv run pytest",
        "npx vitest run",
        "make test-all",
        "./gradlew test",
        "bundle exec rspec spec/",
        "dotnet test",
        "FOO=1 cargo test",
        "cargo build && cargo test",
        "(cd web; npm test)",
    ] {
        assert!(m.is_test(cmd), "{cmd} should be a test run");
    }
    for cmd in [
        "cargo build",
        "echo pytest",
        "ls",
        "git commit -m 'cargo test'",
        "cat tests.txt",
        "cargo test; unterminated '",
        "",
    ] {
        assert!(!m.is_test(cmd), "{cmd} should not be a test run");
    }
    // Every default is a valid pattern and detects itself.
    for p in DEFAULT_TEST_COMMANDS {
        let cmd = p.trim_end_matches('*');
        assert!(m.is_test(cmd), "{p}");
    }
}

#[test]
fn user_patterns_add_and_replace_defaults() {
    let add = EvidenceConfig {
        test_commands: vec!["./check.sh*".into(), "tox*".into()],
        replace_defaults: false,
    };
    let m = matcher(&add);
    assert!(m.is_test("./check.sh --fast"));
    assert!(m.is_test("cd x && tox -e py"));
    assert!(m.is_test("cargo test"), "defaults stay");

    let replace = EvidenceConfig {
        replace_defaults: true,
        ..add
    };
    let m = matcher(&replace);
    assert!(m.is_test("./check.sh"));
    assert!(!m.is_test("cargo test"), "defaults replaced");
    assert!(!m.is_test("pytest"));

    // Replacing with nothing means nothing is a test run.
    let none = EvidenceConfig {
        test_commands: vec![],
        replace_defaults: true,
    };
    assert!(!matcher(&none).is_test("cargo test"));

    // A malformed pattern is an error naming it, not silently ignored.
    for bad in ["[abc", "", "  "] {
        let cfg = EvidenceConfig {
            test_commands: vec![bad.into()],
            replace_defaults: false,
        };
        let err = TestMatcher::new(&cfg).unwrap_err().to_string();
        assert!(err.contains("malformed rule"), "{bad:?}: {err}");
    }
}

// ---- the tracker ----

fn tracker() -> EvidenceTracker {
    EvidenceTracker::new(Arc::new(matcher(&EvidenceConfig::default())))
}

fn bash(code: i32, counts: Option<(u32, u32)>) -> ToolResult {
    let mut r = ToolResult::ok("out");
    r.is_error = code != 0;
    r.effects.push(AppliedEffect::Exec { may_write: true });
    let mut meta = ToolMeta::default();
    meta.exit_code = Some(code);
    meta.counts = counts.map(|(passed, failed)| TestCounts { passed, failed });
    r.meta = meta;
    r
}

fn edit(path: &str) -> ToolResult {
    let mut r = ToolResult::ok("edited");
    r.effects.push(AppliedEffect::Edited {
        path: PathBuf::from(path),
        pre_image: None,
        pre_mode: None,
        post_hash: blake3::hash(b"x"),
    });
    r
}

fn delete(path: &str) -> ToolResult {
    let mut r = ToolResult::ok("deleted");
    r.effects.push(AppliedEffect::Deleted {
        path: PathBuf::from(path),
        pre_image: b"x".to_vec(),
        pre_mode: 0o644,
    });
    r
}

#[test]
fn evidence_reports_last_test_run_with_counts() {
    let mut t = tracker();
    t.on_tool(1, Some("cargo test -p a"), &bash(0, Some((9, 0))), 3);
    t.on_tool(
        2,
        Some("  cargo   test\n  -p b  "),
        &bash(101, Some((7, 2))),
        5,
    );
    let e = t.finish().expect("a test ran");
    assert_eq!(
        e,
        TestEvidence {
            command: "cargo test -p b".into(),
            exit_code: Some(101),
            passed: Some(7),
            failed: Some(2),
            step: 2,
            write_gen: 5,
            stale: false,
            edits_since: 0,
            execs_since: 0,
        }
    );

    // Without parsed counts, or killed before an exit code.
    let mut t = tracker();
    let mut killed = bash(0, None);
    killed.meta = ToolMeta::default();
    t.on_tool(4, Some("pytest"), &killed, 9);
    let e = t.finish().unwrap();
    assert_eq!((e.exit_code, e.passed, e.failed), (None, None, None));
}

#[test]
fn long_commands_are_cut_to_200_chars() {
    let mut t = tracker();
    let cmd = format!("cargo test {}", "x".repeat(500));
    t.on_tool(1, Some(&cmd), &bash(0, None), 1);
    let c = t.finish().unwrap().command;
    assert_eq!(c.chars().count(), 200);
    assert!(c.starts_with("cargo test xxx"));
}

#[test]
fn edit_after_test_marks_stale_with_counts() {
    let mut t = tracker();
    t.on_tool(1, Some("cargo test"), &bash(0, Some((5, 0))), 2);
    t.on_tool(2, None, &edit("src/a.rs"), 3);
    t.on_tool(3, None, &delete("src/b.rs"), 4);
    t.on_tool(3, None, &edit("src/c.rs"), 5);
    let e = t.finish().unwrap();
    assert!(e.stale);
    assert_eq!((e.edits_since, e.execs_since), (3, 0));
    assert_eq!(
        (e.step, e.write_gen),
        (1, 2),
        "the run's own step and generation"
    );
}

#[test]
fn test_after_test_is_fresh() {
    let mut t = tracker();
    t.on_tool(1, Some("cargo test"), &bash(1, Some((4, 1))), 2);
    t.on_tool(2, None, &edit("src/a.rs"), 3);
    t.on_tool(3, Some("cargo test"), &bash(0, Some((5, 0))), 4);
    let e = t.finish().unwrap();
    assert!(!e.stale, "{e:?}");
    assert_eq!((e.edits_since, e.execs_since), (0, 0));
    assert_eq!((e.step, e.passed), (3, Some(5)));
}

#[test]
fn non_test_exec_after_test_marks_stale() {
    let mut t = tracker();
    t.on_tool(1, Some("cargo test"), &bash(0, Some((5, 0))), 2);
    t.on_tool(2, Some("cargo build"), &bash(0, None), 3);
    let e = t.finish().unwrap();
    assert!(e.stale);
    assert_eq!((e.edits_since, e.execs_since), (0, 1));

    // A command that cannot have written, and plain reads, do not make it stale.
    let mut t = tracker();
    t.on_tool(1, Some("cargo test"), &bash(0, Some((5, 0))), 2);
    let mut quiet = ToolResult::ok("x");
    quiet.effects.push(AppliedEffect::Exec { may_write: false });
    t.on_tool(2, Some("ls"), &quiet, 2);
    t.on_tool(3, None, &ToolResult::ok("a read"), 2);
    let e = t.finish().unwrap();
    assert!(!e.stale, "{e:?}");

    // A test run is not a stale-maker for an earlier test run.
    let mut t = tracker();
    t.on_tool(1, Some("cargo test -p a"), &bash(0, Some((1, 0))), 2);
    t.on_tool(2, Some("cd b && go test ./..."), &bash(0, None), 3);
    let e = t.finish().unwrap();
    assert!(!e.stale);
    assert_eq!(e.command, "cd b && go test ./...");
}

#[test]
fn an_abandoned_write_after_the_test_marks_it_stale() {
    let mut t = tracker();
    t.on_tool(1, Some("cargo test"), &bash(0, Some((1, 0))), 2);
    t.on_lost_write();
    let e = t.finish().unwrap();
    assert!(e.stale && e.execs_since == 1, "{e:?}");
}

#[test]
fn no_test_run_gives_none() {
    assert_eq!(tracker().finish(), None);
    let mut t = tracker();
    t.on_tool(1, None, &edit("a"), 1);
    t.on_tool(2, Some("cargo build"), &bash(0, None), 2);
    t.on_tool(3, Some("echo pytest"), &bash(0, None), 3);
    assert_eq!(t.finish(), None);
    // Edits before the test do not count against it.
    let mut t = tracker();
    t.on_tool(1, None, &edit("a"), 1);
    t.on_tool(2, Some("cargo test"), &bash(0, Some((1, 0))), 2);
    assert!(!t.finish().unwrap().stale);
}

// ---- config ----

struct Fixture {
    dir: TempDir,
}

impl Fixture {
    fn new() -> Self {
        let f = Fixture {
            dir: TempDir::new().unwrap(),
        };
        std::fs::create_dir_all(f.root()).unwrap();
        f
    }

    fn root(&self) -> PathBuf {
        self.dir.path().join("project")
    }

    fn user_file(&self) -> PathBuf {
        self.dir.path().join("user").join("config.toml")
    }

    fn user(&self, toml: &str) {
        let p = self.user_file();
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, toml).unwrap();
    }

    fn project(&self, toml: &str) {
        let p = self.root().join(".tautly").join("config.toml");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, toml).unwrap();
    }

    fn load(&self) -> tautly_core::LoadedConfig {
        Config::load_from(
            Some(&self.user_file()),
            &self.root(),
            CliOverrides::default(),
        )
        .unwrap()
    }
}

#[test]
fn project_layer_cannot_set_evidence() {
    let f = Fixture::new();
    f.user("[evidence]\ntest_commands = [\"./mine.sh*\"]\n");
    f.project("[evidence]\ntest_commands = [\"true\"]\nreplace_defaults = true\n");
    let loaded = f.load();
    assert_eq!(
        loaded.config.evidence,
        EvidenceConfig {
            test_commands: vec!["./mine.sh*".into()],
            replace_defaults: false,
        },
        "the user's setting stands"
    );
    assert!(
        loaded.warnings.iter().any(|w| w.contains(".tautly")
            && w.contains("`evidence`")
            && w.contains("may not set")),
        "{:?}",
        loaded.warnings
    );

    // With no user layer the default stays.
    let f = Fixture::new();
    f.project("[evidence]\ntest_commands = [\"true\"]\n");
    let loaded = Config::load_from(None, &f.root(), CliOverrides::default()).unwrap();
    assert_eq!(loaded.config.evidence, EvidenceConfig::default());
    assert!(!matcher(&loaded.config.evidence).is_test("true"));
}

#[test]
fn user_evidence_config_is_validated() {
    let f = Fixture::new();
    f.user("[evidence]\ntest_commands = [\"[abc\"]\n");
    let err = Config::load_from(Some(&f.user_file()), &f.root(), CliOverrides::default())
        .unwrap_err()
        .to_string();
    assert!(err.contains("evidence") || err.contains("[abc"), "{err}");
    let f = Fixture::new();
    f.user("[evidence]\nunknown_key = 1\n");
    assert!(Config::load_from(Some(&f.user_file()), &f.root(), CliOverrides::default()).is_err());
}

// ---- the wire ----

#[test]
fn old_turn_finished_json_without_evidence_parses() {
    let e: Event =
        serde_json::from_value(json!({"type": "turn_finished", "reason": "completed"})).unwrap();
    assert_eq!(
        e,
        Event::TurnFinished {
            reason: FinishReason::Completed,
            evidence: None
        }
    );
    // Not written when absent, so readers of the first list see the old shape.
    assert_eq!(
        serde_json::to_value(&e).unwrap(),
        json!({"type": "turn_finished", "reason": "completed"})
    );

    let evidence = TestEvidence {
        command: "cargo test".into(),
        exit_code: Some(0),
        passed: Some(3),
        failed: Some(0),
        step: 2,
        write_gen: 7,
        stale: true,
        edits_since: 1,
        execs_since: 0,
    };
    let e = Event::TurnFinished {
        reason: FinishReason::Completed,
        evidence: Some(evidence),
    };
    let v = serde_json::to_value(&e).unwrap();
    assert_eq!(v["evidence"]["command"], "cargo test");
    assert_eq!(v["evidence"]["stale"], true);
    assert_eq!(serde_json::from_value::<Event>(v).unwrap(), e);
}

// ---- through the engine ----

const MODEL: &str = "mock/test";
const WAIT: Duration = Duration::from_secs(30);

fn config(patterns: &[&str]) -> Config {
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
        evidence: EvidenceConfig {
            test_commands: patterns.iter().map(|p| p.to_string()).collect(),
            replace_defaults: true,
        },
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

fn tools_step(calls: Vec<ProviderEvent>) -> MockTurn {
    let mut evs = calls;
    evs.push(ProviderEvent::Done {
        stop: StopReason::ToolUse,
    });
    MockTurn::Events(evs)
}

fn text_step() -> MockTurn {
    MockTurn::Events(vec![
        ProviderEvent::TextDelta("done".into()),
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

/// A project whose `./cargo` script prints a passing libtest summary.
fn project() -> (TempDir, TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("cargo");
    std::fs::write(
        &script,
        "#!/bin/sh\necho 'running 4 tests'\necho 'test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s'\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    (dir, tempfile::tempdir().unwrap())
}

async fn turn(
    engine: &tautly_core::Engine,
    events: &mut tokio::sync::mpsc::Receiver<Event>,
    text: &str,
) -> Option<TestEvidence> {
    engine
        .send(Command::SendMessage { text: text.into() })
        .await
        .unwrap();
    loop {
        let ev = tokio::time::timeout(WAIT, events.recv())
            .await
            .expect("timed out")
            .expect("event channel closed");
        if let Event::TurnFinished { reason, evidence } = ev {
            assert_eq!(reason, FinishReason::Completed);
            return evidence;
        }
    }
}

fn start(
    dir: &TempDir,
    home: &TempDir,
    patterns: &[&str],
    script: Vec<MockTurn>,
) -> (tautly_core::Engine, tokio::sync::mpsc::Receiver<Event>) {
    let mock = MockProvider::new(script);
    EngineBuilder {
        provider: Some(Arc::new(mock)),
        data_home: Some(home.path().to_path_buf()),
        ..EngineBuilder::new(config(patterns), dir.path())
    }
    .build()
    .unwrap()
}

#[tokio::test]
async fn turn_finished_carries_the_last_test_run_and_its_staleness() {
    let (dir, home) = project();
    let write = |id: &str| call(id, "write", json!({"path": "out.txt", "content": "x"}));
    let test = |id: &str| call(id, "bash", json!({"command": "./cargo test -p x"}));
    let (engine, mut events) = start(
        &dir,
        &home,
        &["./cargo test*"],
        vec![
            // Turn 1: test, then an edit.
            tools_step(vec![test("t1")]),
            tools_step(vec![write("w1")]),
            text_step(),
            // Turn 2: test, edit, test again.
            tools_step(vec![test("t2")]),
            tools_step(vec![write("w2")]),
            tools_step(vec![test("t3")]),
            text_step(),
            // Turn 3: an edit only.
            tools_step(vec![write("w3")]),
            text_step(),
            // Turn 4: nothing at all.
            text_step(),
        ],
    );

    let e = turn(&engine, &mut events, "one").await.expect("evidence");
    assert_eq!(e.command, "./cargo test -p x");
    assert_eq!(e.exit_code, Some(0));
    assert_eq!((e.passed, e.failed), (Some(4), Some(0)), "{e:?}");
    assert!(e.stale, "{e:?}");
    assert_eq!((e.edits_since, e.execs_since), (1, 0));
    assert!(e.write_gen >= 1);

    let e = turn(&engine, &mut events, "two").await.expect("evidence");
    assert!(!e.stale, "{e:?}");
    assert_eq!((e.edits_since, e.execs_since), (0, 0));
    assert_eq!((e.passed, e.failed), (Some(4), Some(0)));

    // A turn that edited without testing, and one that did nothing, report none: nothing
    // carries over from the earlier turns.
    assert_eq!(turn(&engine, &mut events, "three").await, None);
    assert_eq!(turn(&engine, &mut events, "four").await, None);
}

#[tokio::test]
async fn a_later_command_that_may_write_makes_the_run_stale() {
    let (dir, home) = project();
    let (engine, mut events) = start(
        &dir,
        &home,
        &["./cargo test*"],
        vec![
            tools_step(vec![call("t1", "bash", json!({"command": "./cargo test"}))]),
            tools_step(vec![call("b1", "bash", json!({"command": "touch built"}))]),
            text_step(),
        ],
    );
    let e = turn(&engine, &mut events, "go").await.expect("evidence");
    assert!(e.stale);
    assert_eq!((e.edits_since, e.execs_since), (0, 1));
}

#[tokio::test]
async fn an_invalid_evidence_pattern_stops_the_engine_from_starting() {
    let (dir, home) = project();
    let mock = MockProvider::new(vec![text_step()]);
    let err = EngineBuilder {
        provider: Some(Arc::new(mock)),
        data_home: Some(home.path().to_path_buf()),
        ..EngineBuilder::new(config(&["[abc"]), dir.path())
    }
    .build()
    .err()
    .expect("build fails");
    assert!(matches!(err, EngineError::Evidence(_)), "{err}");
}
