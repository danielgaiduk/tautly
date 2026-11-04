//! Session rules ("allow for this session", spec 7.11.8 and 5.5): `Approve{session_rule}`
//! through the engine with a scripted provider and a real sandbox on a temporary directory.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tautly_core::{Command, Config, Engine, EngineBuilder, Event, FinishReason};
use tautly_provider::{Catalog, MockProvider, MockTurn, ModelId, ProviderEvent, StopReason};
use tautly_sandbox::Mode;
use tempfile::TempDir;
use tokio::sync::mpsc;

const MODEL: &str = "mock/test";
const WAIT: Duration = Duration::from_secs(30);

fn config(deny: &[&str]) -> Config {
    let catalog = Catalog::from_toml(&format!(
        r#"[models."{MODEL}"]
context_window = 200000
max_output = 8000
parallel_tools = false
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
    c.permissions.mode = Mode::Ask;
    c.permissions.deny = deny.iter().map(|d| d.to_string()).collect();
    c.checkpoints.enabled = false;
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

fn step(c: ProviderEvent) -> MockTurn {
    MockTurn::Events(vec![
        c,
        ProviderEvent::Done {
            stop: StopReason::ToolUse,
        },
    ])
}

fn text_step(text: &str) -> MockTurn {
    MockTurn::Events(vec![
        ProviderEvent::TextDelta(text.into()),
        ProviderEvent::Done {
            stop: StopReason::EndTurn,
        },
    ])
}

struct Fx {
    dir: TempDir,
    home: TempDir,
}

impl Fx {
    fn new() -> Fx {
        Fx {
            dir: tempfile::tempdir().unwrap(),
            home: tempfile::tempdir().unwrap(),
        }
    }

    fn start(&self, cfg: Config, mock: &MockProvider, resume: Option<String>) -> Run {
        let (engine, events) = EngineBuilder {
            provider: Some(Arc::new(mock.clone())),
            data_home: Some(self.home.path().to_path_buf()),
            interactive: true,
            resume,
            ..EngineBuilder::new(cfg, self.dir.path())
        }
        .build()
        .unwrap();
        Run { engine, events }
    }
}

struct Run {
    engine: Engine,
    events: mpsc::Receiver<Event>,
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

    /// Events up to the next approval request (its id) or the end of the turn (None).
    async fn until_ask_or_end(&mut self, seen: &mut Vec<Event>) -> Option<String> {
        loop {
            let ev = self.next().await;
            seen.push(ev.clone());
            match ev {
                Event::ToolApprovalRequested { request_id, .. } => return Some(request_id),
                Event::TurnFinished { .. } => return None,
                _ => {}
            }
        }
    }

    async fn approve(&self, request_id: String, rule: Option<&str>) {
        self.send(Command::Approve {
            request_id,
            session_rule: rule.map(str::to_string),
        })
        .await;
    }

    async fn shutdown(mut self) {
        self.send(Command::Shutdown).await;
        while tokio::time::timeout(WAIT, self.events.recv())
            .await
            .expect("timed out waiting for the shutdown")
            .is_some()
        {}
    }
}

fn asks(events: &[Event]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, Event::ToolApprovalRequested { .. }))
        .count()
}

/// The warnings about session rules (others, such as an index still building, may come).
fn warnings(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::Warning { message } if message.contains("session rule") => Some(message.clone()),
            _ => None,
        })
        .collect()
}

/// `(call_id, is_error, output)` of every `ToolFinished`.
fn finished(events: &[Event]) -> Vec<(String, bool, String)> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::ToolFinished {
                call_id,
                is_error,
                output,
                ..
            } => Some((call_id.clone(), *is_error, output.clone())),
            _ => None,
        })
        .collect()
}

fn finish_reason(events: &[Event]) -> Option<FinishReason> {
    events.iter().rev().find_map(|e| match e {
        Event::TurnFinished { reason, .. } => Some(*reason),
        _ => None,
    })
}

#[tokio::test]
async fn session_rule_allows_matching_call() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![
        step(bash("b1", "echo  hi")),
        step(bash("b2", "echo hi")),
        text_step("done"),
    ]);
    let mut run = fx.start(config(&[]), &mock, None);
    run.send(Command::SendMessage { text: "go".into() }).await;
    let mut seen = Vec::new();
    let id = run
        .until_ask_or_end(&mut seen)
        .await
        .expect("first call asks");
    run.approve(id, Some("bash(echo hi)")).await;
    let end = run.until_ask_or_end(&mut seen).await;
    assert_eq!(
        end, None,
        "the second identical call is not asked: {seen:?}"
    );
    assert_eq!(asks(&seen), 1);
    assert_eq!(finish_reason(&seen), Some(FinishReason::Completed));
    let f = finished(&seen);
    assert_eq!(f.len(), 2, "{f:?}");
    assert!(f.iter().all(|(_, err, _)| !err), "{f:?}");
    assert!(warnings(&seen).is_empty(), "{seen:?}");
    run.shutdown().await;
}

