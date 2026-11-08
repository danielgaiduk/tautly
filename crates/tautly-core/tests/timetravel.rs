//! Time travel (spec 7.12, Task 49): file states at step boundaries, rewind as a fork,
//! switching branches, the working-tree lease, labels and the session tree. Against real
//! git repositories (and a plain directory) with scripted providers.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tautly_core::record::{CheckpointPoint, session_label};
use tautly_core::resume::rebuild;
use tautly_core::timetravel::{expected_tip, state_at, step_at_offset};
use tautly_core::{
    Command, Config, EngineBuilder, Event, FinishReason, MaterializeReport, SessionRecord, TtError,
    tree,
};
use tautly_provider::mock::{call, text_step_no_usage, tools_step};
use tautly_provider::{CanonicalRequest, Catalog, MockProvider, MockTurn, ModelId, ProviderEvent};
use tautly_sandbox::{LocalSandbox, Mode, Sandbox};
use tautly_session::{Origin, SessionReader};
use tautly_testkit::Repo;
use tautly_testkit::{Delegate, Delegating, catalog};
use tautly_testkit::{MODEL, Run, WAIT, start};

// ------------------------------------------------------------------------ fixtures

/// `a.txt`, the file of a plain work tree.
const A: &[(&str, &str)] = &[("a.txt", "zero\n")];
/// `a.txt` and `b.txt`, committed in a repository.
const AB: &[(&str, &str)] = &[("a.txt", "zero\n"), ("b.txt", "bee\n")];

trait RepoExt {
    fn home(&self) -> &Path;
    fn files(&self) -> BTreeMap<String, String>;
    fn config() -> Config;
    fn builder(&self, mock: &MockProvider) -> EngineBuilder;
    fn project(&self) -> String;
}

impl RepoExt for Repo {
    fn home(&self) -> &Path {
        self.home.path()
    }

