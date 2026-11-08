//! Where checkpoints are taken (`Checkpoint.point`) and the files at a step boundary by
//! log offset (spec 7.12.1), the rule Task 49's `state_at` implements: the first
//! `Checkpoint` after the last record of step N, with the `Edit`s between reverted. Each
//! case of the rule is checked here against a real git repository, with the rule itself
//! written out in this file ([`boundary`]).

use std::path::{Path, PathBuf};
use std::process::Command as Process;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tautly_core::record::CheckpointPoint;
use tautly_core::{Command, Config, EngineBuilder, Event, SessionRecord};
use tautly_provider::{Catalog, MockProvider, MockTurn, ModelId, ProviderEvent, StopReason};
use tautly_sandbox::Mode;
use tautly_session::{BlobRef, RelPath, SessionReader};
use tempfile::TempDir;
use tokio::sync::mpsc;

const MODEL: &str = "mock/test";
const WAIT: Duration = Duration::from_secs(30);

// ------------------------------------------------------------------------ fixtures

struct Repo {
    dir: TempDir,
    home: TempDir,
}

impl Repo {
    /// A repository with `a.txt` committed.
    fn new() -> Repo {
        let repo = Repo {
            dir: tempfile::tempdir().unwrap(),
            home: tempfile::tempdir().unwrap(),
        };
        repo.git(&["init", "-q", "-b", "main"]);
        repo.write("a.txt", "zero\n");
        repo.git(&["add", "-A"]);
        repo.git(&["commit", "-q", "-m", "init"]);
        repo
    }

    fn root(&self) -> PathBuf {
        self.dir.path().canonicalize().unwrap()
    }

    fn write(&self, rel: &str, text: &str) {
        std::fs::write(self.root().join(rel), text).unwrap();
    }

