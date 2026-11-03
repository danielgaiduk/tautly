//! Diffs in approvals and results, and `Command::Diff` (spec 7.11.4), through the engine
//! with a scripted provider and a real sandbox on a temporary directory.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tautly_core::{
    Command, Config, DiffFile, DiffPreview, DiffStatus, Engine, EngineBuilder, Event, FinishReason,
};
use tautly_provider::{Catalog, MockProvider, MockTurn, ModelId, ProviderEvent, StopReason};
use tautly_sandbox::Mode;
use tempfile::TempDir;
use tokio::sync::mpsc;

const MODEL: &str = "mock/test";
const WAIT: Duration = Duration::from_secs(30);

fn config(mode: Mode, edit_format: &str) -> Config {
    let catalog = Catalog::from_toml(&format!(
        r#"[models."{MODEL}"]
context_window = 200000
max_output = 8000
parallel_tools = true
reasoning = "none"
cache = "none"
images = false
edit_format = "{edit_format}"
"#
    ))
    .unwrap();
    let mut c = Config {
        model: ModelId::from(MODEL),
        catalog_overrides: Some(catalog),
        ..Config::default()
    };
    c.permissions.mode = mode;
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

fn text_step(text: &str) -> MockTurn {
    MockTurn::Events(vec![
        ProviderEvent::TextDelta(text.into()),
        ProviderEvent::Done {
            stop: StopReason::EndTurn,
        },
    ])
}

fn read(id: &str, path: &str) -> ProviderEvent {
    call(id, "read", json!({ "path": path }))
}

fn write(id: &str, path: &str, content: &str) -> ProviderEvent {
    call(id, "write", json!({ "path": path, "content": content }))
}

fn replace(id: &str, path: &str, old: &str, new: &str) -> ProviderEvent {
    call(
        id,
        "edit",
        json!({ "path": path, "old_text": old, "new_text": new }),
    )
}

fn patch(id: &str, body: &str) -> ProviderEvent {
    call(
        id,
        "edit",
        json!({ "patch": format!("*** Begin Patch\n{body}*** End Patch\n") }),
    )
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

    fn write(&self, rel: &str, text: &str) {
        let p = self.dir.path().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    fn slurp(&self, rel: &str) -> String {
        std::fs::read_to_string(self.dir.path().join(rel)).unwrap()
    }

    fn start(&self, cfg: Config, mock: &MockProvider, interactive: bool) -> Run {
        let (engine, events) = EngineBuilder {
            provider: Some(Arc::new(mock.clone())),
            data_home: Some(self.home.path().to_path_buf()),
            interactive,
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

    async fn say(&self, text: &str) {
        self.send(Command::SendMessage { text: text.into() }).await;
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

    async fn turn(&mut self) -> Vec<Event> {
        self.until(|e| matches!(e, Event::TurnFinished { .. }))
            .await
    }

    /// Wait for the next approval request and answer it with `Approve`.
    async fn approve_next(&mut self) -> (Vec<Event>, String, Option<DiffPreview>) {
        let events = self
            .until(|e| matches!(e, Event::ToolApprovalRequested { .. }))
            .await;
        let Some(Event::ToolApprovalRequested {
            request_id, diff, ..
        }) = events.last().cloned()
        else {
            unreachable!()
        };
        (events, request_id, diff)
    }

    /// `Command::Diff` and its answer (a `SessionDiff`, or an `Error`).
    async fn diff(&mut self, turn: Option<u32>) -> Event {
        self.send(Command::Diff { turn }).await;
        self.until(|e| matches!(e, Event::SessionDiff { .. } | Event::Error { .. }))
            .await
            .pop()
            .unwrap()
    }
}

/// `(call_id, is_error, error_kind, diff)` of every `ToolFinished`.
fn finished(events: &[Event]) -> Vec<(String, bool, Option<String>, Option<DiffPreview>)> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::ToolFinished {
                call_id,
                is_error,
                meta,
                diff,
                ..
            } => Some((
                call_id.clone(),
                *is_error,
                meta.error_kind.clone(),
                diff.clone(),
            )),
            _ => None,
        })
        .collect()
}

fn approvals(events: &[Event]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, Event::ToolApprovalRequested { .. }))
        .count()
}

fn reason(events: &[Event]) -> FinishReason {
    match events.last() {
        Some(Event::TurnFinished { reason }) => *reason,
        other => panic!("turn did not finish: {other:?}"),
    }
}