    /// Every file of the work tree (outside `.git` and `.tautly`) and its content.
    fn files(&self) -> BTreeMap<String, String> {
        fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, String>) {
            for e in std::fs::read_dir(dir).unwrap() {
                let p = e.unwrap().path();
                let rel = p.strip_prefix(root).unwrap().to_string_lossy().into_owned();
                if rel == ".git" || rel == ".tautly" {
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
        walk(&self.root(), &self.root(), &mut out);
        out
    }

    fn config() -> Config {
        let catalog = catalog(200_000, 8000, "str_replace");
        let mut cfg = Config {
            model: ModelId::from(MODEL),
            catalog_overrides: Some(catalog),
            ..Config::default()
        };
        cfg.permissions.mode = Mode::Yolo;
        // A snapshot that times out turns checkpoints off for the session; under load
        // that would change what these tests see. Tests of the limit set their own.
        cfg.checkpoints.timeout_ms = 120_000;
        cfg
    }

    fn builder(&self, mock: &MockProvider) -> EngineBuilder {
        EngineBuilder {
            provider: Some(Arc::new(mock.clone())),
            data_home: Some(self.home().to_path_buf()),
            ..EngineBuilder::new(Repo::config(), self.dir.path())
        }
    }

    fn project(&self) -> String {
        tautly_session::paths::project_id(&self.root())
    }
}

fn write(id: &str, path: &str, content: &str) -> ProviderEvent {
    call(id, "write", json!({ "path": path, "content": content }))
}

fn bash(id: &str, command: &str) -> ProviderEvent {
    call(id, "bash", json!({ "command": command }))
}

trait RunExt {
    async fn answer(&mut self, cmd: Command) -> Answer;
    async fn rewind(&mut self, at: u32, dry_run: bool, overwrite: &[&str]) -> Answer;
    async fn switch(&mut self, id: &str, overwrite: &[&str]) -> Answer;
    fn current(&self) -> (String, PathBuf);
}

impl RunExt for Run {
    /// Send `cmd` and wait for its answer: `Rewound`/`Switched` (then the
    /// `SessionChanged` that follows an applied one), or an `Error`.
    async fn answer(&mut self, cmd: Command) -> Answer {
        self.engine.send(cmd).await.unwrap();
        let seen = self
            .until(|e| {
                matches!(
                    e,
                    Event::Rewound { .. } | Event::Switched { .. } | Event::Error { .. }
                )
            })
            .await;
        match seen.last().unwrap().clone() {
            Event::Rewound { branch, report, .. }
            | Event::Switched {
                id: _,
                branch,
                report,
            } => {
                if report.applied {
                    let changed = self
                        .until(|e| matches!(e, Event::SessionChanged { .. }))
                        .await;
                    let Some(Event::SessionChanged {
                        id: None,
                        session_id,
                    }) = changed.last()
                    else {
                        unreachable!()
                    };
                    assert_eq!(*session_id, branch);
                }
                Answer::Done(branch, report)
            }
            Event::Error { message, .. } => Answer::Error(message),
            _ => unreachable!(),
        }
    }

    async fn rewind(&mut self, at: u32, dry_run: bool, overwrite: &[&str]) -> Answer {
        self.answer(Command::Rewind {
            id: None,
            at,
            label: None,
            dry_run,
            overwrite: overwrite.iter().map(|s| s.to_string()).collect(),
        })
        .await
    }

    async fn switch(&mut self, id: &str, overwrite: &[&str]) -> Answer {
        self.answer(Command::Switch {
            id: None,
            session_id: id.into(),
            dry_run: false,
            overwrite: overwrite.iter().map(|s| s.to_string()).collect(),
        })
        .await
    }

    fn current(&self) -> (String, PathBuf) {
        self.engine.current_session()
    }
}

#[derive(Debug)]
enum Answer {
    Done(String, MaterializeReport),
    Error(String),
}

impl Answer {
    fn done(self) -> (String, MaterializeReport) {
        match self {
            Answer::Done(b, r) => (b, r),
            Answer::Error(e) => panic!("expected an answer, got the error: {e}"),
        }
    }

    fn error(self) -> String {
        match self {
            Answer::Error(e) => e,
            Answer::Done(b, r) => panic!("expected an error, got {b}: {r:?}"),
        }
    }
}

fn reader(path: &Path) -> SessionReader<SessionRecord> {
    SessionReader::<SessionRecord>::open(path).unwrap()
}

fn records(path: &Path) -> Vec<SessionRecord> {
    reader(path).records().map(|r| r.unwrap().1).collect()
}

fn raw_lines(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect()
}

fn logs(home: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let sessions = home.join("sessions");
    for project in std::fs::read_dir(sessions).unwrap() {
        for e in std::fs::read_dir(project.unwrap().path()).unwrap() {
            let p = e.unwrap().path();
            if p.extension().is_some_and(|x| x == "jsonl") {
                out.push(p);
            }
        }
    }
    out
}

/// Two turns: (1 user, 2 write a=one, 3 text) and (4 user, 5 write a=two and run a
/// command that changes b.txt and creates c.txt, 6 text).
fn two_turns() -> MockProvider {
    MockProvider::new(vec![
        tools_step(vec![write("w1", "a.txt", "one\n")]),
        text_step_no_usage("first"),
        tools_step(vec![
            write("w2", "a.txt", "two\n"),
            bash("b1", "printf 'changed\\n' > b.txt && printf 'x\\n' > c.txt"),
        ]),
        text_step_no_usage("second"),
    ])
}

async fn run_two_turns(repo: &Repo, mock: &MockProvider) -> Run {
    let mut run = start(repo.builder(mock));
    run.turn_with("one").await;
    run.turn_with("two").await;
    run
}

// --------------------------------------------------------------------- file states

#[tokio::test]
async fn state_at_uses_earliest_checkpoint_after_and_reverts_tool_edits() {
    // Steps: 1 user, 2 write a=one, 3 write a=two then bash c=x, 4 text.
    let repo = Repo::git_repo(AB);
    let mock = MockProvider::new(vec![
        tools_step(vec![write("w1", "a.txt", "one\n")]),
        tools_step(vec![
            write("w2", "a.txt", "two\n"),
            bash("b1", "printf 'x\\n' > c.txt"),
        ]),
        text_step_no_usage("done"),
    ]);
    let mut run = start(repo.builder(&mock));
    run.turn_with("go").await;
    let log = run.current().1;
    run.stop().await;
    let recs = records(&log);
    let commit_at = |point: CheckpointPoint| {
        recs.iter()
            .find_map(|r| match r {
                SessionRecord::Checkpoint {
                    commit, point: p, ..
                } if *p == Some(point) => Some(commit.clone()),
                _ => None,
            })
            .unwrap()
    };
    let r = reader(&log);

    // After step 2: step 3's `BeforeExec` checkpoint, with step 3's write reverted.
    let s2 = state_at(&r, 2).unwrap();
    assert_eq!(
        s2.base,
        Some(commit_at(CheckpointPoint::BeforeExec)),
        "{s2:?}"
    );
    assert_eq!(s2.partial, None);
    assert_eq!(s2.overlay.len(), 1, "{s2:?}");
    assert_eq!(s2.overlay[0].0.as_str(), "a.txt");
    let pre = s2.overlay[0].1.clone().expect("a.txt existed after step 2");
    assert_eq!(r.read_blob(&pre).unwrap(), b"one\n");

    // After step 3 (its own `BeforeExec` came before its command ran): the turn end.
    let s3 = state_at(&r, 3).unwrap();
    assert_eq!(s3.base, Some(commit_at(CheckpointPoint::TurnEnd)), "{s3:?}");
    assert!(s3.overlay.is_empty());
    assert_eq!(s3.partial, None);

    // Before the first step: the session-start checkpoint.
    let s0 = state_at(&r, 0).unwrap();
    assert_eq!(s0.base, Some(commit_at(CheckpointPoint::SessionStart)));
    assert!(s0.overlay.is_empty());

    // The tip is the turn end, with nothing after it.
    let tip = expected_tip(&r).unwrap();
    assert_eq!(tip.base, Some(commit_at(CheckpointPoint::TurnEnd)));
    assert!(tip.overlay.is_empty());

    assert!(matches!(
        state_at(&r, 9),
        Err(TtError::NoSuchStep { at: 9, last: 4 })
    ));
}

/// The log at `log`, rewritten with every checkpoint's `point` removed (`None`) or set
/// to a value this build does not know.
fn without_points(log: &Path, point: Option<&str>) -> PathBuf {
    let text = std::fs::read_to_string(log).unwrap();
    let mut lines = text.lines();
    let mut out = vec![lines.next().unwrap().to_string()];
    for l in lines {
        let mut v: Value = serde_json::from_str(l).unwrap();
        if v["data"]["type"] == "checkpoint" {
            let data = v["data"]["data"].as_object_mut().unwrap();
            match point {
                None => drop(data.remove("point")),
                Some(p) => drop(data.insert("point".into(), json!(p))),
            }
        }
        out.push(v.to_string());
    }
    let rewritten = log.with_extension(format!("{}.jsonl", point.unwrap_or("none")));
    std::fs::write(&rewritten, out.join("\n") + "\n").unwrap();
    rewritten
}

#[tokio::test]
async fn state_at_with_skipped_checkpoint_is_partial() {
    // A cancelled turn logs `CheckpointSkipped{cancelled}` at its end: the next turn's
    // checkpoint (which has whatever changed meanwhile) is only partial for step 2.
    let repo = Repo::git_repo(AB);
    let mock = MockProvider::new(vec![
        tools_step(vec![bash("b1", "sleep 30")]),
        tools_step(vec![bash("b2", "true")]),
        text_step_no_usage("done"),
    ]);
    // A generous checkpoint time limit: one skipped under load is another partial case.
    let patient = |mut b: EngineBuilder| {
        b.config.checkpoints.timeout_ms = 120_000;
        b
    };
    let mut run = start(patient(repo.builder(&mock)));
    run.engine
        .send(Command::SendMessage {
            id: None,
            text: "one".into(),
        })
        .await
        .unwrap();
    run.until(|e| matches!(e, Event::ToolStarted { .. })).await;
    run.engine.send(Command::Cancel).await.unwrap();
    run.until(|e| matches!(e, Event::TurnFinished { .. })).await;
    run.turn_with("two").await;
    let log = run.current().1;
    run.stop().await;
    let r = reader(&log);
    let s = state_at(&r, 2).unwrap();
    assert!(s.base.is_some());
    assert!(s.partial.is_some(), "{s:?}");

    // The matrix's other partial case (spec 7.12.1): a checkpoint whose point is missing
    // (an older build) or unknown (a newer one), with no `turn_end` checkpoint of the
    // step's turn before it. With its point known, the same boundary is exact.
    let mock = MockProvider::new(vec![
        tools_step(vec![write("w1", "a.txt", "one\n")]),
        text_step_no_usage("first"),
    ]);
    let mut run = start(patient(repo.builder(&mock)));
    run.turn_with("one").await;
    let log = run.current().1;
    run.stop().await;
    let exact = state_at(&reader(&log), 3).unwrap();
    assert_eq!(exact.partial, None, "{exact:?}");
    for point in [None, Some("from_the_future")] {
        let rewritten = without_points(&log, point);
        let s = state_at(&reader(&rewritten), 3).unwrap();
        assert_eq!(s.base, exact.base);
        assert!(s.partial.is_some(), "{point:?}: {s:?}");
    }
}

// ------------------------------------------------------------------------- rewind

#[tokio::test]
async fn rewind_creates_branch_and_leaves_old_log_unmodified_except_tip_checkpoint() {
    let repo = Repo::git_repo(AB);
    let mock = two_turns();
    let mut run = run_two_turns(&repo, &mock).await;
    let (old_id, old_log) = run.current();
    let before = raw_lines(&old_log);

    let (branch, report) = run.rewind(3, false, &[]).await.done();
    assert!(report.applied, "{report:?}");
    assert!(!report.dry_run);
    assert!(report.conflicts.is_empty());
    assert!(report.backup_ref.is_some());
    assert_ne!(branch, old_id);
    assert_eq!(run.current().0, branch);
    assert_eq!(repo.read("a.txt").as_deref(), Some("one\n"));

    // The old log: every line as it was, then only the command, the tip checkpoint and
    // the restore's begin and end (naming the backup) (spec 7.12.2).
    let after = raw_lines(&old_log);
    assert_eq!(after[..before.len()], before[..]);
    let added: Vec<Value> = after[before.len()..]
        .iter()
        .map(|l| serde_json::from_str::<Value>(l).unwrap()["data"].clone())
        .collect();
    let kinds: Vec<String> = added
        .iter()
        .map(|d| match d["type"].as_str().unwrap() {
            "checkpoint" => format!("checkpoint:{}", d["data"]["point"].as_str().unwrap()),
            "command" => format!("command:{}", d["data"]["type"].as_str().unwrap()),
            other => other.to_string(),
        })
        .collect();
    assert_eq!(
        kinds,
        [
            "command:rewind",
            "checkpoint:before_restore",
            "restore_begun",
            "restore_ended"
        ],
        "{added:?}"
    );
    assert_eq!(
        added[2]["data"]["backup_ref"].as_str(),
        report.backup_ref.as_deref()
    );
    assert_eq!(added[2]["data"]["by"], "rewind");
    assert_eq!(added[3]["data"]["outcome"], "applied");

    // The branch: origin rewind, parent (old session, offset of step 3's end).
    let branch_log = run.current().1;
    let b = reader(&branch_log);
    assert_eq!(b.header().origin, Origin::Rewind);
    let (parent, offset) = b.header().parent.clone().unwrap();
    assert_eq!(parent, old_id);
    assert_eq!(step_at_offset(&b, offset), 3);
    assert_eq!(step_at_offset(&reader(&old_log), offset), 3);
    // Its tip is the files it continues from: a checkpoint of its own with that tree.
    let tip = expected_tip(&b).unwrap();
    let at3 = state_at(&reader(&old_log), 3).unwrap();
    assert!(tip.overlay.is_empty() && at3.overlay.is_empty());
    let tree = |c: &str| repo.git(&["rev-parse", &format!("{c}^{{tree}}")]).unwrap();
    assert_eq!(tree(&tip.base.unwrap()), tree(&at3.base.unwrap()));
    assert!(matches!(
        records(&branch_log).last(),
        Some(SessionRecord::Checkpoint {
            point: Some(CheckpointPoint::Restored),
            step: 3,
            ..
        })
    ));
    run.stop().await;
}

#[tokio::test]
async fn rewind_restores_tool_and_bash_changes() {
    let repo = Repo::git_repo(AB);
    let mock = two_turns();
    let mut run = run_two_turns(&repo, &mock).await;
    assert_eq!(repo.read("c.txt").as_deref(), Some("x\n"));
    let (_, report) = run.rewind(3, false, &[]).await.done();
    assert!(report.applied);
    assert_eq!(repo.read("a.txt").as_deref(), Some("one\n"));
    assert_eq!(repo.read("b.txt").as_deref(), Some("bee\n"));
    assert_eq!(repo.read("c.txt"), None);
    let mut restored = report.restored.clone();
    restored.sort();
    assert_eq!(restored, ["a.txt", "b.txt"]);
    assert_eq!(report.deleted, ["c.txt"]);
    // The user's index and HEAD are untouched: only the work tree differs from HEAD.
    assert_eq!(repo.git(&["status", "--porcelain"]).unwrap(), " M a.txt\n");
    run.stop().await;
}

#[tokio::test]
async fn rewind_conflict_on_human_edit_writes_nothing_and_creates_no_branch() {
    let repo = Repo::git_repo(AB);
    let mock = two_turns();
    let mut run = run_two_turns(&repo, &mock).await;
    let (id, _) = run.current();
    repo.write("a.txt", "human\n");
    let sessions = logs(repo.home()).len();

    // The dry run says so.
    let (_, dry) = run.rewind(3, true, &[]).await.done();
    assert!(dry.dry_run && !dry.applied);
    assert_eq!(dry.conflicts, ["a.txt"]);

    let (branch, report) = run.rewind(3, false, &[]).await.done();
    assert!(!report.applied, "{report:?}");
    assert_eq!(report.conflicts, ["a.txt"]);
    assert_eq!(report.backup_ref, None);
    assert!(branch.is_empty());
    assert_eq!(repo.read("a.txt").as_deref(), Some("human\n"));
    assert_eq!(repo.read("c.txt").as_deref(), Some("x\n"));
    assert_eq!(run.current().0, id);
    assert_eq!(logs(repo.home()).len(), sessions);
    run.stop().await;
}

#[tokio::test]
async fn rewind_overwrite_list_applies_with_backup_ref() {
    let repo = Repo::git_repo(AB);
    let mock = two_turns();
    let mut run = run_two_turns(&repo, &mock).await;
    repo.write("a.txt", "human\n");
    let (branch, report) = run.rewind(3, false, &["a.txt"]).await.done();
    assert!(report.applied, "{report:?}");
    assert_eq!(report.conflicts, ["a.txt"]);
    assert!(!branch.is_empty());
    assert_eq!(repo.read("a.txt").as_deref(), Some("one\n"));
    // The backup holds the human edit.
    let backup = report.backup_ref.unwrap();
    assert!(backup.contains("/undo-"), "{backup}");
    assert_eq!(
        repo.git(&["show", &format!("{backup}:a.txt")]).as_deref(),
        Some("human\n")
    );
    run.stop().await;
}

#[tokio::test]
async fn human_edit_outside_changed_paths_left_alone() {
    let repo = Repo::git_repo(AB);
    repo.write("z.txt", "zed\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "z"]);
    let mock = two_turns();
    let mut run = run_two_turns(&repo, &mock).await;
    repo.write("z.txt", "human\n");
    repo.write("notes.txt", "mine\n");
    let (_, report) = run.rewind(3, false, &[]).await.done();
    assert!(report.applied, "{report:?}");
    assert!(report.conflicts.is_empty());
    assert_eq!(repo.read("z.txt").as_deref(), Some("human\n"));
    assert_eq!(repo.read("notes.txt").as_deref(), Some("mine\n"));
    assert_eq!(repo.read("a.txt").as_deref(), Some("one\n"));
    run.stop().await;
}

#[tokio::test]
async fn human_added_file_not_deleted_without_overwrite() {
    // Turn 2's command created c.txt; a human replaced it. The rewind would delete it: a
    // conflict. A file only a human added is no change at all.
    let repo = Repo::git_repo(AB);
    let mock = two_turns();
    let mut run = run_two_turns(&repo, &mock).await;
    repo.write("c.txt", "human\n");
    repo.write("new.txt", "mine\n");
    let (_, report) = run.rewind(3, false, &[]).await.done();
    assert!(!report.applied);
    assert_eq!(report.conflicts, ["c.txt"]);
    assert_eq!(repo.read("c.txt").as_deref(), Some("human\n"));
    assert_eq!(repo.read("a.txt").as_deref(), Some("two\n"));

    let (_, report) = run.rewind(3, false, &["c.txt"]).await.done();
    assert!(report.applied, "{report:?}");
    assert_eq!(report.deleted, ["c.txt"]);
    assert_eq!(repo.read("c.txt"), None);
    assert_eq!(repo.read("new.txt").as_deref(), Some("mine\n"));
    run.stop().await;
}

fn assemble(rebuilt: &tautly_core::resume::Rebuilt) -> CanonicalRequest {
    let caps = Repo::config()
        .effective_catalog()
        .get(&ModelId::from(MODEL))
        .caps;
    let specs: Vec<_> = tautly_tools::registry(Vec::new())
        .iter()
        .map(|t| t.spec(&caps))
        .collect();
    rebuilt
        .context
        .assemble(&caps, &specs, &ModelId::from(MODEL))
        .0
}

#[tokio::test]
async fn context_after_rewind_equals_rebuild_until() {
    let repo = Repo::git_repo(AB);
    let mut script = vec![
        tools_step(vec![write("w1", "a.txt", "one\n")]),
        text_step_no_usage("first"),
        tools_step(vec![write("w2", "a.txt", "two\n")]),
        text_step_no_usage("second"),
    ];
    script.push(text_step_no_usage("on the branch"));
    let mock = MockProvider::new(script);
    let mut run = run_two_turns(&repo, &mock).await;
    let old_log = run.current().1;
    let expected = assemble(&rebuild(&reader(&old_log), Some(3)).unwrap());
    run.rewind(3, false, &[]).await.done();
    run.turn_with("and now?").await;
    let req = mock.requests().last().unwrap().clone();
    let n = expected.messages.len();
    assert_eq!(req.system, expected.system);
    assert_eq!(req.messages[..n - 1], expected.messages[..n - 1]);
    let text = serde_json::to_string(&req.messages).unwrap();
    assert!(text.contains("and now?"));
    assert!(!text.contains("second"), "{text}");
    run.stop().await;
}

#[tokio::test]
async fn non_git_rewind_restores_tool_edits_and_notes_commands() {
    let repo = Repo::plain(A);
    let mock = MockProvider::new(vec![
        tools_step(vec![write("w1", "a.txt", "one\n")]),
        text_step_no_usage("first"),
        tools_step(vec![
            write("w2", "a.txt", "two\n"),
            bash("b1", "printf 'x\\n' > c.txt"),
        ]),
        text_step_no_usage("second"),
    ]);
    let mut run = run_two_turns(&repo, &mock).await;
    let (branch, report) = run.rewind(3, false, &[]).await.done();
    assert!(report.applied, "{report:?}");
    assert!(!branch.is_empty());
    assert_eq!(report.backup_ref, None);
    assert_eq!(report.restored, ["a.txt"]);
    assert_eq!(repo.read("a.txt").as_deref(), Some("one\n"));
    // The command's file is not known to the rewind.
    assert_eq!(repo.read("c.txt").as_deref(), Some("x\n"));
    let note = report.note.unwrap_or_default();
    assert!(note.contains("command"), "{note}");

    // A human edit is a conflict outside git too.
    let mut run2 = run;
    repo.write("a.txt", "human\n");
    let (old, _) = {
        let logs = logs(repo.home());
        let old = logs.into_iter().find(|p| p != &run2.current().1).unwrap();
        (reader(&old).header().session_id.clone(), ())
    };
    let (_, report) = run2.switch(&old, &[]).await.done();
    assert!(!report.applied);
    assert_eq!(report.conflicts, ["a.txt"]);
    run2.stop().await;
}

// ---------------------------------------------------------------- switch and lease

#[tokio::test]
async fn switch_back_and_forth_restores_each_tip_exactly() {
    let repo = Repo::git_repo(AB);
    let mut script = two_turns_script();
    script.extend([
        tools_step(vec![
            write("w3", "d.txt", "dee\n"),
            bash("b2", "printf 'branch\\n' > b.txt"),
        ]),
        text_step_no_usage("on the branch"),
    ]);
    let mock = MockProvider::new(script);
    let mut run = run_two_turns(&repo, &mock).await;
    let (a, _) = run.current();
    let tip_a = repo.files();

    let (b, _) = run.rewind(3, false, &[]).await.done();
    run.turn_with("branch work").await;
    let tip_b = repo.files();
    assert_ne!(tip_a, tip_b);

    for _ in 0..2 {
        let (to, report) = run.switch(&a, &[]).await.done();
        assert!(report.applied, "{report:?}");
        assert!(report.conflicts.is_empty(), "{report:?}");
        assert_eq!(to, a);
        assert_eq!(run.current().0, a);
        assert_eq!(repo.files(), tip_a);

        let (to, report) = run.switch(&b, &[]).await.done();
        assert!(report.applied, "{report:?}");
        assert_eq!(to, b);
        assert_eq!(repo.files(), tip_b);
    }
    // Switching to the session it is on is refused.
    let why = run.switch(&b, &[]).await.error();
    assert!(why.contains("already"), "{why}");
    run.stop().await;
}

fn two_turns_script() -> Vec<MockTurn> {
    vec![
        tools_step(vec![write("w1", "a.txt", "one\n")]),
        text_step_no_usage("first"),
        tools_step(vec![
            write("w2", "a.txt", "two\n"),
            bash("b1", "printf 'changed\\n' > b.txt && printf 'x\\n' > c.txt"),
        ]),
        text_step_no_usage("second"),
    ]
}

#[tokio::test]
async fn rewind_and_switch_refused_while_lease_shared_elsewhere() {
    let repo = Repo::git_repo(AB);
    let mock = two_turns();
    let mut run = run_two_turns(&repo, &mock).await;
    let (a, _) = run.current();
    // A second engine on the same work tree.
    let other = start(repo.builder(&MockProvider::new(vec![])));
    let other_id = other.engine.session_id().to_string();
    let why = run.rewind(3, false, &[]).await.error();
    assert!(why.contains("another live session"), "{why}");
    let why = run.switch(&other_id, &[]).await.error();
    assert!(why.contains("another live session"), "{why}");
    assert_eq!(repo.read("a.txt").as_deref(), Some("two\n"));
    assert_eq!(run.current().0, a);
    other.stop().await;

    // Sessions of this process sharing the workspace count too.
    let sb: Arc<dyn Sandbox> = Arc::new(LocalSandbox::new(repo.root()).unwrap());
    let ws = tautly_core::Workspace::new(sb, repo.home());
    let mock = two_turns();
    let mut shared = start(EngineBuilder {
        workspace: Some(Arc::clone(&ws)),
        ..repo.builder(&mock)
    });
    shared.turn_with("one").await;
    run.stop().await;
    let second = start(EngineBuilder {
        workspace: Some(Arc::clone(&ws)),
        ..repo.builder(&MockProvider::new(vec![]))
    });
    let why = shared.rewind(1, false, &[]).await.error();
    assert!(why.contains("another live session"), "{why}");
    second.stop().await;
    // Alone again: allowed.
    let (_, report) = shared.rewind(1, false, &[]).await.done();
    assert!(report.applied, "{report:?}");
    shared.stop().await;
}

#[tokio::test]
async fn fork_children_own_checkpoint_refs() {
    let repo = Repo::git_repo(AB);
    let mock = two_turns();
    let mut run = run_two_turns(&repo, &mock).await;
    let (a, _) = run.current();
    let (b, _) = run.rewind(3, false, &[]).await.done();
    let b_log = run.current().1;
    // Back on the parent, whose tip the files are again.
    run.switch(&a, &[]).await.done();
    run.stop().await;

    // Every checkpoint the branch's log names has a ref of the branch's own.
    let refs = repo
        .git(&[
            "for-each-ref",
            "--format=%(objectname)",
            &format!("refs/tautly/checkpoints/{b}/"),
        ])
        .unwrap();
    for rec in records(&b_log) {
        if let SessionRecord::Checkpoint { commit, .. } = rec {
            assert!(refs.contains(&commit), "{commit} has no ref of {b}: {refs}");
        }
    }

    // Delete the parent (its refs go) and drop unreachable objects: the branch still
    // materializes, from its own refs.
    let sb = LocalSandbox::new(repo.root()).unwrap();
    tautly_core::sessions::delete(repo.home(), &sb, &a)
        .await
        .unwrap();
    repo.git(&["reflog", "expire", "--expire=now", "--all"]);
    repo.git(&["gc", "-q", "--prune=now"]).unwrap();
    assert_eq!(repo.read("c.txt").as_deref(), Some("x\n"));
    let mut run = start(repo.builder(&MockProvider::new(vec![])));
    let (_, report) = run.switch(&b, &[]).await.done();
    assert!(report.applied, "{report:?}");
    assert!(report.conflicts.is_empty(), "{report:?}");
    assert_eq!(repo.read("a.txt").as_deref(), Some("one\n"));
    assert_eq!(repo.read("b.txt").as_deref(), Some("bee\n"));
    assert_eq!(repo.read("c.txt"), None);
    run.stop().await;
}

/// Compare's ledger rule (Task 52) relies on this: a fork's ledger rows are its own
/// requests only, never the parent's it copied, so the rows cover exactly the records
/// after its header's parent offset.
#[tokio::test]
async fn a_fork_childs_ledger_rows_hold_only_its_post_fork_requests() {
    let repo = Repo::git_repo(AB);
    let mock = MockProvider::new(vec![
        text_step_no_usage("one"),
        text_step_no_usage("two"),
        text_step_no_usage("three"),
    ]);
    let mut run = start(repo.builder(&mock));
    run.turn_with("first").await;
    let (parent, _) = run.current();
    run.engine
        .send(Command::Fork {
            id: None,
            at: None,
            label: Some("b".into()),
        })
        .await
        .unwrap();
    run.until(|e| matches!(e, Event::SessionChanged { .. }))
        .await;
    let (child, child_log) = run.current();
    assert_ne!(child, parent);
    run.turn_with("second").await;
    run.turn_with("third").await;
    run.stop().await;

    let ledger =
        tautly_core::Ledger::open(&repo.home().join(tautly_core::ledger::LEDGER_FILE)).unwrap();
    assert_eq!(ledger.session_totals(&parent).unwrap().requests, 1);
    assert_eq!(ledger.session_totals(&child).unwrap().requests, 2);

    // Compare, cut at the child's own fork record, takes the child's rows as they are.
    let r = reader(&child_log);
    let cut = r.header().parent.as_ref().unwrap().1;
    let s = tautly_core::compare::summarize_at(&r, Some(&ledger), &Catalog::default(), Some(cut))
        .unwrap();
    let rows = ledger.session_totals(&child).unwrap();
    assert_eq!((s.tokens_in, s.tokens_out), (rows.input, rows.output));
    assert_eq!(s.user_turns, 2);
}

// ------------------------------------------------------------ labels and the tree

#[tokio::test]
async fn label_command_renames_branch() {
    let repo = Repo::git_repo(AB);
    let mock = MockProvider::new(vec![text_step_no_usage("hi")]);
    let mut run = start(repo.builder(&mock));
    run.turn_with("hello").await;
    run.engine
        .send(Command::Label {
            label: "main-line".into(),
        })
        .await
        .unwrap();
    let log = run.current().1;
    let id = run.current().0;
    run.stop().await;
    assert_eq!(
        session_label(&reader(&log)).unwrap().as_deref(),
        Some("main-line")
    );
    let nodes = tree::build(repo.home(), &repo.project(), None).unwrap();
    let node = nodes.iter().find(|n| n.session_id == id).unwrap();
    assert_eq!(node.label.as_deref(), Some("main-line"));
}

#[tokio::test]
async fn tree_lists_parent_step_label_origin_live_current() {
    let repo = Repo::git_repo(AB);
    let mock = two_turns();
    let mut run = run_two_turns(&repo, &mock).await;
    let (a, _) = run.current();
    let answer = run
        .answer(Command::Rewind {
            id: None,
            at: 3,
            label: Some("try-b".into()),
            dry_run: false,
            overwrite: vec![],
        })
        .await;
    let (b, _) = answer.done();
    let nodes = tree::build(repo.home(), &repo.project(), Some(&b)).unwrap();
    assert_eq!(nodes.len(), 2, "{nodes:?}");
    // Parents first.
    assert_eq!(nodes[0].session_id, a);
    assert_eq!(nodes[0].parent, None);
    assert_eq!(nodes[0].origin, Origin::User);
    assert_eq!(nodes[0].last_step, 6);
    assert_eq!(nodes[0].last_prompt, "two");
    assert!(!nodes[0].live && !nodes[0].current);
    assert_eq!(nodes[1].session_id, b);
    assert_eq!(nodes[1].parent, Some((a.clone(), 3)));
    assert_eq!(nodes[1].label.as_deref(), Some("try-b"));
    assert_eq!(nodes[1].origin, Origin::Rewind);
    assert_eq!(nodes[1].last_step, 3);
    assert_eq!(nodes[1].last_prompt, "one");
    assert!(nodes[1].live && nodes[1].current);

    let lines = tree::lines(&nodes);
    assert_eq!(lines.len(), 2);
    assert!(
        lines[0].starts_with(&format!("  {}  step 6 · user · ", &a[..8])),
        "{lines:?}"
    );
    assert!(lines[0].ends_with(" · two"), "{lines:?}");
    assert!(
        lines[1].starts_with("  * try-b  step 3 · rewind · "),
        "{lines:?}"
    );
    run.stop().await;
    // The log is closed (its lock released) before the event stream ends (review M6).
    let nodes = tree::build(repo.home(), &repo.project(), None).unwrap();
    assert!(nodes.iter().all(|n| !n.live && !n.current), "{nodes:?}");
    // So resuming it at once finds it free, not "already open for writing".
    let again = start(EngineBuilder {
        resume: Some(b.clone()),
        ..repo.builder(&MockProvider::new(vec![]))
    });
    again.stop().await;
}

// ------------------------------------------------- fix round 1: human-owned changes

/// A field of a report as it goes over the wire.
fn wire(report: &MaterializeReport, field: &str) -> Value {
    serde_json::to_value(report).unwrap()[field].clone()
}

#[tokio::test]
async fn offline_human_edit_absorbed_by_resume_is_a_conflict() {
    // I2: the resume's start checkpoint holds the edit; it is still the human's.
    let repo = Repo::git_repo(AB);
    let mock = two_turns();
    let run = run_two_turns(&repo, &mock).await;
    let id = run.current().0;
    run.stop().await;
    repo.write("a.txt", "offline\n");
    let mut run = start(EngineBuilder {
        resume: Some(id),
        ..repo.builder(&MockProvider::new(vec![]))
    });
    let (_, report) = run.rewind(3, false, &[]).await.done();
    assert!(!report.applied, "{report:?}");
    assert!(
        report.conflicts.contains(&"a.txt".to_string()),
        "{report:?}"
    );
    assert_eq!(repo.read("a.txt").as_deref(), Some("offline\n"));
    let (_, report) = run.rewind(3, false, &["a.txt"]).await.done();
    assert!(report.applied, "{report:?}");
    assert_eq!(repo.read("a.txt").as_deref(), Some("one\n"));
    run.stop().await;
}

#[tokio::test]
async fn human_edit_between_turns_is_a_conflict() {
    // I3: the next turn's first checkpoint holds the edits; they are still the human's.
    let repo = Repo::git_repo(AB);
    let mock = MockProvider::new(vec![
        tools_step(vec![write("w1", "a.txt", "one\n")]),
        text_step_no_usage("first"),
        tools_step(vec![
            write("w2", "a.txt", "two\n"),
            bash("b1", "printf 'x\\n' > c.txt"),
        ]),
        text_step_no_usage("second"),
    ]);
    let mut run = start(repo.builder(&mock));
    run.turn_with("one").await;
    repo.write("b.txt", "mine\n");
    repo.write("mine.txt", "new\n");
    run.turn_with("two").await;
    let (_, report) = run.rewind(3, false, &[]).await.done();
    assert!(!report.applied, "{report:?}");
    let mut conflicts = report.conflicts.clone();
    conflicts.sort();
    assert_eq!(conflicts, ["b.txt", "mine.txt"], "{report:?}");
    assert_eq!(repo.read("b.txt").as_deref(), Some("mine\n"));
    assert_eq!(repo.read("mine.txt").as_deref(), Some("new\n"));
    assert_eq!(repo.read("a.txt").as_deref(), Some("two\n"));
    // A rewind to after the edits does not touch them.
    let (_, report) = run.rewind(4, true, &[]).await.done();
    assert!(report.conflicts.is_empty(), "{report:?}");
    let (_, report) = run.rewind(3, false, &["b.txt", "mine.txt"]).await.done();
    assert!(report.applied, "{report:?}");
    assert_eq!(repo.read("b.txt").as_deref(), Some("bee\n"));
    assert_eq!(repo.read("mine.txt"), None);
    run.stop().await;
}

#[tokio::test]
async fn a_symlinked_parent_is_blocked_and_never_written_through() {
    // I1, from git (the checkpoint) and outside git (a pre-image).
    for git in [true, false] {
        let repo = if git {
            Repo::git_repo(AB)
        } else {
            Repo::plain(A)
        };
        let mock = MockProvider::new(vec![
            tools_step(vec![write("w1", "sub/y.txt", "one\n")]),
            text_step_no_usage("first"),
            tools_step(vec![write("w2", "sub/y.txt", "two\n")]),
            text_step_no_usage("second"),
        ]);
        let mut run = run_two_turns(&repo, &mock).await;
        std::fs::remove_dir_all(repo.root().join("sub")).unwrap();
        repo.write("other/y.txt", "mine\n");
        std::os::unix::fs::symlink("other", repo.root().join("sub")).unwrap();
        for overwrite in [&[][..], &["sub/y.txt"][..]] {
            let (_, report) = run.rewind(3, false, overwrite).await.done();
            assert!(!report.applied, "git {git}: {report:?}");
            assert_eq!(wire(&report, "blocked"), json!(["sub/y.txt"]), "{report:?}");
            // N11: the reason is its own.
            let note = report.note.clone().unwrap_or_default();
            assert!(
                note.contains("a symbolic link or a file is above it"),
                "{note}"
            );
            assert!(!note.contains("snapshot cannot hold"), "{note}");
            assert_eq!(repo.read("other/y.txt").as_deref(), Some("mine\n"));
            let link = std::fs::symlink_metadata(repo.root().join("sub")).unwrap();
            assert!(link.file_type().is_symlink(), "git {git}");
        }
        run.stop().await;
    }
}

#[tokio::test]
async fn a_tool_edit_of_an_ignored_file_is_reverted() {
    // M4: no checkpoint holds `gen.log`; its pre-image (none) is what it goes back to.
    let repo = Repo::git_repo(AB);
    repo.write(".gitignore", "*.log\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "ignore"]);
    let mock = MockProvider::new(vec![
        tools_step(vec![write("w1", "a.txt", "one\n")]),
        text_step_no_usage("first"),
        tools_step(vec![write("w2", "gen.log", "generated\n")]),
        text_step_no_usage("second"),
    ]);
    let mut run = run_two_turns(&repo, &mock).await;
    repo.write("h.log", "the human's\n");
    let (_, report) = run.rewind(3, false, &[]).await.done();
    assert!(report.applied, "{report:?}");
    assert_eq!(report.deleted, ["gen.log"], "{report:?}");
    assert_eq!(repo.read("gen.log"), None);
    assert_eq!(repo.read("h.log").as_deref(), Some("the human's\n"));
    run.stop().await;
}

#[tokio::test]
async fn a_mode_change_is_a_conflict() {
    use std::os::unix::fs::PermissionsExt;
    let repo = Repo::git_repo(AB);
    let mock = two_turns();
    let mut run = run_two_turns(&repo, &mock).await;
    let a = repo.root().join("a.txt");
    std::fs::set_permissions(&a, std::fs::Permissions::from_mode(0o755)).unwrap();
    let (_, report) = run.rewind(3, false, &[]).await.done();
    assert!(!report.applied);
    assert_eq!(report.conflicts, ["a.txt"]);
    assert_eq!(repo.read("a.txt").as_deref(), Some("two\n"));
    run.stop().await;
}

#[tokio::test]
async fn rewind_to_step_zero_branches_before_the_first_step() {
    // I6.
    let repo = Repo::git_repo(AB);
    let mock = two_turns();
    let mut run = run_two_turns(&repo, &mock).await;
    // From a branch too (its copy holds nothing kept before step 1, review probe p9).
    run.rewind(4, false, &[]).await.done();
    let (a, _) = run.current();
    let (branch, report) = run.rewind(0, false, &[]).await.done();
    assert!(report.applied, "{report:?}");
    assert!(!branch.is_empty());
    assert_eq!(repo.read("a.txt").as_deref(), Some("zero\n"));
    assert_eq!(repo.read("b.txt").as_deref(), Some("bee\n"));
    assert_eq!(repo.read("c.txt"), None);
    let b = reader(&run.current().1);
    let (parent, offset) = b.header().parent.clone().unwrap();
    assert_eq!(parent, a);
    assert_eq!(step_at_offset(&b, offset), 0);
    assert_eq!(step_at_offset(&b, u64::MAX), 0);
    run.stop().await;
}

#[tokio::test]
async fn a_large_human_file_is_a_conflict_and_backed_up_in_git() {
    // Over the 64 MiB read limit: compared by its streaming hash, kept whole by the backup.
    let repo = Repo::git_repo(AB);
    let mock = two_turns();
    let mut run = run_two_turns(&repo, &mock).await;
    let size = 65 * 1024 * 1024 + 7;
    std::fs::write(repo.root().join("c.txt"), vec![b'h'; size]).unwrap();
    let (_, report) = run.rewind(3, false, &[]).await.done();
    assert!(!report.applied);
    assert_eq!(report.conflicts, ["c.txt"]);
    let (_, report) = run.rewind(3, false, &["c.txt"]).await.done();
    assert!(report.applied, "{report:?}");
    assert_eq!(repo.read("c.txt"), None);
    let backup = report.backup_ref.unwrap();
    let kept = repo
        .git(&["cat-file", "-s", &format!("{backup}:c.txt")])
        .unwrap();
    assert_eq!(kept.trim(), size.to_string());
    run.stop().await;
}

/// Run `f` with `dir` read-only, then make it writable again.
fn read_only(dir: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode)).unwrap();
}

#[tokio::test]
async fn a_failed_write_puts_the_files_back_and_names_the_backup() {
    // I4: c.txt is deleted first; then d/a.txt cannot be written (its directory is
    // read-only). The deletion is undone and the error says so, with the snapshot.
    let repo = Repo::git_repo(AB);
    let mock = MockProvider::new(vec![
        tools_step(vec![write("w1", "d/a.txt", "one\n")]),
        text_step_no_usage("first"),
        tools_step(vec![
            write("w2", "d/a.txt", "two\n"),
            bash("b1", "printf 'x\\n' > c.txt"),
        ]),
        text_step_no_usage("second"),
    ]);
    let mut run = run_two_turns(&repo, &mock).await;
    let d = repo.root().join("d");
    read_only(&d, 0o555);
    let why = run.rewind(3, false, &[]).await.error();
    read_only(&d, 0o755);
    assert!(why.contains("put back as they were"), "{why}");
    assert!(why.contains("snapshot refs/tautly/checkpoints/"), "{why}");
    assert_eq!(repo.read("c.txt").as_deref(), Some("x\n"));
    assert_eq!(repo.read("d/a.txt").as_deref(), Some("two\n"));
    run.stop().await;
}

#[tokio::test]
async fn a_failed_branch_puts_the_files_back() {
    // I4: the branch's log cannot be created (its directory is read-only).
    let repo = Repo::git_repo(AB);
    let mock = two_turns();
    let mut run = run_two_turns(&repo, &mock).await;
    let (id, log) = run.current();
    let dir = log.parent().unwrap().to_path_buf();
    read_only(&dir, 0o555);
    let why = run.rewind(3, false, &[]).await.error();
    read_only(&dir, 0o755);
    assert!(why.contains("put back as they were"), "{why}");
    assert_eq!(repo.read("a.txt").as_deref(), Some("two\n"));
    assert_eq!(repo.read("c.txt").as_deref(), Some("x\n"));
    assert_eq!(run.current().0, id);
    // The old log says how it ended.
    let last: Value = serde_json::from_str(raw_lines(&log).last().unwrap()).unwrap();
    assert_eq!(last["data"]["type"], "restore_ended", "{last}");
    assert_eq!(last["data"]["data"]["outcome"], "rolled_back");
    run.stop().await;
}

#[tokio::test]
async fn outside_git_an_overwrite_keeps_the_content_in_the_session_log() {
    // I5: no snapshot; the bytes go to the blob store, in a `RestoreBegun` record.
    let repo = Repo::plain(A);
    let mock = MockProvider::new(vec![
        tools_step(vec![write("w1", "a.txt", "one\n")]),
        text_step_no_usage("first"),
        tools_step(vec![write("w2", "a.txt", "two\n")]),
        text_step_no_usage("second"),
    ]);
    let mut run = run_two_turns(&repo, &mock).await;
    let log = run.current().1;
    repo.write("a.txt", "human\n");
    let (_, dry) = run.rewind(3, true, &[]).await.done();
    assert_eq!(wire(&dry, "backup"), json!("log"), "{dry:?}");
    let (_, report) = run.rewind(3, false, &["a.txt"]).await.done();
    assert!(report.applied, "{report:?}");
    assert_eq!(wire(&report, "backup"), json!("log"));
    let begun: Value = raw_lines(&log)
        .iter()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .find(|v| v["data"]["type"] == "restore_begun")
        .expect("a RestoreBegun record");
    let file = &begun["data"]["data"]["files"][0];
    assert_eq!(file["path"], "a.txt");
    let blob = tautly_session::BlobRef::parse(file["from_blob"].as_str().unwrap()).unwrap();
    assert_eq!(reader(&log).read_blob(&blob).unwrap(), b"human\n");
    // The branch's assertion records are marked as such (M5).
    let branch_log = run.current().1;
    let edits: Vec<Value> = raw_lines(&branch_log)
        .iter()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .filter(|v| v["data"]["type"] == "edit")
        .collect();
    assert_eq!(edits.last().unwrap()["data"]["data"]["asserted"], true);

    // A file over 64 MiB cannot be kept: the report says no backup.
    std::fs::write(repo.root().join("a.txt"), vec![b'h'; 65 * 1024 * 1024]).unwrap();
    let (_, dry) = run.rewind(1, true, &[]).await.done();
    assert_eq!(dry.conflicts, ["a.txt"], "{dry:?}");
    assert_eq!(wire(&dry, "backup"), Value::Null, "{dry:?}");
    run.stop().await;
}

#[tokio::test]
async fn an_unknown_step_changes_nothing() {
    // M6: refused before the tip checkpoint is taken.
    let repo = Repo::git_repo(AB);
    let mock = two_turns();
    let mut run = run_two_turns(&repo, &mock).await;
    let log = run.current().1;
    let before = raw_lines(&log).len();
    let why = run.rewind(99, false, &[]).await.error();
    assert!(why.contains("no step 99"), "{why}");
    run.stop().await;
    let added: Vec<Value> = raw_lines(&log)[before..]
        .iter()
        .map(|l| serde_json::from_str::<Value>(l).unwrap()["data"]["type"].clone())
        .collect();
    assert!(!added.contains(&json!("checkpoint")), "{added:?}");
}

#[tokio::test]
async fn a_restore_cut_off_is_warned_about_at_resume() {
    // M7: a `RestoreBegun` with no `RestoreEnded` (a crash mid-restore).
    let repo = Repo::git_repo(AB);
    let mock = MockProvider::new(vec![text_step_no_usage("hi")]);
    let mut run = start(repo.builder(&mock));
    run.turn_with("hello").await;
    let (id, log) = run.current();
    run.stop().await;
    let offset = records(&log).len();
    let line = json!({"type": "record", "offset": offset, "t": 0, "data": {
        "type": "restore_begun",
        "data": {"backup_ref": "refs/tautly/checkpoints/x/undo-9", "files": [],
                 "paths": ["a.txt", "b.txt"]}}});
    let mut text = std::fs::read_to_string(&log).unwrap();
    text.push_str(&format!("{line}\n"));
    std::fs::write(&log, text).unwrap();
    let mut run = start(EngineBuilder {
        resume: Some(id),
        ..repo.builder(&MockProvider::new(vec![]))
    });
    let seen = run
        .until(|e| matches!(e, Event::Warning { message } if message.contains("cut off")))
        .await;
    let Some(Event::Warning { message }) = seen.last() else {
        unreachable!()
    };
    assert!(
        message.contains("refs/tautly/checkpoints/x/undo-9"),
        "{message}"
    );
    // N8: only the listed paths are suggested, after a look at the difference.
    assert!(
        message.contains("git diff refs/tautly/checkpoints/x/undo-9 -- a.txt b.txt"),
        "{message}"
    );
    assert!(!message.contains("-- ."), "{message}");
    run.stop().await;
}

// ------------------------------------------------- fix round 2

#[tokio::test]
async fn human_edit_after_a_cancelled_command_turn_is_a_conflict() {
    // N1 (review probe p13b): turn 1's command is cancelled; its end checkpoint is taken
    // after it, so the human's edit between turns falls in a command-free pair.
    let repo = Repo::git_repo(AB);
    let mock = MockProvider::new(vec![
        tools_step(vec![bash("b1", "sleep 30")]),
        tools_step(vec![write("w1", "a.txt", "two\n")]),
        text_step_no_usage("second"),
    ]);
    let mut run = start(repo.builder(&mock));
    run.engine
        .send(Command::SendMessage {
            id: None,
            text: "one".into(),
        })
        .await
        .unwrap();
    run.until(|e| matches!(e, Event::ToolStarted { .. })).await;
    run.engine.send(Command::Cancel).await.unwrap();
    run.until(|e| matches!(e, Event::TurnFinished { .. })).await;
    // At once (review probe p13b): the end checkpoint may well be taken after the edit.
    repo.write("b.txt", "mine\n");
    run.turn_with("two").await;
    // Whether the end checkpoint was taken before the edit (then the edit is a conflict)
    // or after it (then step 2's files hold it already), the edit is never written over.
    let (_, report) = run.rewind(2, false, &[]).await.done();
    let b = "b.txt".to_string();
    assert!(
        report.conflicts.contains(&b) || !report.restored.contains(&b),
        "{report:?}"
    );
    assert!(!report.deleted.contains(&b), "{report:?}");
    assert_eq!(repo.read("b.txt").as_deref(), Some("mine\n"));
    run.stop().await;
}

#[tokio::test]
async fn an_offline_edit_after_a_crash_in_a_command_turn_is_a_conflict() {
    // N1 round 3: the turn's end was never checkpointed (the log was cut after its
    // command); the pair that ends in the resume's checkpoint spans that cut end, so what
    // changed in it unexplained is the human's: the offline edit, and also the command's
    // own file (a spurious conflict, which fails safe). The snapshots get a generous time
    // limit: one skipped under load leaves the offline edit out of the tip (Task 49
    // M-R3a), which is safe but not what this checks.
    let repo = Repo::git_repo(AB);
    let patient = |mut b: EngineBuilder| {
        b.config.checkpoints.timeout_ms = 120_000;
        b
    };
    let mock = MockProvider::new(vec![
        tools_step(vec![bash("b1", "printf 'cmd\\n' > c.txt")]),
        text_step_no_usage("done"),
    ]);
    let mut run = start(patient(repo.builder(&mock)));
    let id = run.engine.session_id().to_string();
    run.turn_with("go").await;
    let log = run.current().1;
    run.stop().await;
    let recs = records(&log);
    let cut = recs
        .iter()
        .rposition(|r| matches!(r, SessionRecord::ToolResult { .. }))
        .unwrap();
    let text = std::fs::read_to_string(&log).unwrap();
    let kept: Vec<&str> = text.lines().take(cut + 2).collect();
    std::fs::write(&log, kept.join("\n") + "\n").unwrap();
    repo.write("b.txt", "offline\n");
    let mut run = start(EngineBuilder {
        resume: Some(id),
        ..patient(repo.builder(&MockProvider::new(vec![])))
    });
    let (_, report) = run.rewind(1, false, &[]).await.done();
    // Whatever else happens, the human edit is never overwritten.
    assert_eq!(repo.read("b.txt").as_deref(), Some("offline\n"));
    assert!(!report.applied, "{report:?}");
    assert_eq!(report.conflicts, ["b.txt", "c.txt"], "{report:?}");
    run.stop().await;
}

#[tokio::test]
async fn an_offline_edit_after_cancel_then_quit_is_a_conflict() {
    // N1 round 3 (review probe p28): cancel, quit at once, edit offline, resume, rewind.
    let repo = Repo::git_repo(AB);
    let mock = MockProvider::new(vec![tools_step(vec![bash("b1", "sleep 30")])]);
    let mut run = start(repo.builder(&mock));
    let id = run.engine.session_id().to_string();
    run.engine
        .send(Command::SendMessage {
            id: None,
            text: "one".into(),
        })
        .await
        .unwrap();
    run.until(|e| matches!(e, Event::ToolStarted { .. })).await;
    run.engine.send(Command::Cancel).await.unwrap();
    run.until(|e| matches!(e, Event::TurnFinished { .. })).await;
    run.stop().await;
    repo.write("b.txt", "offline\n");
    let mut run = start(EngineBuilder {
        resume: Some(id),
        ..repo.builder(&MockProvider::new(vec![]))
    });
    let (_, report) = run.rewind(1, false, &[]).await.done();
    assert!(!report.applied, "{report:?}");
    assert_eq!(report.conflicts, ["b.txt"], "{report:?}");
    assert_eq!(repo.read("b.txt").as_deref(), Some("offline\n"));
    run.stop().await;
}

fn ignoring_logs() -> Repo {
    let repo = Repo::git_repo(AB);
    repo.write(".gitignore", "*.log\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "ignore"]);
    repo
}

fn writes_gen_log() -> MockProvider {
    MockProvider::new(vec![
        tools_step(vec![write("w1", "a.txt", "one\n")]),
        text_step_no_usage("first"),
        tools_step(vec![write("w2", "gen.log", "generated\n")]),
        text_step_no_usage("second"),
    ])
}

#[tokio::test]
async fn an_ignored_file_is_kept_in_the_log_before_a_git_overwrite() {
    // N2 (review probe p14): the snapshot cannot hold an ignored file.
    let repo = ignoring_logs();
    let mock = writes_gen_log();
    let mut run = run_two_turns(&repo, &mock).await;
    let log = run.current().1;
    repo.write("gen.log", "the human's\n");
    let (_, report) = run.rewind(3, false, &[]).await.done();
    assert_eq!(report.conflicts, ["gen.log"], "{report:?}");
    let (_, report) = run.rewind(3, false, &["gen.log"]).await.done();
    assert!(report.applied, "{report:?}");
    assert_eq!(repo.read("gen.log"), None);
    let backup = report.backup_ref.clone().unwrap();
    assert_eq!(repo.git(&["show", &format!("{backup}:gen.log")]), None);
    let begun: Value = raw_lines(&log)
        .iter()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .rfind(|v| v["data"]["type"] == "restore_begun")
        .unwrap();
    let file = &begun["data"]["data"]["files"][0];
    assert_eq!(file["path"], "gen.log", "{begun}");
    let blob = tautly_session::BlobRef::parse(file["from_blob"].as_str().unwrap()).unwrap();
    assert_eq!(reader(&log).read_blob(&blob).unwrap(), b"the human's\n");
    assert_eq!(begun["data"]["data"]["paths"], json!(["gen.log"]));
    run.stop().await;
}

#[tokio::test]
async fn an_ignored_file_too_large_to_keep_is_blocked() {
    // N2: over 64 MiB, it could be neither kept nor put back: never overwritten.
    let repo = ignoring_logs();
    let mock = writes_gen_log();
    let mut run = run_two_turns(&repo, &mock).await;
    let size = 65 * 1024 * 1024;
    std::fs::write(repo.root().join("gen.log"), vec![b'h'; size]).unwrap();
    for overwrite in [&[][..], &["gen.log"][..]] {
        let (_, report) = run.rewind(3, false, overwrite).await.done();
        assert!(!report.applied, "{report:?}");
        assert_eq!(report.blocked, ["gen.log"], "{report:?}");
        assert!(report.conflicts.is_empty(), "{report:?}");
    }
    assert_eq!(
        std::fs::metadata(repo.root().join("gen.log"))
            .unwrap()
            .len(),
        size as u64
    );
    run.stop().await;
}

#[tokio::test]
async fn a_switch_restores_an_ignored_file_the_target_wrote() {
    // N7: the target's tool wrote `gen.log`; no checkpoint holds it.
    let repo = ignoring_logs();
    let mock = writes_gen_log();
    let mut run = run_two_turns(&repo, &mock).await;
    let (a, _) = run.current();
    run.rewind(3, false, &[]).await.done();
    assert_eq!(repo.read("gen.log"), None);
    let (_, report) = run.switch(&a, &[]).await.done();
    assert!(report.applied, "{report:?}");
    assert_eq!(repo.read("gen.log").as_deref(), Some("generated\n"));
    run.stop().await;
}

#[tokio::test]
async fn a_switch_to_a_locked_session_logs_nothing() {
    // N7: the target's lock is checked before the tip checkpoint is taken.
    use std::os::fd::AsRawFd;
    let repo = Repo::git_repo(AB);
    let mock = two_turns();
    let mut run = run_two_turns(&repo, &mock).await;
    let (a, _) = run.current();
    let (_, b_log) = {
        run.rewind(3, false, &[]).await.done();
        run.current()
    };
    let a_log = logs(repo.home())
        .into_iter()
        .find(|p| reader(p).header().session_id == a)
        .unwrap();
    let held = std::fs::File::open(&a_log).unwrap();
    // SAFETY: flock on a file descriptor we own.
    assert_eq!(unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_EX) }, 0);
    let before = raw_lines(&b_log).len();
    let why = run.switch(&a, &[]).await.error();
    assert!(why.contains("open"), "{why}");
    drop(held);
    let added: Vec<Value> = raw_lines(&b_log)[before..]
        .iter()
        .map(|l| serde_json::from_str::<Value>(l).unwrap()["data"]["type"].clone())
        .collect();
    assert!(!added.contains(&json!("checkpoint")), "{added:?}");
    run.stop().await;
}

#[tokio::test]
async fn a_switch_records_the_restore_in_both_sessions() {
    // N8: the session switched to is the one usually resumed.
    let repo = Repo::git_repo(AB);
    let mock = two_turns();
    let mut run = run_two_turns(&repo, &mock).await;
    let (a, a_log) = run.current();
    run.rewind(3, false, &[]).await.done();
    run.switch(&a, &[]).await.done();
    let types: Vec<Value> = raw_lines(&a_log)
        .iter()
        .map(|l| serde_json::from_str::<Value>(l).unwrap()["data"]["type"].clone())
        .collect();
    let begun = types.iter().filter(|t| **t == "restore_begun").count();
    let ended = types.iter().filter(|t| **t == "restore_ended").count();
    // The rewind's (A was left) and the switch's (A was switched to).
    assert_eq!((begun, ended), (2, 2), "{types:?}");
    run.stop().await;
}

#[tokio::test]
async fn an_engine_never_runs_without_the_lease() {
    // N3 (review probe p15): another engine's rewind holds it exclusively too long.
    let repo = Repo::git_repo(AB);
    let lease = tautly_core::Lease::shared(repo.home(), &repo.root()).unwrap();
    let held = lease.try_exclusive().unwrap();
    let mock = MockProvider::new(vec![text_step_no_usage("never")]);
    let mut run = start(EngineBuilder {
        lease_wait: Duration::from_millis(300),
        ..repo.builder(&mock)
    });
    let seen = run.until(|e| matches!(e, Event::Error { .. })).await;
    let Some(Event::Error { message, .. }) = seen.last() else {
        unreachable!()
    };
    assert!(message.contains("cannot start the session"), "{message}");
    run.engine
        .send(Command::SendMessage {
            id: None,
            text: "hi".into(),
        })
        .await
        .unwrap();
    let seen = run.until(|e| matches!(e, Event::Error { .. })).await;
    assert!(
        matches!(seen.last(), Some(Event::Error { message, .. }) if message.contains("takes no more commands")),
        "{seen:?}"
    );
    // Review CP5-A I1: the message's turn still ends, and a replay still finishes.
    let seen = run.until(|e| matches!(e, Event::TurnFinished { .. })).await;
    assert!(
        matches!(
            seen.last(),
            Some(Event::TurnFinished {
                reason: FinishReason::Error,
                ..
            })
        ),
        "{seen:?}"
    );
    run.engine
        .send(Command::Replay {
            id: None,
            session_id: None,
            speed: 0.0,
            from_step: None,
        })
        .await
        .unwrap();
    let seen = run
        .until(|e| matches!(e, Event::ReplayFinished { .. }))
        .await;
    assert!(
        matches!(seen.last(), Some(Event::ReplayFinished { events: 0, .. })),
        "{seen:?}"
    );
    assert!(mock.requests().is_empty());
    drop(held);
    run.stop().await;

    // A lock file that cannot be opened fails the build.
    let repo = Repo::git_repo(AB);
    std::fs::write(repo.home().join("locks"), "not a directory").unwrap();
    let built = repo.builder(&MockProvider::new(vec![])).build();
    let err = built.err().expect("built without a lease");
    assert!(err.to_string().contains("working-tree lease"), "{err}");
}

/// `restore_backup`, once the stopped engine has let go of the lease (its actor ends just
/// after its event stream).
async fn restore_backup(
    repo: &Repo,
    id: &str,
    opts: &tautly_core::sessions::RestoreBackupOptions,
) -> Result<tautly_core::sessions::BackupRestored, tautly_core::sessions::BackupError> {
    let sb = LocalSandbox::new(repo.root()).unwrap();
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        match tautly_core::sessions::restore_backup(repo.home(), &sb, id, opts).await {
            Err(tautly_core::sessions::BackupError::Lease(_))
                if tokio::time::Instant::now() < deadline =>
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            other => return other,
        }
    }
}