    fn git(&self, args: &[&str]) -> Option<String> {
        let out = Process::new("git")
            .current_dir(self.root())
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

    fn builder(&self, mock: &MockProvider) -> EngineBuilder {
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
        // A snapshot that times out turns checkpoints off for the session; under load
        // that would change what these tests see. Tests of the limit set their own.
        cfg.checkpoints.timeout_ms = 120_000;
        EngineBuilder {
            provider: Some(Arc::new(mock.clone())),
            data_home: Some(self.home.path().to_path_buf()),
            ..EngineBuilder::new(cfg, self.dir.path())
        }
    }
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

    async fn turn_with(&mut self, text: &str) {
        self.engine
            .send(Command::SendMessage {
                id: None,
                text: text.into(),
            })
            .await
            .unwrap();
        loop {
            let ev = tokio::time::timeout(WAIT, self.events.recv())
                .await
                .expect("timed out waiting for an event")
                .expect("event channel closed");
            if matches!(ev, Event::TurnFinished { .. }) {
                return;
            }
        }
    }

    /// Shut down; the log's path.
    async fn stop(mut self) -> PathBuf {
        let path = self.engine.session_path().to_path_buf();
        self.engine.send(Command::Shutdown).await.unwrap();
        while tokio::time::timeout(WAIT, self.events.recv())
            .await
            .expect("the engine did not stop")
            .is_some()
        {}
        path
    }
}

fn records(path: &Path) -> Vec<SessionRecord> {
    SessionReader::<SessionRecord>::open(path)
        .unwrap()
        .records()
        .map(|r| r.unwrap().1)
        .collect()
}

fn points(recs: &[SessionRecord]) -> Vec<(Option<CheckpointPoint>, u32)> {
    recs.iter()
        .filter_map(|r| match r {
            SessionRecord::Checkpoint { point, step, .. } => Some((*point, *step)),
            _ => None,
        })
        .collect()
}

// --------------------------------------------------------- the rule (spec 7.12.1)

/// What [`boundary`] finds for step N.
#[derive(Debug)]
struct Boundary {
    commit: String,
    point: Option<CheckpointPoint>,
    /// `Edit`s between the step's last record and the checkpoint: `(path, pre_image)`,
    /// in log order (revert them newest first).
    reverts: Vec<(RelPath, Option<BlobRef>)>,
    partial: bool,
}

/// The files at the end of provider step `n`, by log offset: the first `Checkpoint` after
/// the last record of step `n` (its `Request`, `Provider`, `ToolCall`, `ToolResult` and
/// `Edit` records; checkpoints are never a step's last record), the `Edit`s between to
/// revert, and whether the result is partial: a `CheckpointSkipped` between, a
/// `ResumeStart` checkpoint (the turn's end was never checkpointed: a crash), one whose
/// point is none or unknown without a `TurnEnd` checkpoint of the step's turn before it,
/// or none at all.
fn boundary(recs: &[SessionRecord], n: u32) -> Option<Boundary> {
    let mut step = None;
    let mut last = None;
    let mut user_turn = None;
    for (i, r) in recs.iter().enumerate() {
        match r {
            SessionRecord::Request { step: s, turn, .. } => {
                step = Some(*s);
                if *s == n {
                    last = Some(i);
                    user_turn = Some(*turn);
                }
            }
            SessionRecord::TurnStart { .. } | SessionRecord::TurnEnd { .. } => step = None,
            SessionRecord::Provider(_)
            | SessionRecord::ToolCall { .. }
            | SessionRecord::ToolResult { .. }
            | SessionRecord::Edit { .. }
                if step == Some(n) =>
            {
                last = Some(i);
            }
            _ => {}
        }
    }
    let last = last?;
    let turn_end_seen = recs[..last].iter().any(|r| {
        matches!(r, SessionRecord::Checkpoint {
            point: Some(CheckpointPoint::TurnEnd),
            turn,
            ..
        } if Some(*turn) == user_turn)
    });
    let mut reverts = Vec::new();
    let mut partial = false;
    for r in &recs[last + 1..] {
        match r {
            SessionRecord::Edit {
                path, pre_image, ..
            } => reverts.push((path.clone(), pre_image.clone())),
            SessionRecord::CheckpointSkipped { .. } => partial = true,
            SessionRecord::Checkpoint { commit, point, .. } => {
                // Spec 7.12.1: a resume's start checkpoint is partial, and so is one whose
                // point is not known (an older or newer build wrote it) unless a turn-end
                // checkpoint of the step's turn came before it.
                let unsure =
                    matches!(point, None | Some(CheckpointPoint::Unknown)) && !turn_end_seen;
                return Some(Boundary {
                    commit: commit.clone(),
                    point: *point,
                    reverts,
                    partial: partial || *point == Some(CheckpointPoint::ResumeStart) || unsure,
                });
            }
            _ => {}
        }
    }
    None
}

/// The content of `rel` at `b`: the checkpoint's, with the edits reverted.
fn file_at(repo: &Repo, log: &Path, b: &Boundary, rel: &str) -> Option<String> {
    let reader = SessionReader::<SessionRecord>::open(log).unwrap();
    let mut content = repo.git(&["show", &format!("{}:{rel}", b.commit)]);
    for (path, pre) in b.reverts.iter().rev() {
        if path.as_str() == rel {
            content = pre
                .as_ref()
                .map(|p| String::from_utf8(reader.read_blob(p).unwrap()).unwrap());
        }
    }
    content
}

// ------------------------------------------------------------------------- the cases

#[tokio::test]
async fn checkpoints_record_where_they_were_taken() {
    let repo = Repo::new();
    let mock = MockProvider::new(vec![
        tools_step(vec![bash("b1", "true")]),
        text_step("done"),
    ]);
    let mut run = Run::start(repo.builder(&mock));
    let id = run.engine.session_id().to_string();
    run.turn_with("one").await;
    let path = run.stop().await;
    let mock = MockProvider::new(vec![text_step("again")]);
    let mut run = Run::start(EngineBuilder {
        resume: Some(id),
        ..repo.builder(&mock)
    });
    run.turn_with("two").await;
    run.stop().await;
    let got: Vec<Option<CheckpointPoint>> = points(&records(&path))
        .into_iter()
        .map(|(p, _)| p)
        .collect();
    assert_eq!(
        got,
        [
            Some(CheckpointPoint::SessionStart),
            Some(CheckpointPoint::BeforeExec),
            Some(CheckpointPoint::TurnEnd),
            Some(CheckpointPoint::ResumeStart),
            Some(CheckpointPoint::TurnEnd),
        ]
    );
}

#[tokio::test]
async fn at_a_turn_end_the_turn_end_checkpoint_is_exact_and_ignores_later_human_edits() {
    // Steps: 1 user, 2 write, 3 text | human edit | 4 user, 5 bash, 6 text.
    let repo = Repo::new();
    let mock = MockProvider::new(vec![
        tools_step(vec![write("w1", "a.txt", "one\n")]),
        text_step("first"),
        tools_step(vec![bash("b1", "printf 'two\\n' > b.txt")]),
        text_step("second"),
    ]);
    let mut run = Run::start(repo.builder(&mock));
    run.turn_with("one").await;
    repo.write("a.txt", "human\n");
    run.turn_with("two").await;
    let log = run.stop().await;
    let recs = records(&log);

    let b = boundary(&recs, 3).unwrap();
    assert_eq!(b.point, Some(CheckpointPoint::TurnEnd), "{b:?}");
    assert!(!b.partial);
    assert_eq!(file_at(&repo, &log, &b, "a.txt").as_deref(), Some("one\n"));
    assert_eq!(file_at(&repo, &log, &b, "b.txt"), None);
    // Choosing by `step > 3` would take the next turn's `BeforeExec`, with the human edit.
    let (_, later_step) = points(&recs)
        .into_iter()
        .find(|(p, _)| *p == Some(CheckpointPoint::BeforeExec))
        .unwrap();
    assert_eq!(later_step, 5);
}

#[tokio::test]
async fn mid_turn_the_next_before_exec_with_the_edits_between_reverted_is_exact() {
    // Steps: 1 user, 2 write a=one, 3 write a=two then bash c=x, 4 text.
    let repo = Repo::new();
    let mock = MockProvider::new(vec![
        tools_step(vec![write("w1", "a.txt", "one\n")]),
        tools_step(vec![
            write("w2", "a.txt", "two\n"),
            bash("b1", "printf 'x\\n' > c.txt"),
        ]),
        text_step("done"),
    ]);
    let mut run = Run::start(repo.builder(&mock));
    run.turn_with("go").await;
    let log = run.stop().await;
    let recs = records(&log);

    // After step 2: step 3's `BeforeExec` holds its write, which is reverted.
    let b2 = boundary(&recs, 2).unwrap();
    assert_eq!(b2.point, Some(CheckpointPoint::BeforeExec), "{b2:?}");
    assert!(!b2.partial);
    assert_eq!(b2.reverts.len(), 1);
    assert_eq!(file_at(&repo, &log, &b2, "a.txt").as_deref(), Some("one\n"));
    assert_eq!(file_at(&repo, &log, &b2, "c.txt"), None);

    // After step 3: not its own `BeforeExec` (stamped step 3, before its bash ran) but
    // the turn end.
    let b3 = boundary(&recs, 3).unwrap();
    assert_eq!(b3.point, Some(CheckpointPoint::TurnEnd), "{b3:?}");
    assert_eq!(file_at(&repo, &log, &b3, "a.txt").as_deref(), Some("two\n"));
    assert_eq!(file_at(&repo, &log, &b3, "c.txt").as_deref(), Some("x\n"));
}

#[tokio::test]
async fn after_a_crash_the_resume_checkpoint_is_partial() {
    // Steps: 1 user, 2 write + bash, 3 text; the log is cut after step 2's results (a
    // crash before the turn-end checkpoint), a human edits, the session resumes.
    let repo = Repo::new();
    let mock = MockProvider::new(vec![
        tools_step(vec![
            write("w1", "a.txt", "one\n"),
            bash("b1", "printf 'y\\n' > d.txt"),
        ]),
        text_step("done"),
    ]);
    let mut run = Run::start(repo.builder(&mock));
    let id = run.engine.session_id().to_string();
    run.turn_with("go").await;
    let log = run.stop().await;
    let recs = records(&log);
    let cut = recs
        .iter()
        .rposition(|r| matches!(r, SessionRecord::ToolResult { .. }))
        .unwrap();
    let text = std::fs::read_to_string(&log).unwrap();
    // The header line, then one line per record.
    let kept: Vec<&str> = text.lines().take(cut + 2).collect();
    std::fs::write(&log, kept.join("\n") + "\n").unwrap();
    repo.write("a.txt", "human\n");

    let mock = MockProvider::new(vec![text_step("resumed")]);
    let mut run = Run::start(EngineBuilder {
        resume: Some(id),
        ..repo.builder(&mock)
    });
    run.turn_with("again").await;
    run.stop().await;
    let recs = records(&log);
    let b = boundary(&recs, 2).unwrap();
    assert_eq!(b.point, Some(CheckpointPoint::ResumeStart), "{b:?}");
    assert!(b.partial, "{b:?}");
    // It holds the human edit: only a partial state.
    assert_eq!(
        file_at(&repo, &log, &b, "a.txt").as_deref(),
        Some("human\n")
    );

    // Re-review 2 M2: the same log as an older build (no `point`) or a newer one (a point
    // this build does not know) wrote it. Nothing says that checkpoint is not a resume's,
    // and the step's turn has no `turn_end` checkpoint before it: partial.
    for point in [None, Some(json!("from_the_future"))] {
        let text = std::fs::read_to_string(&log).unwrap();
        let mut lines = text.lines();
        let mut out = vec![lines.next().unwrap().to_string()];
        for l in lines {
            let mut v: Value = serde_json::from_str(l).unwrap();
            if v["data"]["type"] == "checkpoint" {
                let data = v["data"]["data"].as_object_mut().unwrap();
                match &point {
                    None => drop(data.remove("point")),
                    Some(p) => drop(data.insert("point".into(), p.clone())),
                }
            }
            out.push(v.to_string());
        }
        let rewritten = log.with_extension("rewritten.jsonl");
        std::fs::write(&rewritten, out.join("\n") + "\n").unwrap();
        let b = boundary(&records(&rewritten), 2).unwrap();
        let expected = point.as_ref().map(|_| CheckpointPoint::Unknown);
        assert_eq!(b.point, expected, "{b:?}");
        assert!(b.partial, "{point:?}: {b:?}");
    }
}

#[tokio::test]
async fn a_skipped_turn_end_makes_the_next_checkpoint_partial() {
    // A cancelled turn logs `CheckpointSkipped{cancelled}` at its end.
    let repo = Repo::new();
    let mock = MockProvider::new(vec![
        tools_step(vec![bash("b1", "sleep 30")]),
        tools_step(vec![bash("b2", "true")]),
        text_step("done"),
    ]);
    let mut run = Run::start(repo.builder(&mock));
    run.engine
        .send(Command::SendMessage {
            id: None,
            text: "one".into(),
        })
        .await
        .unwrap();
    loop {
        let ev = tokio::time::timeout(WAIT, run.events.recv())
            .await
            .unwrap()
            .unwrap();
        if matches!(ev, Event::ToolStarted { .. }) {
            break;
        }
    }
    run.engine.send(Command::Cancel).await.unwrap();
    loop {
        let ev = tokio::time::timeout(WAIT, run.events.recv())
            .await
            .unwrap()
            .unwrap();
        if matches!(ev, Event::TurnFinished { .. }) {
            break;
        }
    }
    run.turn_with("two").await;
    let log = run.stop().await;
    let recs = records(&log);
    assert!(recs.iter().any(|r| matches!(
        r,
        SessionRecord::CheckpointSkipped {
            point: Some(CheckpointPoint::TurnEnd),
            ..
        }
    )));
    // The turn's end checkpoint is taken after it, once its command has stopped (Task 49
    // N1): logged after the `TurnEnd`, behind the skip, so still partial.
    let b = boundary(&recs, 2).unwrap();
    assert_eq!(b.point, Some(CheckpointPoint::TurnEnd), "{b:?}");
    assert!(b.partial, "{b:?}");
    let end = recs
        .iter()
        .position(|r| matches!(r, SessionRecord::TurnEnd { .. }))
        .unwrap();
    let next = recs[end..]
        .iter()
        .position(|r| matches!(r, SessionRecord::TurnStart { .. }))
        .unwrap();
    assert!(
        recs[end..end + next].iter().any(|r| matches!(
            r,
            SessionRecord::Checkpoint {
                point: Some(CheckpointPoint::TurnEnd),
                turn: 1,
                ..
            }
        )),
        "{:?}",
        &recs[end..]
    );
}

/// Wait for the first event `pick` accepts.
async fn wait_for(events: &mut mpsc::Receiver<Event>, pick: impl Fn(&Event) -> bool) -> Event {
    loop {
        let ev = tokio::time::timeout(WAIT, events.recv())
            .await
            .expect("timed out waiting for an event")
            .expect("event channel closed");
        if pick(&ev) {
            return ev;
        }
    }
}

/// Cancel a turn at its first tool call, and wait for its `TurnFinished`.
async fn cancelled_turn(run: &mut Run) {
    run.engine
        .send(Command::SendMessage {
            id: None,
            text: "one".into(),
        })
        .await
        .unwrap();
    wait_for(&mut run.events, |e| matches!(e, Event::ToolStarted { .. })).await;
    run.engine.send(Command::Cancel).await.unwrap();
    wait_for(&mut run.events, |e| matches!(e, Event::TurnFinished { .. })).await;
}

fn is_turn_end_checkpoint(r: &SessionRecord) -> bool {
    matches!(
        r,
        SessionRecord::Checkpoint {
            point: Some(CheckpointPoint::TurnEnd),
            turn: 1,
            ..
        }
    )
}

#[tokio::test]
async fn a_fork_right_after_a_cancel_leaves_the_recovered_checkpoint_in_the_parent() {
    // Review CP5-A I2: a cancelled turn's end checkpoint is recovered in the background;
    // a fork sent at once must wait for it, so its record lands in the parent.
    let repo = Repo::new();
    let mock = MockProvider::new(vec![tools_step(vec![bash("b1", "sleep 30")])]);
    let mut run = Run::start(repo.builder(&mock));
    let parent = run.engine.session_path().to_path_buf();
    cancelled_turn(&mut run).await;
    run.engine
        .send(Command::Fork {
            id: Some("f-1".into()),
            at: None,
            label: None,
        })
        .await
        .unwrap();
    let answer = wait_for(&mut run.events, |e| {
        matches!(e, Event::SessionChanged { .. } | Event::Error { .. })
    })
    .await;
    assert_eq!(answer.reply_id(), Some("f-1"), "{answer:?}");
    let branch = run.engine.current_session().1;
    assert_ne!(branch, parent);
    run.stop().await;
    let parent_recs = records(&parent);
    let end = parent_recs
        .iter()
        .position(|r| matches!(r, SessionRecord::TurnEnd { .. }))
        .unwrap();
    assert!(
        parent_recs[end..].iter().any(is_turn_end_checkpoint),
        "parent: {:?}",
        &parent_recs[end..]
    );
    // At most the parent's record as copied, never a second one of the branch's own.
    let branch_recs = records(&branch);
    let n = branch_recs
        .iter()
        .filter(|r| is_turn_end_checkpoint(r))
        .count();
    assert!(n <= 1, "branch: {branch_recs:?}");
}

#[tokio::test]
async fn undo_after_a_cancel_reverts_the_cancelled_commands_writes() {
    // Re-review CP5-ENG concern 4: the recovered turn-end checkpoint (logged after the
    // `TurnEnd`) is the turn's end for undo, so Ctrl-C then `/undo` reverts the command.
    let repo = Repo::new();
    let mock = MockProvider::new(vec![tools_step(vec![bash(
        "b1",
        "echo x > f.txt; echo changed > a.txt; sleep 30",
    )])]);
    let mut run = Run::start(repo.builder(&mock));
    run.engine
        .send(Command::SendMessage {
            id: None,
            text: "one".into(),
        })
        .await
        .unwrap();
    wait_for(&mut run.events, |e| matches!(e, Event::ToolStarted { .. })).await;
    let f = repo.root().join("f.txt");
    let deadline = tokio::time::Instant::now() + WAIT;
    while !(f.exists()
        && std::fs::read_to_string(repo.root().join("a.txt")).unwrap() == "changed\n")
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the command never wrote"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    run.engine.send(Command::Cancel).await.unwrap();
    wait_for(&mut run.events, |e| matches!(e, Event::TurnFinished { .. })).await;
    run.engine
        .send(Command::Undo {
            id: None,
            turn: None,
            dry_run: false,
        })
        .await
        .unwrap();
    let ev = wait_for(&mut run.events, |e| {
        matches!(e, Event::Undone { .. } | Event::Error { .. })
    })
    .await;
    let Event::Undone { note, .. } = &ev else {
        panic!("{ev:?}")
    };
    assert!(!note.as_deref().unwrap_or("").contains("skipped"), "{ev:?}");
    assert!(!f.exists(), "{ev:?}");
    assert_eq!(
        std::fs::read_to_string(repo.root().join("a.txt")).unwrap(),
        "zero\n"
    );
    run.stop().await;
}