fn session_diff(ev: Event) -> (Option<u32>, Vec<DiffFile>, String, bool) {
    match ev {
        Event::SessionDiff {
            turn,
            files,
            unified,
            truncated,
        } => (turn, files, unified, truncated),
        other => panic!("not a session diff: {other:?}"),
    }
}

fn file(path: &str, status: DiffStatus, added: u32, removed: u32) -> DiffFile {
    DiffFile {
        path: path.into(),
        status,
        added,
        removed,
    }
}

#[tokio::test]
async fn approval_carries_diff_preview() {
    let fx = Fx::new();
    fx.write("src/lib.rs", "fn a() {}\nfn old() {}\n");
    let mock = MockProvider::new(vec![
        tools_step(vec![read("r1", "src/lib.rs")]),
        tools_step(vec![replace(
            "e1",
            "src/lib.rs",
            "fn old() {}",
            "fn new() {}",
        )]),
        text_step("done"),
    ]);
    let mut run = fx.start(config(Mode::Ask, "str_replace"), &mock, true);
    run.say("rename it").await;
    let (_, request_id, diff) = run.approve_next().await;
    let diff = diff.expect("the approval carries a diff");
    assert!(
        diff.unified
            .contains("--- a/src/lib.rs\n+++ b/src/lib.rs\n"),
        "{}",
        diff.unified
    );
    assert!(
        diff.unified.contains("-fn old() {}\n+fn new() {}\n"),
        "{}",
        diff.unified
    );
    assert!(!diff.truncated);
    assert_eq!(fx.slurp("src/lib.rs"), "fn a() {}\nfn old() {}\n");
    run.send(Command::Approve { request_id }).await;
    let events = run.turn().await;
    assert_eq!(reason(&events), FinishReason::Completed);
    let f = finished(&events);
    assert_eq!(f.len(), 1);
    assert!(!f[0].1, "{f:?}");
    assert_eq!(
        f[0].3.as_ref(),
        Some(&diff),
        "the approved diff is the applied diff"
    );
    assert_eq!(fx.slurp("src/lib.rs"), "fn a() {}\nfn new() {}\n");
}

#[tokio::test]
async fn failing_preview_answers_without_asking() {
    let fx = Fx::new();
    fx.write("a.txt", "alpha\n");
    let mock = MockProvider::new(vec![
        tools_step(vec![read("r1", "a.txt")]),
        tools_step(vec![replace("e1", "a.txt", "no such text", "x")]),
        text_step("done"),
    ]);
    let mut run = fx.start(config(Mode::Ask, "str_replace"), &mock, true);
    run.say("edit").await;
    let events = run.turn().await;
    assert_eq!(reason(&events), FinishReason::Completed);
    assert_eq!(approvals(&events), 0, "{events:?}");
    let f = finished(&events);
    let edit = f.iter().find(|f| f.0 == "e1").unwrap();
    assert!(edit.1);
    assert_eq!(edit.2.as_deref(), Some("not_found"));
    assert!(edit.3.is_none());
    assert_eq!(fx.slurp("a.txt"), "alpha\n");
}

#[tokio::test]
async fn file_changed_after_approval_fails_stale() {
    let fx = Fx::new();
    fx.write("a.txt", "alpha\nbeta\n");
    let mock = MockProvider::new(vec![
        tools_step(vec![read("r1", "a.txt")]),
        tools_step(vec![replace("e1", "a.txt", "beta", "BETA")]),
        text_step("done"),
    ]);
    let mut run = fx.start(config(Mode::Ask, "str_replace"), &mock, true);
    run.say("edit").await;
    let (_, request_id, diff) = run.approve_next().await;
    assert!(diff.is_some());
    fx.write("a.txt", "alpha\nbeta\nexternal\n");
    run.send(Command::Approve { request_id }).await;
    let events = run.turn().await;
    let f = finished(&events);
    let edit = f.iter().find(|f| f.0 == "e1").unwrap();
    assert!(edit.1, "{f:?}");
    assert_eq!(edit.2.as_deref(), Some("changed_after_approval"));
    assert!(edit.3.is_none());
    assert_eq!(fx.slurp("a.txt"), "alpha\nbeta\nexternal\n");
    let output = events
        .iter()
        .find_map(|e| match e {
            Event::ToolFinished {
                call_id, output, ..
            } if call_id == "e1" => Some(output.clone()),
            _ => None,
        })
        .unwrap();
    assert!(
        output.starts_with("stale: file changed after approval"),
        "{output}"
    );
}