fn opts(
    dry_run: bool,
    force: bool,
    overwrite: &[&str],
) -> tautly_core::sessions::RestoreBackupOptions {
    tautly_core::sessions::RestoreBackupOptions {
        dry_run,
        force,
        overwrite: overwrite.iter().map(|s| s.to_string()).collect(),
        include_own: false,
    }
}

/// [`opts`] that also considers the backups `restore-backup` kept itself.
fn own_opts(force: bool, overwrite: &[&str]) -> tautly_core::sessions::RestoreBackupOptions {
    tautly_core::sessions::RestoreBackupOptions {
        include_own: true,
        ..opts(false, force, overwrite)
    }
}

/// Outside git: two turns writing a.txt, a human edit, a rewind that overwrites it.
async fn overwritten_outside_git() -> (Repo, String) {
    let repo = Repo::plain(A);
    let mock = MockProvider::new(vec![
        tools_step(vec![write("w1", "a.txt", "one\n")]),
        text_step_no_usage("first"),
        tools_step(vec![write("w2", "a.txt", "two\n")]),
        text_step_no_usage("second"),
    ]);
    let mut run = run_two_turns(&repo, &mock).await;
    let a = run.current().0;
    repo.write("a.txt", "human\n");
    run.rewind(3, false, &["a.txt"]).await.done();
    assert_eq!(repo.read("a.txt").as_deref(), Some("one\n"));
    // Not while an engine runs on the work tree.
    let sb = LocalSandbox::new(repo.root()).unwrap();
    let busy =
        tautly_core::sessions::restore_backup(repo.home(), &sb, &a, &opts(true, true, &[])).await;
    assert!(busy.is_err());
    run.stop().await;
    (repo, a)
}