#[tokio::test]
async fn session_rule_not_persisted_and_gone_after_resume() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![step(bash("b1", "echo hi")), text_step("done")]);
    let mut run = fx.start(config(&[]), &mock, None);
    let id = run.engine.session_id().to_string();
    let log = run.engine.session_path().to_path_buf();
    run.send(Command::SendMessage { text: "go".into() }).await;
    let mut seen = Vec::new();
    let req = run.until_ask_or_end(&mut seen).await.expect("asks");
    run.approve(req, Some("bash(echo hi)")).await;
    assert_eq!(run.until_ask_or_end(&mut seen).await, None);
    run.shutdown().await;

    // Never written anywhere: not to the session log (as consent or otherwise), not to a
    // configuration file.
    let text = std::fs::read_to_string(&log).unwrap();
    assert!(!text.contains("session_rule"), "{text}");
    assert!(!text.contains("bash(echo hi)"), "{text}");
    assert!(!fx.dir.path().join(".tautly/config.toml").exists());

    let mock = MockProvider::new(vec![step(bash("b2", "echo hi")), text_step("done")]);
    let mut run = fx.start(config(&[]), &mock, Some(id));
    run.send(Command::SendMessage {
        text: "again".into(),
    })
    .await;
    let mut seen = Vec::new();
    let req = run.until_ask_or_end(&mut seen).await;
    assert!(req.is_some(), "the resumed session asks again: {seen:?}");
    run.approve(req.unwrap(), None).await;
    assert_eq!(run.until_ask_or_end(&mut seen).await, None);
    run.shutdown().await;
}

#[tokio::test]
async fn deny_rule_beats_session_rule() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![
        step(bash("b1", "echo hi")),
        step(bash("b2", "echo bye")),
        text_step("done"),
    ]);
    let mut run = fx.start(config(&["bash(echo bye*)"]), &mock, None);
    run.send(Command::SendMessage { text: "go".into() }).await;
    let mut seen = Vec::new();
    let req = run.until_ask_or_end(&mut seen).await.expect("asks");
    run.approve(req, Some("bash(echo*)")).await;
    assert_eq!(run.until_ask_or_end(&mut seen).await, None, "{seen:?}");
    assert!(
        warnings(&seen).is_empty(),
        "the rule was accepted: {seen:?}"
    );
    let f = finished(&seen);
    let b2 = f.iter().find(|f| f.0 == "b2").expect("b2 finished");
    assert!(b2.1, "{b2:?}");
    assert!(b2.2.contains("denied by the permission rule"), "{b2:?}");
    run.shutdown().await;
}

#[tokio::test]
async fn protected_path_still_asks_with_session_rule() {
    let fx = Fx::new();
    std::fs::write(
        fx.dir.path().join("Cargo.toml"),
        "[package]\nname = \"a\"\n",
    )
    .unwrap();
    let edit = |id: &str, old: &str, new: &str| {
        call(
            id,
            "edit",
            json!({ "path": "Cargo.toml", "old_text": old, "new_text": new }),
        )
    };
    let mock = MockProvider::new(vec![
        step(call("r1", "read", json!({ "path": "Cargo.toml" }))),
        step(edit("e1", "\"a\"", "\"b\"")),
        step(edit("e2", "\"b\"", "\"c\"")),
        text_step("done"),
    ]);
    let mut run = fx.start(config(&[]), &mock, None);
    run.send(Command::SendMessage { text: "go".into() }).await;
    let mut seen = Vec::new();
    let req = run.until_ask_or_end(&mut seen).await.expect("e1 asks");
    run.approve(req, Some("edit(Cargo.toml)")).await;
    let req = run.until_ask_or_end(&mut seen).await;
    assert!(req.is_some(), "edit(Cargo.toml) asks again: {seen:?}");
    assert!(
        warnings(&seen).is_empty(),
        "the rule was accepted: {seen:?}"
    );
    run.approve(req.unwrap(), None).await;
    assert_eq!(run.until_ask_or_end(&mut seen).await, None);
    assert_eq!(asks(&seen), 2);
    run.shutdown().await;
}

#[tokio::test]
async fn non_matching_session_rule_approves_once_and_warns() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![
        step(bash("b1", "echo hi")),
        step(bash("b2", "echo hi")),
        step(bash("b3", "echo hi")),
        text_step("done"),
    ]);
    let mut run = fx.start(config(&[]), &mock, None);
    run.send(Command::SendMessage { text: "go".into() }).await;
    let mut seen = Vec::new();
    // A rule for another command, then one that does not parse: each approves this call
    // once, with a warning, and adds nothing.
    let req = run.until_ask_or_end(&mut seen).await.expect("b1 asks");
    run.approve(req, Some("bash(ls*)")).await;
    let req = run.until_ask_or_end(&mut seen).await.expect("b2 asks");
    run.approve(req, Some("bash(echo (hi)")).await;
    let req = run.until_ask_or_end(&mut seen).await.expect("b3 asks");
    run.approve(req, None).await;
    assert_eq!(run.until_ask_or_end(&mut seen).await, None);
    let f = finished(&seen);
    assert_eq!(f.len(), 3, "{f:?}");
    assert!(
        f.iter().all(|(_, err, _)| !err),
        "approved once each: {f:?}"
    );
    let w = warnings(&seen);
    assert_eq!(w.len(), 2, "{w:?}");
    assert!(
        w[0].contains("bash(ls*)") && w[0].contains("does not match"),
        "{w:?}"
    );
    assert!(w[1].contains("bash(echo (hi)"), "{w:?}");
    run.shutdown().await;
}