#[tokio::test]
async fn tool_finished_carries_applied_diff_in_every_mode() {
    for mode in [Mode::Ask, Mode::AutoEdit, Mode::Yolo] {
        let fx = Fx::new();
        fx.write("a.txt", "one\n");
        let mock = MockProvider::new(vec![
            tools_step(vec![read("r1", "a.txt")]),
            tools_step(vec![replace("e1", "a.txt", "one", "ONE")]),
            tools_step(vec![write("w1", "new.txt", "fresh\n")]),
            text_step("done"),
        ]);
        let mut run = fx.start(config(mode, "str_replace"), &mock, true);
        run.say("go").await;
        let mut events = Vec::new();
        if mode == Mode::Ask {
            for _ in 0..2 {
                let (evs, request_id, _) = run.approve_next().await;
                events.extend(evs);
                run.send(Command::Approve { request_id }).await;
            }
        }
        events.extend(run.turn().await);
        assert_eq!(reason(&events), FinishReason::Completed, "{mode:?}");
        let f = finished(&events);
        assert!(f[0].3.is_none(), "a read has no diff: {f:?}");
        let edit = f[1].3.as_ref().unwrap_or_else(|| panic!("{mode:?}: {f:?}"));
        assert!(
            edit.unified.contains("--- a/a.txt\n+++ b/a.txt\n"),
            "{mode:?}"
        );
        assert!(edit.unified.contains("-one\n+ONE\n"), "{mode:?}");
        let created = f[2].3.as_ref().unwrap_or_else(|| panic!("{mode:?}: {f:?}"));
        assert_eq!(
            created.unified, "--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1 @@\n+fresh\n",
            "{mode:?}"
        );
    }
}

#[tokio::test]
async fn session_diff_net_change_per_file() {
    let fx = Fx::new();
    fx.write("b.txt", "one\ntwo\n");
    fx.write("same.txt", "x\n");
    let mock = MockProvider::new(vec![
        // Turn 1: edit b.txt, create c.txt, edit same.txt (reverted in turn 2).
        tools_step(vec![read("r1", "b.txt"), read("r2", "same.txt")]),
        tools_step(vec![patch(
            "p1",
            "*** Update File: b.txt\n@@\n one\n-two\n+TWO\n*** Add File: c.txt\n+temp\n\
             *** Update File: same.txt\n@@\n-x\n+y\n",
        )]),
        text_step("turn 1 done"),
        // Turn 2: edit b.txt again, delete c.txt, revert same.txt, create d.txt.
        tools_step(vec![patch(
            "p2",
            "*** Update File: b.txt\n@@\n-one\n+ONE\n TWO\n*** Delete File: c.txt\n\
             *** Update File: same.txt\n@@\n-y\n+x\n*** Add File: d.txt\n+new\n",
        )]),
        text_step("turn 2 done"),
    ]);
    let mut run = fx.start(config(Mode::Yolo, "patch"), &mock, false);
    run.say("one").await;
    let events = run.turn().await;
    assert!(finished(&events).iter().all(|f| !f.1), "{events:?}");
    run.say("two").await;
    let events = run.turn().await;
    assert!(finished(&events).iter().all(|f| !f.1), "{events:?}");
    assert!(!fx.dir.path().join("c.txt").exists());

    let (turn, files, unified, truncated) = session_diff(run.diff(None).await);
    assert_eq!(turn, None);
    assert!(!truncated);
    assert_eq!(
        files,
        vec![
            file("b.txt", DiffStatus::Modified, 2, 2),
            file("d.txt", DiffStatus::Added, 1, 0),
        ]
    );
    assert_eq!(
        unified,
        "--- a/b.txt\n+++ b/b.txt\n@@ -1,2 +1,2 @@\n-one\n-two\n+ONE\n+TWO\n\
         --- /dev/null\n+++ b/d.txt\n@@ -0,0 +1 @@\n+new\n"
    );

    // A file deleted for good shows as deleted.
    let fx = Fx::new();
    fx.write("old.txt", "bye\n");
    let mock = MockProvider::new(vec![
        tools_step(vec![read("r1", "old.txt")]),
        tools_step(vec![patch("p1", "*** Delete File: old.txt\n")]),
        text_step("done"),
    ]);
    let mut run = fx.start(config(Mode::Yolo, "patch"), &mock, false);
    run.say("delete").await;
    run.turn().await;
    let (_, files, unified, _) = session_diff(run.diff(None).await);
    assert_eq!(files, vec![file("old.txt", DiffStatus::Deleted, 0, 1)]);
    assert_eq!(
        unified,
        "--- a/old.txt\n+++ /dev/null\n@@ -1 +0,0 @@\n-bye\n"
    );
}