#[tokio::test]
async fn restore_backup_puts_back_what_the_log_kept() {
    // N6, N10: outside git, the bytes kept in `RestoreBegun` go back on request; after a
    // restore that finished only with --force; what it replaces is kept in turn.
    let (repo, a) = overwritten_outside_git().await;
    let refused = restore_backup(&repo, &a, &opts(false, false, &[])).await;
    assert!(
        matches!(
            refused,
            Err(tautly_core::sessions::BackupError::Finished(..))
        ),
        "{refused:?}"
    );
    let dry = restore_backup(&repo, &a, &opts(true, true, &[]))
        .await
        .unwrap();
    assert_eq!(dry.written, ["a.txt"]);
    assert_eq!(repo.read("a.txt").as_deref(), Some("one\n"));
    let done = restore_backup(&repo, &a, &opts(false, true, &[]))
        .await
        .unwrap();
    assert_eq!(done.written, ["a.txt"]);
    assert_eq!(repo.read("a.txt").as_deref(), Some("human\n"));
    assert!(!done.own);
    // And it can be put back in turn, when asked: the content it replaced was kept.
    let again = restore_backup(&repo, &a, &own_opts(true, &[]))
        .await
        .unwrap();
    assert!(again.own);
    assert_eq!(again.written, ["a.txt"]);
    assert_eq!(repo.read("a.txt").as_deref(), Some("one\n"));
}

#[tokio::test]
async fn restore_backup_run_again_does_not_reverse_itself() {
    // Task 49 M-R3b: without --include-own, its own backups are passed over, so a run
    // again (here after the first was cut off before its end) finishes the rewind's
    // backup instead of undoing what the first run wrote.
    let (repo, a) = overwritten_outside_git().await;
    let done = restore_backup(&repo, &a, &opts(false, true, &[]))
        .await
        .unwrap();
    assert_eq!(done.written, ["a.txt"]);
    assert_eq!(repo.read("a.txt").as_deref(), Some("human\n"));
    // Cut off: its `RestoreEnded` is lost.
    let log = logs(repo.home())
        .into_iter()
        .find(|p| p.to_string_lossy().contains(&a))
        .unwrap();
    let text = std::fs::read_to_string(&log).unwrap();
    let kept: Vec<&str> = text.lines().collect();
    assert!(kept.last().unwrap().contains("restore_ended"));
    std::fs::write(&log, kept[..kept.len() - 1].join("\n") + "\n").unwrap();
    let again = restore_backup(&repo, &a, &opts(false, true, &[]))
        .await
        .unwrap();
    assert!(again.skipped_own && !again.own, "{again:?}");
    assert!(again.written.is_empty(), "{again:?}");
    assert_eq!(repo.read("a.txt").as_deref(), Some("human\n"));
}