#[tokio::test]
async fn session_diff_turn_scope_uses_user_turns() {
    let fx = Fx::new();
    fx.write("a.txt", "a\n");
    let mock = MockProvider::new(vec![
        // User turn 1 takes three steps.
        tools_step(vec![read("r1", "a.txt")]),
        tools_step(vec![replace("e1", "a.txt", "a", "A")]),
        tools_step(vec![write("w1", "x.txt", "x\n")]),
        text_step("turn 1 done"),
        // User turn 2: one more step.
        tools_step(vec![
            write("w2", "y.txt", "y\n"),
            replace("e2", "a.txt", "A", "AA"),
        ]),
        text_step("turn 2 done"),
    ]);
    let mut run = fx.start(config(Mode::Yolo, "str_replace"), &mock, false);
    // The second message arrives (and is logged) while the first turn is still running.
    run.say("first").await;
    run.say("second").await;
    run.turn().await;
    run.turn().await;

    let (turn, files, unified, _) = session_diff(run.diff(Some(1)).await);
    assert_eq!(turn, Some(1));
    assert_eq!(
        files,
        vec![
            file("a.txt", DiffStatus::Modified, 1, 1),
            file("x.txt", DiffStatus::Added, 1, 0),
        ]
    );
    // The earliest pre-image in scope against the disk as it is now.
    assert!(unified.contains("-a\n+AA\n"), "{unified}");

    let (turn, files, unified, _) = session_diff(run.diff(Some(2)).await);
    assert_eq!(turn, Some(2));
    assert_eq!(
        files,
        vec![
            file("a.txt", DiffStatus::Modified, 1, 1),
            file("y.txt", DiffStatus::Added, 1, 0),
        ]
    );
    assert!(unified.contains("-A\n+AA\n"), "{unified}");

    match run.diff(Some(3)).await {
        Event::Error { message, .. } => assert!(message.contains("no turn 3"), "{message}"),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn session_diff_capped_at_2000_lines() {
    let fx = Fx::new();
    let big: String = (1..=3000).map(|i| format!("line {i}\n")).collect();
    let mock = MockProvider::new(vec![
        tools_step(vec![write("w1", "big.txt", &big)]),
        text_step("done"),
    ]);
    let mut run = fx.start(config(Mode::Yolo, "str_replace"), &mock, false);
    run.say("write").await;
    run.turn().await;
    let (_, files, unified, truncated) = session_diff(run.diff(None).await);
    assert_eq!(files, vec![file("big.txt", DiffStatus::Added, 3000, 0)]);
    assert!(truncated);
    assert_eq!(unified.lines().count(), 2001);
    assert!(
        unified.ends_with("… 1003 more lines (diff truncated)"),
        "{}",
        &unified[unified.len() - 80..]
    );
}

#[test]
fn old_json_without_diff_fields_parses() {
    let finished: Event = serde_json::from_value(json!({
        "type": "tool_finished",
        "call_id": "c1",
        "tool": "edit",
        "output": "edited a.txt",
        "is_error": false,
        "blocked": false,
        "effects": [],
        "meta": {"exit_code": null, "error_kind": null}
    }))
    .unwrap();
    assert!(
        matches!(finished, Event::ToolFinished { diff: None, .. }),
        "{finished:?}"
    );
    let asked: Event = serde_json::from_value(json!({
        "type": "tool_approval_requested",
        "request_id": "approval-1",
        "call_id": "c1",
        "tool": "edit",
        "args": {},
        "reason": "edits need approval"
    }))
    .unwrap();
    assert!(
        matches!(asked, Event::ToolApprovalRequested { diff: None, .. }),
        "{asked:?}"
    );
    // And the new ones read back.
    let diff: Event = serde_json::from_value(json!({
        "type": "session_diff",
        "turn": null,
        "files": [{"path": "a", "status": "renamed", "added": 1, "removed": 0}],
        "unified": "",
        "truncated": false
    }))
    .unwrap();
    let Event::SessionDiff { files, .. } = diff else {
        panic!()
    };
    assert_eq!(files[0].status, DiffStatus::Unknown);
    let cmd: Command = serde_json::from_value(json!({"type": "diff"})).unwrap();
    assert_eq!(cmd, Command::Diff { turn: None });
}