#[tokio::test]
async fn restore_backup_counts_a_mode_change_as_a_conflict() {
    // Task 49 M-R3c: a human `chmod` alone is a change too.
    use std::os::unix::fs::PermissionsExt;
    let (repo, a) = overwritten_outside_git().await;
    let path = repo.root().join("a.txt");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    let done = restore_backup(&repo, &a, &opts(false, true, &[]))
        .await
        .unwrap();
    assert_eq!(done.conflicts, ["a.txt"], "{done:?}");
    assert!(done.written.is_empty());
    assert_eq!(repo.read("a.txt").as_deref(), Some("one\n"));
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o755);
}

#[tokio::test]
async fn restore_backup_counts_a_mode_change_of_a_file_the_restore_created() {
    // Task 50 M6: outside git, a rewind recreates n.txt a human removed; the human then
    // makes it executable. Putting the backup back (removing it) would lose that.
    use std::os::unix::fs::PermissionsExt;
    let repo = Repo::plain(A);
    let mock = MockProvider::new(vec![
        tools_step(vec![write("w1", "n.txt", "one\n")]),
        text_step_no_usage("first"),
        tools_step(vec![write("w2", "n.txt", "two\n")]),
        text_step_no_usage("second"),
    ]);
    let mut run = run_two_turns(&repo, &mock).await;
    let a = run.current().0;
    std::fs::remove_file(repo.root().join("n.txt")).unwrap();
    run.rewind(3, false, &["n.txt"]).await.done();
    assert_eq!(repo.read("n.txt").as_deref(), Some("one\n"));
    run.stop().await;
    let path = repo.root().join("n.txt");
    // N4: created with the default mode (the umask respected).
    let default = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o666))
        .tempfile()
        .unwrap();
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o7777;
    assert_eq!(mode(&path), mode(default.path()));
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    let done = restore_backup(&repo, &a, &opts(false, true, &[]))
        .await
        .unwrap();
    assert_eq!(done.conflicts, ["n.txt"], "{done:?}");
    assert!(done.removed.is_empty());
    assert!(path.exists());
}

#[tokio::test]
async fn restore_backup_never_overwrites_a_later_human_edit() {
    // N10 (review probe p27): the human edits after the restore finished.
    let (repo, a) = overwritten_outside_git().await;
    repo.write("a.txt", "precious\n");
    let done = restore_backup(&repo, &a, &opts(false, true, &[]))
        .await
        .unwrap();
    assert_eq!(done.conflicts, ["a.txt"]);
    assert!(done.written.is_empty());
    assert_eq!(repo.read("a.txt").as_deref(), Some("precious\n"));
    // Named, it is put back, and what it replaced is kept.
    let done = restore_backup(&repo, &a, &opts(false, true, &["a.txt"]))
        .await
        .unwrap();
    assert_eq!(done.written, ["a.txt"]);
    assert_eq!(repo.read("a.txt").as_deref(), Some("human\n"));
    let back = restore_backup(&repo, &a, &own_opts(true, &[]))
        .await
        .unwrap();
    assert_eq!(back.written, ["a.txt"]);
    assert_eq!(repo.read("a.txt").as_deref(), Some("precious\n"));
}

#[tokio::test]
async fn restore_backup_never_removes_a_file_a_human_changed() {
    // N10 (review probe p27b): a switch recreated n.txt; a human then edited it.
    let repo = Repo::plain(A);
    let mock = MockProvider::new(vec![
        tools_step(vec![write("w1", "a.txt", "one\n")]),
        text_step_no_usage("first"),
        tools_step(vec![write("w2", "n.txt", "new\n")]),
        text_step_no_usage("second"),
    ]);
    let mut run = run_two_turns(&repo, &mock).await;
    let a = run.current().0;
    run.rewind(3, false, &[]).await.done();
    assert_eq!(repo.read("n.txt"), None);
    run.switch(&a, &[]).await.done();
    assert_eq!(repo.read("n.txt").as_deref(), Some("new\n"));
    run.stop().await;
    repo.write("n.txt", "edited\n");
    let done = restore_backup(&repo, &a, &opts(false, true, &[]))
        .await
        .unwrap();
    assert_eq!(done.conflicts, ["n.txt"], "{done:?}");
    assert!(done.removed.is_empty());
    assert_eq!(repo.read("n.txt").as_deref(), Some("edited\n"));
}

#[tokio::test]
async fn shutdown_during_the_lease_wait_returns_at_once() {
    // N12: the wait is polled in short slices and stops on Shutdown.
    let repo = Repo::git_repo(AB);
    let lease = tautly_core::Lease::shared(repo.home(), &repo.root()).unwrap();
    let held = lease.try_exclusive().unwrap();
    let run = start(EngineBuilder {
        lease_wait: Duration::from_secs(30),
        ..repo.builder(&MockProvider::new(vec![]))
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    let t0 = std::time::Instant::now();
    run.stop().await;
    assert!(t0.elapsed() < Duration::from_secs(3), "{:?}", t0.elapsed());
    drop(held);
}

// ------------------------------------------- cp5 review C I1: one step numbering

#[derive(Debug, Clone, Copy)]
enum NoResponse {
    Cancel,
    Error,
    Crash,
}

/// Turn one is answered (step 1 its message, 2 its text); turn two's request (logged as
/// step 4) gets no response, `how`. The run continues on that session; its next answer is
/// `third`.
async fn unanswered(repo: &Repo, how: NoResponse) -> Run {
    let second = match how {
        NoResponse::Error => MockTurn::Error(tautly_provider::ProviderError::InvalidRequest(
            "refused".into(),
        )),
        NoResponse::Cancel | NoResponse::Crash => MockTurn::Stall,
    };
    let mock = MockProvider::new(vec![
        text_step_no_usage("first"),
        second,
        text_step_no_usage("third"),
    ]);
    let mut run = start(repo.builder(&mock));
    let id = run.engine.session_id().to_string();
    run.turn_with("one").await;
    let log = run.current().1;
    run.engine
        .send(Command::SendMessage {
            id: None,
            text: "two".into(),
        })
        .await
        .unwrap();
    if !matches!(how, NoResponse::Error) {
        // The request stalls once sent.
        run.until(|e| matches!(e, Event::TurnStarted)).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        run.engine.send(Command::Cancel).await.unwrap();
    }
    run.until(|e| matches!(e, Event::TurnFinished { .. })).await;
    if !matches!(how, NoResponse::Crash) {
        return run;
    }
    // A crash: the log ends on the request.
    run.stop().await;
    let lines = raw_lines(&log);
    let request = lines
        .iter()
        .rposition(|l| l.contains(r#""type":"request""#))
        .unwrap();
    std::fs::write(&log, lines[..=request].join("\n") + "\n").unwrap();
    start(EngineBuilder {
        resume: Some(id),
        ..repo.builder(&MockProvider::new(vec![text_step_no_usage("third")]))
    })
}

/// Fork the current session at `at`: the branch's id, or the error.
async fn fork_at(run: &mut Run, at: u32) -> Result<String, String> {
    run.engine
        .send(Command::Fork {
            id: None,
            at: Some(at),
            label: None,
        })
        .await
        .unwrap();
    let seen = run
        .until(|e| matches!(e, Event::SessionChanged { .. } | Event::Error { .. }))
        .await;
    match seen.last().unwrap() {
        Event::SessionChanged { session_id, .. } => Ok(session_id.clone()),
        Event::Error { message, .. } => Err(message.clone()),
        _ => unreachable!(),
    }
}

fn user_texts(rebuilt: &tautly_core::resume::Rebuilt) -> Vec<String> {
    rebuilt
        .context
        .history
        .iter()
        .flat_map(|t| &t.messages)
        .filter(|m| m.role == tautly_provider::Role::User)
        .flat_map(|m| &m.content)
        .filter_map(|c| match c {
            tautly_provider::Content::Text(t) => Some(t.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_request_with_no_response_shares_its_number_with_the_next_step() {
    for how in [NoResponse::Cancel, NoResponse::Error, NoResponse::Crash] {
        let repo = Repo::git_repo(AB);
        let mut run = unanswered(&repo, how).await;
        let (id, log) = run.current();
        // The tip every surface shows is the one rewind and fork cut at.
        let tip = step_at_offset(&reader(&log), u64::MAX);
        assert_eq!(tip, 3, "{how:?}");
        let rebuilt = rebuild(&reader(&log), Some(tip)).unwrap();
        assert_eq!(user_texts(&rebuilt), ["one", "two"], "{how:?}");
        assert!(rebuild(&reader(&log), Some(tip + 1)).is_err(), "{how:?}");
        let nodes = tree::build(repo.home(), &repo.project(), None).unwrap();
        let node = nodes.iter().find(|n| n.session_id == id).unwrap();
        assert_eq!(node.last_step, tip, "{how:?}");
        assert!(state_at(&reader(&log), tip).is_ok(), "{how:?}");

        // The shown tip rewinds and forks.
        let (branch, _) = run.rewind(tip, false, &[]).await.done();
        assert!(!branch.is_empty(), "{how:?}");
        let b = reader(&run.current().1);
        let (_, offset) = b.header().parent.clone().unwrap();
        assert_eq!(step_at_offset(&b, offset), tip, "{how:?}");
        let forked = fork_at(&mut run, tip).await;
        assert!(forked.is_ok(), "{how:?}: {forked:?}");

        // The next message is step 4 on both numberings.
        run.turn_with("three").await;
        let log = run.current().1;
        let r = reader(&log);
        assert_eq!(step_at_offset(&r, u64::MAX), 5, "{how:?}");
        assert_eq!(
            user_texts(&rebuild(&r, Some(4)).unwrap()),
            ["one", "two", "three"],
            "{how:?}"
        );
        assert_eq!(
            user_texts(&rebuild(&r, Some(3)).unwrap()),
            ["one", "two"],
            "{how:?}"
        );
        run.stop().await;
    }
}

#[tokio::test]
async fn a_request_with_no_response_mid_log_keeps_every_boundary_in_step() {
    // Turn two's request is cancelled; turn three is answered. Every boundary rebuilds and
    // restores at the step the counters agree on.
    let repo = Repo::git_repo(AB);
    let mut run = unanswered(&repo, NoResponse::Cancel).await;
    run.turn_with("three").await;
    let log = run.current().1;
    run.stop().await;
    let r = reader(&log);
    let mut at_start = BTreeMap::new();
    for rec in r.records() {
        let (at, rec) = rec.unwrap();
        if let SessionRecord::TurnStart { text, .. } = rec {
            at_start.insert(text, step_at_offset(&r, at));
        }
    }
    assert_eq!(at_start["two"], 3);
    assert_eq!(at_start["three"], 4);
    assert_eq!(step_at_offset(&r, u64::MAX), 5);
    for at in 0..=5 {
        let s = state_at(&r, at);
        assert!(s.is_ok(), "state_at({at}): {s:?}");
        let texts = user_texts(&rebuild(&r, Some(at)).unwrap());
        let expected: &[&str] = match at {
            0 => &[],
            1 | 2 => &["one"],
            3 => &["one", "two"],
            _ => &["one", "two", "three"],
        };
        assert_eq!(texts, expected, "rebuild({at})");
    }
}

// ------------------------------------------- cp5 review C I2: unreadable payloads

#[tokio::test]
async fn a_log_holding_a_null_replay_speed_resumes_forks_rewinds_and_compares() {
    let repo = Repo::git_repo(AB);
    let mock = MockProvider::new(vec![
        tools_step(vec![write("w1", "a.txt", "one\n")]),
        text_step_no_usage("first"),
    ]);
    let mut run = start(repo.builder(&mock));
    let id = run.engine.session_id().to_string();
    run.turn_with("one").await;
    let log = run.current().1;
    run.stop().await;
    // What a build before the freeze logged for `replay {speed: 1e39}`.
    let mut line: Value = serde_json::from_str(raw_lines(&log).last().unwrap()).unwrap();
    line["data"] = json!({"type": "command", "data": {"type": "replay", "session_id": null,
        "speed": null, "from_step": null}});
    line["offset"] = json!(line["offset"].as_u64().unwrap() + 1);
    let mut text = std::fs::read_to_string(&log).unwrap();
    text.push_str(&format!("{line}\n"));
    std::fs::write(&log, text).unwrap();

    let mut run = start(EngineBuilder {
        resume: Some(id),
        ..repo.builder(&MockProvider::new(vec![
            tools_step(vec![write("w2", "a.txt", "two\n")]),
            text_step_no_usage("second"),
        ]))
    });
    run.engine
        .send(Command::SendMessage {
            id: None,
            text: "two".into(),
        })
        .await
        .unwrap();
    let evs = run.until(|e| matches!(e, Event::TurnFinished { .. })).await;
    assert_eq!(repo.read("a.txt").as_deref(), Some("two\n"), "{evs:#?}");
    let (_, report) = run.rewind(3, false, &[]).await.done();
    assert!(report.applied, "{report:?}");
    assert_eq!(repo.read("a.txt").as_deref(), Some("one\n"));
    let forked = fork_at(&mut run, 2).await;
    assert!(forked.is_ok(), "{forked:?}");
    run.stop().await;
    let s =
        tautly_core::compare::summarize_at(&reader(&log), None, &Catalog::default(), None).unwrap();
    assert_eq!(s.user_turns, 2);
}

// ------------------------------------------- cp5 review C M2: synthetic turn ends

/// Each record line's `(type, t, data)`.
fn typed_lines(path: &Path) -> Vec<(String, u64, Value)> {
    raw_lines(path)[1..]
        .iter()
        .map(|l| {
            let v: Value = serde_json::from_str(l).unwrap();
            let ty = v["data"]["type"].as_str().unwrap().to_string();
            (ty, v["t"].as_u64().unwrap(), v["data"]["data"].clone())
        })
        .collect()
}

#[tokio::test]
async fn synthetic_turn_ends_are_marked_and_stamped_at_the_last_record() {
    let repo = Repo::git_repo(AB);
    let mut run = unanswered(&repo, NoResponse::Crash).await;
    let log = run.current().1;
    // Answered once the resume (which closes the crashed turn) is done.
    run.rewind(3, true, &[]).await.done();
    let lines = typed_lines(&log);
    let end = lines
        .iter()
        .position(|(ty, _, d)| ty == "turn_end" && d["synthetic"] == true)
        .expect("the crashed turn's end");
    assert_eq!(lines[end].2["reason"], "error");
    assert_eq!(lines[end].0, "turn_end");
    assert_eq!(lines[end - 1].0, "request", "{lines:?}");
    assert_eq!(lines[end].1, lines[end - 1].1, "stamped at the last record");
    // Real turn ends are not marked.
    assert!(
        lines[..end]
            .iter()
            .any(|(ty, _, d)| ty == "turn_end" && d.get("synthetic").is_none())
    );

    // A branch cut inside a turn: `Forked` and the closing end, at the copy's last time.
    run.rewind(3, false, &[]).await.done();
    let branch = typed_lines(&run.current().1);
    let forked = branch.iter().position(|(ty, _, _)| ty == "forked").unwrap();
    let (ty, t, d) = &branch[forked + 1];
    assert_eq!(
        (ty.as_str(), &d["reason"], &d["synthetic"]),
        ("turn_end", &json!("cancelled"), &json!(true))
    );
    assert_eq!(*t, branch[forked - 1].1);
    assert_eq!(branch[forked].1, branch[forked - 1].1);
    run.stop().await;
}

// ------------------------------------- cp5 review C M7, M9: branch edges and headers

#[tokio::test]
async fn fork_at_zero_accepts_what_rewind_zero_accepts_and_headers_name_the_step() {
    let repo = Repo::git_repo(AB);
    let mock = two_turns();
    let mut run = run_two_turns(&repo, &mock).await;
    run.rewind(4, false, &[]).await.done();
    let b = reader(&run.current().1);
    assert_eq!(b.header().parent_step, Some(4));
    // From that branch, whose copy holds nothing kept before step 1 (review probe p9).
    let forked = fork_at(&mut run, 0).await;
    assert!(forked.is_ok(), "{forked:?}");
    let f = reader(&run.current().1);
    let (_, offset) = f.header().parent.clone().unwrap();
    assert_eq!(f.header().parent_step, Some(0));
    assert_eq!(step_at_offset(&f, offset), 0);
    // A fork at the tip names it.
    run.turn_with("again").await;
    run.engine
        .send(Command::Fork {
            id: None,
            at: None,
            label: None,
        })
        .await
        .unwrap();
    run.until(|e| matches!(e, Event::SessionChanged { .. }))
        .await;
    let tip = reader(&run.current().1);
    let (parent, offset) = tip.header().parent.clone().unwrap();
    let parent_log = logs(repo.home())
        .into_iter()
        .find(|p| reader(p).header().session_id == parent)
        .unwrap();
    let parent_tip = step_at_offset(&reader(&parent_log), u64::MAX);
    assert_eq!(tip.header().parent_step, Some(parent_tip));
    assert_eq!(step_at_offset(&tip, offset), parent_tip);
    let nodes = tree::build(repo.home(), &repo.project(), None).unwrap();
    let node = nodes
        .iter()
        .find(|n| n.session_id == tip.header().session_id)
        .unwrap();
    assert_eq!(node.parent, Some((parent, parent_tip)));
    run.stop().await;
}

// ------------------------------------------------ cp5 review C M6: large tool inputs

#[tokio::test]
async fn a_large_tool_input_is_one_blob_both_records_name_and_every_reader_resolves() {
    let repo = Repo::git_repo(AB);
    let big = "x".repeat(10_000) + "\n";
    let small = "small\n";
    let mock = MockProvider::new(vec![
        tools_step(vec![
            write("w1", "big.txt", &big),
            write("w2", "s.txt", small),
        ]),
        text_step_no_usage("done"),
    ]);
    let mut run = start(repo.builder(&mock));
    run.turn_with("write").await;
    let log = run.current().1;
    let lines = typed_lines(&log);
    let event = lines
        .iter()
        .find(|(ty, _, d)| ty == "provider" && d["tool_call"]["id"] == "w1")
        .map(|(_, _, d)| d["tool_call"].clone())
        .unwrap();
    let call = lines
        .iter()
        .find(|(ty, _, d)| ty == "tool_call" && d["id"] == "w1")
        .map(|(_, _, d)| d.clone())
        .unwrap();
    // One blob: the exact input text, named by both.
    let blob = event["input_blob"]
        .as_str()
        .expect("the event names a blob");
    assert_eq!(event["input_json"], "");
    assert_eq!(call["args_blob"], blob);
    assert_eq!(call["args"], Value::Null);
    let r = reader(&log);
    let bytes = r
        .read_blob(&tautly_session::BlobRef::parse(blob).unwrap())
        .unwrap();
    let input = serde_json::to_string(&json!({ "path": "big.txt", "content": big })).unwrap();
    assert_eq!(bytes, input.as_bytes());
    // A small input stays inline.
    let small_event = lines
        .iter()
        .find(|(ty, _, d)| ty == "provider" && d["tool_call"]["id"] == "w2")
        .map(|(_, _, d)| d["tool_call"].clone())
        .unwrap();
    assert!(small_event.get("input_blob").is_none());
    assert!(
        small_event["input_json"]
            .as_str()
            .unwrap()
            .contains("s.txt")
    );

    // Rebuild resolves it: the history holds the call's input.
    let rebuilt = rebuild(&r, None).unwrap();
    let used = rebuilt
        .context
        .history
        .iter()
        .flat_map(|t| &t.messages)
        .flat_map(|m| &m.content)
        .find_map(|c| match c {
            tautly_provider::Content::ToolUse { id, input, .. } if id == "w1" => {
                Some(input.clone())
            }
            _ => None,
        })
        .unwrap();
    assert_eq!(used["content"], big);
    // Replay resolves it.
    let proposed = tautly_core::replay::events(&r, None)
        .find_map(|(_, _, ev)| match ev {
            Event::ToolCallProposed { call_id, args, .. } if call_id == "w1" => Some(args),
            _ => None,
        })
        .unwrap();
    assert_eq!(proposed["content"], big);
    // A branch shares the blob rather than copying the input.
    run.rewind(3, false, &[]).await.done();
    let branch = std::fs::read_to_string(run.current().1).unwrap();
    assert!(!branch.contains(&big[..5000]));
    assert!(branch.contains(blob));
    run.stop().await;
}

// ------------------------------------------- cp5 FMT re-review N1: unreadable records

/// `log` with the checkpoint whose `point` is `which` given a newer `point` shape this
/// version cannot read; the rewritten log's path.
fn with_unreadable_checkpoint(log: &Path, which: &str) -> PathBuf {
    let text = std::fs::read_to_string(log).unwrap();
    let mut lines = text.lines();
    let mut out = vec![lines.next().unwrap().to_string()];
    for l in lines {
        let mut v: Value = serde_json::from_str(l).unwrap();
        if v["data"]["type"] == "checkpoint" && v["data"]["data"]["point"] == which {
            v["data"]["data"]["point"] = json!({ "from_the_future": which });
        }
        out.push(v.to_string());
    }
    let rewritten = log.with_extension(format!("{which}-unreadable.jsonl"));
    std::fs::write(&rewritten, out.join("\n") + "\n").unwrap();
    rewritten
}

#[tokio::test]
async fn a_record_this_version_cannot_read_makes_the_states_across_it_partial() {
    // Steps: 1 user, 2 write a=one, 3 a command that creates c.txt (after its before_exec
    // checkpoint), 4 text.
    let repo = Repo::git_repo(AB);
    let mock = MockProvider::new(vec![
        tools_step(vec![write("w1", "a.txt", "one\n")]),
        tools_step(vec![bash("b1", "printf 'x\\n' > c.txt")]),
        text_step_no_usage("done"),
    ]);
    let mut run = start(repo.builder(&mock));
    run.turn_with("go").await;
    let log = run.current().1;
    run.stop().await;
    let exact = state_at(&reader(&log), 2).unwrap();
    assert_eq!(exact.partial, None, "{exact:?}");

    // The reviewer's probe: the before_exec checkpoint reads as Unreadable; the next
    // checkpoint (after the command) must not be taken as exact for step 2.
    let probe = with_unreadable_checkpoint(&log, "before_exec");
    let r = reader(&probe);
    assert!(
        records(&probe)
            .iter()
            .any(|rec| matches!(rec, SessionRecord::Unreadable))
    );
    let s = state_at(&r, 2).unwrap();
    assert_ne!(s.base, exact.base);
    assert!(
        s.partial
            .as_deref()
            .is_some_and(|p| p.contains("newer version")),
        "{s:?}"
    );

    // After the expected tip's base: the tip is partial too.
    let probe = with_unreadable_checkpoint(&log, "turn_end");
    let tip = expected_tip(&reader(&probe)).unwrap();
    assert!(
        tip.partial
            .as_deref()
            .is_some_and(|p| p.contains("newer version")),
        "{tip:?}"
    );
    assert!(expected_tip(&reader(&log)).unwrap().partial.is_none());
}

#[tokio::test]
async fn a_switch_closes_the_targets_cut_turn_before_it_writes_there() {
    // cp5 FMT re-review N3: the synthetic end is stamped at the target's last record and
    // precedes the switch's own records, so compare's wall time leaves the downtime out.
    let repo = Repo::git_repo(AB);
    let lived = Instant::now();
    let mut run = start(repo.builder(&MockProvider::new(vec![text_step_no_usage("one")])));
    let target = run.engine.session_id().to_string();
    run.turn_with("one").await;
    let log = run.current().1;
    run.stop().await;
    // However slow the runner, the target's wall time cannot exceed its own lifetime.
    let lived = lived.elapsed().as_millis() as u64;
    // A crash inside the next turn: the log ends on its `TurnStart`.
    let mut line: Value = serde_json::from_str(raw_lines(&log).last().unwrap()).unwrap();
    line["offset"] = json!(line["offset"].as_u64().unwrap() + 1);
    line["data"] = json!({"type": "turn_start", "data": {"turn": 2, "text": "two"}});
    let mut text = std::fs::read_to_string(&log).unwrap();
    text.push_str(&format!("{line}\n"));
    std::fs::write(&log, text).unwrap();
    let start_t = line["t"].as_u64().unwrap();

    let mut run = start(repo.builder(&MockProvider::new(vec![text_step_no_usage("other")])));
    run.turn_with("elsewhere").await;
    // Time no session of the target ran.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    run.switch(&target, &[]).await.done();
    run.stop().await;
    let lines = typed_lines(&log);
    let started = lines
        .iter()
        .rposition(|(ty, _, d)| ty == "turn_start" && d["turn"] == 2)
        .unwrap();
    let (ty, t, d) = &lines[started + 1];
    assert_eq!(ty, "turn_end", "{lines:?}");
    assert_eq!(d["synthetic"], true);
    assert_eq!(*t, start_t);
    let synthetic = lines
        .iter()
        .filter(|(ty, _, d)| ty == "turn_end" && d["synthetic"] == true)
        .count();
    assert_eq!(synthetic, 1);
    let s =
        tautly_core::compare::summarize_at(&reader(&log), None, &Catalog::default(), None).unwrap();
    // The downtime (1.5 s and the other session) would add at least 1,500 ms.
    assert!(s.wall_ms <= lived, "{} > {lived}", s.wall_ms);
}

// ------------------------------------------- quality Q1: failed restores, links, dirs

/// A sandbox whose next write to `c.txt` fails once armed (once: the rollback's own write
/// of it then goes through).
struct FailC {
    sb: LocalSandbox,
    armed: Arc<std::sync::atomic::AtomicBool>,
}

#[async_trait::async_trait]
impl Delegate for FailC {
    fn inner(&self) -> &LocalSandbox {
        &self.sb
    }
    async fn write_atomic(
        &self,
        p: &Path,
        bytes: &[u8],
        mode: Option<u32>,
    ) -> Result<(), tautly_sandbox::SandboxError> {
        if p.file_name().is_some_and(|n| n == "c.txt")
            && self.armed.swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(tautly_sandbox::SandboxError::Denied("disk full".into()));
        }
        self.sb.write_atomic(p, bytes, mode).await
    }
    async fn spill_stream(
        &self,
    ) -> Result<Option<(PathBuf, tautly_sandbox::SpillWriter)>, tautly_sandbox::SandboxError> {
        self.sb.spill_stream().await
    }
}

/// The outcomes of the `RestoreEnded` records of `log`, each after its `RestoreBegun`.
fn restore_outcomes(log: &Path) -> Vec<tautly_core::record::RestoreOutcome> {
    let mut out = Vec::new();
    let mut open = false;
    for rec in records(log) {
        match rec {
            SessionRecord::RestoreBegun { .. } => {
                assert!(!open, "a RestoreBegun before the last one ended");
                open = true;
            }
            SessionRecord::RestoreEnded { outcome } => {
                assert!(open, "a RestoreEnded without its RestoreBegun");
                open = false;
                out.push(outcome);
            }
            _ => {}
        }
    }
    out
}

#[tokio::test]
async fn a_restore_that_fails_midway_puts_back_what_it_can_and_logs_how_it_ended() {
    use tautly_core::record::RestoreOutcome;
    let repo = Repo::plain(A);
    let mock = MockProvider::new(vec![
        tools_step(vec![
            write("w1", "a.txt", "one\n"),
            write("w2", "b.txt", "one\n"),
            write("w3", "c.txt", "one\n"),
        ]),
        text_step_no_usage("first"),
        tools_step(vec![
            write("w4", "a.txt", "two\n"),
            write("w5", "b.txt", "two\n"),
            write("w6", "c.txt", "two\n"),
        ]),
        text_step_no_usage("second"),
    ]);
    let armed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let sb: Arc<dyn Sandbox> = Arc::new(Delegating(FailC {
        sb: LocalSandbox::new(repo.root()).unwrap(),
        armed: Arc::clone(&armed),
    }));
    let mut run = start(EngineBuilder {
        sandbox: Some(sb),
        ..repo.builder(&mock)
    });
    run.turn_with("one").await;
    run.turn_with("two").await;
    let log = run.current().1;

    // The third write fails: the two before it are put back.
    armed.store(true, std::sync::atomic::Ordering::SeqCst);
    let err = run.rewind(3, false, &[]).await.error();
    assert!(err.contains("disk full"), "{err}");
    assert!(err.contains("were put back as they were"), "{err}");
    for f in ["a.txt", "b.txt", "c.txt"] {
        assert_eq!(repo.read(f).as_deref(), Some("two\n"), "{f}");
    }
    assert_eq!(restore_outcomes(&log), [RestoreOutcome::RolledBack]);

    // Again, with a link and a file over the read limit among the originals: those two are
    // not kept, so they cannot be put back, and the error names them; the file that was
    // kept still goes back byte for byte.
    let b = repo.root().join("b.txt");
    std::fs::remove_file(&b).unwrap();
    std::os::unix::fs::symlink("a.txt", &b).unwrap();
    let big = vec![b'x'; 64 * 1024 * 1024 + 1];
    std::fs::write(repo.root().join("c.txt"), &big).unwrap();
    armed.store(true, std::sync::atomic::Ordering::SeqCst);
    let err = run.rewind(3, false, &["b.txt", "c.txt"]).await.error();
    assert!(err.contains("could NOT all be put back"), "{err}");
    assert!(
        err.contains("not kept, so not put back: b.txt, c.txt"),
        "{err}"
    );
    assert_eq!(repo.read("a.txt").as_deref(), Some("two\n"));
    assert_eq!(
        std::fs::read(repo.root().join("c.txt")).unwrap().len(),
        big.len()
    );
    assert_eq!(
        restore_outcomes(&log),
        [RestoreOutcome::RolledBack, RestoreOutcome::Failed]
    );
    // Still on the session it started from.
    assert_eq!(run.current().1, log);
    run.stop().await;
}

#[tokio::test]
async fn a_restore_never_writes_through_a_link_or_over_a_directory() {
    let repo = Repo::plain(A);
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("x.txt"), "outside\n").unwrap();
    let mock = MockProvider::new(vec![
        tools_step(vec![
            write("w1", "sub/x.txt", "one\n"),
            write("w2", "d.txt", "one\n"),
        ]),
        text_step_no_usage("first"),
        tools_step(vec![
            write("w3", "sub/x.txt", "two\n"),
            write("w4", "d.txt", "two\n"),
        ]),
        text_step_no_usage("second"),
    ]);
    let mut run = run_two_turns(&repo, &mock).await;
    let log = run.current().1;
    // A repository can plant `sub -> /elsewhere`; a directory can take a file's place.
    std::fs::remove_dir_all(repo.root().join("sub")).unwrap();
    std::os::unix::fs::symlink(outside.path(), repo.root().join("sub")).unwrap();
    std::fs::remove_file(repo.root().join("d.txt")).unwrap();
    std::fs::create_dir(repo.root().join("d.txt")).unwrap();

    let (_branch, report) = run.rewind(3, false, &["sub/x.txt", "d.txt"]).await.done();
    assert!(!report.applied, "{report:?}");
    assert_eq!(report.blocked, ["d.txt", "sub/x.txt"]);
    let note = report.note.unwrap_or_default();
    assert!(
        note.contains("blocked, a directory is in the way: d.txt"),
        "{note}"
    );
    assert!(
        note.contains("blocked, a symbolic link or a file is above it"),
        "{note}"
    );
    assert_eq!(
        std::fs::read_to_string(outside.path().join("x.txt")).unwrap(),
        "outside\n"
    );
    assert!(repo.root().join("d.txt").is_dir());
    // Nothing was written, so no branch was made and no restore was logged.
    assert_eq!(run.current().1, log);
    assert!(restore_outcomes(&log).is_empty());
    run.stop().await;
}

#[test]
fn a_tool_edit_whose_result_was_not_captured_is_named_in_the_tip() {
    let repo = Repo::plain(A);
    let mut w = tautly_session::SessionWriter::<SessionRecord>::create_in(
        repo.home(),
        &repo.root(),
        tautly_session::HeaderInit::default(),
    )
    .unwrap();
    let path = |p: &str| tautly_session::RelPath::from_abs(&repo.root(), &repo.root().join(p));
    let kept = w.blob(b"kept\n").unwrap();
    // big.bin existed before (a pre-image) and has no post-image: not captured.
    for (p, post) in [("a.txt", Some(kept.clone())), ("big.bin", None)] {
        w.append(&SessionRecord::Edit {
            path: path(p).unwrap(),
            pre_image: Some(kept.clone()),
            post_image_blob: post,
            step: 2,
            asserted: false,
        })
        .unwrap();
    }
    w.checkpoint().unwrap();
    let tip = expected_tip(&reader(w.path())).unwrap();
    assert_eq!(
        tip.partial.as_deref(),
        Some("the content a tool left in big.bin was not captured")
    );
    let paths: Vec<&str> = tip.overlay.iter().map(|(p, _)| p.as_str()).collect();
    assert_eq!(paths, ["a.txt"]);
}
