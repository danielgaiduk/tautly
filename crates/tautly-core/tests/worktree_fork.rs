//! Fork at a step into an isolated git worktree (spec 7.12.4, Task 50): the worktree holds
//! `state_at(N)` while the original work tree and its index stay as they were, the fork's
//! log copies the records up to step N, git runs no hook, and what cannot be forked
//! exactly is refused. Against real git repositories with scripted providers.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command as Process;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tautly_core::timetravel::step_at_offset;
use tautly_core::worktree::{self, ForkRequest, WtError};
use tautly_core::{Command, Config, EngineBuilder, Event, ProviderFactory, SessionRecord};
use tautly_provider::{
    Catalog, MockProvider, MockTurn, ModelId, Provider, ProviderEvent, StopReason,
};
use tautly_sandbox::{LocalSandbox, Mode, Sandbox};
use tautly_session::{Origin, SessionReader};
use tempfile::TempDir;
use tokio::sync::mpsc;

const MODEL: &str = "mock/test";
const OTHER: &str = "anthropic/other";
const WAIT: Duration = Duration::from_secs(30);

// ------------------------------------------------------------------------ fixtures

struct Repo {
    dir: TempDir,
    home: TempDir,
}

impl Repo {
    /// A git repository with `a.txt` and `b.txt` committed.
    fn git_repo() -> Repo {
        let repo = Repo::plain();
        repo.git(&["init", "-q", "-b", "main"]);
        repo.write("a.txt", "zero\n");
        repo.write("b.txt", "bee\n");
        repo.git(&["add", "-A"]);
        repo.git(&["commit", "-q", "-m", "init"]);
        repo
    }

    /// A directory that is not a git work tree, with `a.txt`.
    fn plain() -> Repo {
        let repo = Repo {
            dir: tempfile::tempdir().unwrap(),
            home: tempfile::tempdir().unwrap(),
        };
        repo.write("a.txt", "zero\n");
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

    fn sandbox(&self) -> Arc<dyn Sandbox> {
        Arc::new(LocalSandbox::new(self.root()).unwrap())
    }

    fn builder(&self, mock: &MockProvider) -> EngineBuilder {
        EngineBuilder {
            provider: Some(Arc::new(mock.clone())),
            provider_factory: Some(factory(mock)),
            data_home: Some(self.home()),
            ..EngineBuilder::new(config(), self.dir.path())
        }
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
    let entry = |id: &str| {
        format!(
            r#"[models."{id}"]
context_window = 200000
max_output = 8000
parallel_tools = true
reasoning = "none"
cache = "none"
images = false
edit_format = "str_replace"
"#
        )
    };
    let catalog = Catalog::from_toml(&(entry(MODEL) + &entry(OTHER))).unwrap();
    let mut cfg = Config {
        model: ModelId::from(MODEL),
        catalog_overrides: Some(catalog),
        ..Config::default()
    };
    cfg.permissions.mode = Mode::Yolo;
    cfg
}

fn factory(mock: &MockProvider) -> ProviderFactory {
    let mock = mock.clone();
    Arc::new(move |_: &ModelId| Ok(Arc::new(mock.clone()) as Arc<dyn Provider>))
}

/// Every file of the tree at `root` (outside `.git` and `.tautly`) and its content.
fn files(root: &Path) -> BTreeMap<String, String> {
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

fn text_step(text: &str) -> MockTurn {
    MockTurn::Events(vec![
        ProviderEvent::TextDelta(text.into()),
        ProviderEvent::Done {
            stop: StopReason::EndTurn,
        },
    ])
}

/// Two turns: (1 user, 2 write a=one, 3 text) and (4 user, 5 write a=two and run a
/// command that changes b.txt and creates c.txt, 6 text).
fn two_turns() -> MockProvider {
    MockProvider::new(vec![
        tools_step(vec![write("w1", "a.txt", "one\n")]),
        text_step("first"),
        tools_step(vec![
            write("w2", "a.txt", "two\n"),
            bash("b1", "printf 'changed\\n' > b.txt && printf 'x\\n' > c.txt"),
        ]),
        text_step("second"),
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

/// A finished two-turn session in `repo`: its id and log.
async fn two_turn_session(repo: &Repo) -> (String, PathBuf) {
    let mut run = Run::start(repo.builder(&two_turns()));
    run.turn_with("one").await;
    run.turn_with("two").await;
    let (id, log) = run.engine.current_session();
    run.stop().await;
    (id, log)
}

fn reader(path: &Path) -> SessionReader<SessionRecord> {
    SessionReader::<SessionRecord>::open(path).unwrap()
}

fn records(path: &Path) -> Vec<SessionRecord> {
    reader(path).records().map(|r| r.unwrap().1).collect()
}

/// The `data` of each record line of the log, as written.
fn raw_records(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .filter(|v| v["type"] == "record")
        .map(|v| v["data"].clone())
        .collect()
}

async fn fork(repo: &Repo, log: &Path, at: u32) -> Result<worktree::WorktreeFork, WtError> {
    worktree::fork_to_worktree(
        repo.sandbox(),
        &repo.home(),
        log,
        ForkRequest {
            at: Some(at),
            ..ForkRequest::default()
        },
    )
    .await
}

/// Every session log of the data home.
fn logs(home: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(projects) = std::fs::read_dir(home.join("sessions")) else {
        return out;
    };
    for project in projects {
        for e in std::fs::read_dir(project.unwrap().path()).unwrap() {
            let p = e.unwrap().path();
            if p.extension().is_some_and(|x| x == "jsonl") {
                out.push(p);
            }
        }
    }
    out
}

// --------------------------------------------------------------------------- tests

#[tokio::test]
async fn fork_at_n_creates_worktree_with_state_at_n_and_original_tree_and_index_untouched() {
    let repo = Repo::git_repo();
    let (_, log) = two_turn_session(&repo).await;
    let before = files(&repo.root());
    let index = std::fs::read(repo.root().join(".git/index")).unwrap();
    let head = repo.git(&["rev-parse", "HEAD"]).unwrap();
    let status = repo.git(&["status", "--porcelain"]).unwrap();

    // Step 4 is the second user message: the next checkpoint (before step 5's command)
    // already has step 5's write of a.txt, which is reverted.
    let made = fork(&repo, &log, 4).await.unwrap();
    let wt = made.worktree.clone();
    assert!(wt.starts_with(repo.home().join("worktrees")), "{wt:?}");
    assert_eq!(made.at, 4);
    assert_eq!(made.note, None);
    let there = files(&wt);
    assert_eq!(there.get("a.txt").map(String::as_str), Some("one\n"));
    assert_eq!(there.get("b.txt").map(String::as_str), Some("bee\n"));
    assert!(!there.contains_key("c.txt"), "{there:?}");
    // The new worktree is clean: what it holds is its own HEAD.
    assert_eq!(git_in(&wt, &["status", "--porcelain"]).unwrap(), "");

    // The original work tree, its index, HEAD and status are as they were.
    assert_eq!(files(&repo.root()), before);
    assert_eq!(
        std::fs::read(repo.root().join(".git/index")).unwrap(),
        index
    );
    assert_eq!(repo.git(&["rev-parse", "HEAD"]).unwrap(), head);
    assert_eq!(repo.git(&["status", "--porcelain"]).unwrap(), status);

    // At the end of the first turn.
    let made = fork(&repo, &log, 3).await.unwrap();
    let there = files(&made.worktree);
    assert_eq!(there.get("a.txt").map(String::as_str), Some("one\n"));
    assert_ne!(made.worktree, wt);
}

#[tokio::test]
async fn fork_session_has_parent_origin_what_if_and_records_up_to_n() {
    let repo = Repo::git_repo();
    let (parent, log) = two_turn_session(&repo).await;
    let made = fork(&repo, &log, 3).await.unwrap();
    let child = reader(&made.log);
    let header = child.header().clone();
    assert_eq!(header.session_id, made.session_id);
    assert_eq!(header.origin, Origin::WhatIf);
    assert_eq!(header.root, made.worktree.canonicalize().unwrap());
    assert_eq!(header.project, reader(&log).header().project);
    let (from, offset) = header.parent.clone().unwrap();
    assert_eq!(from, parent);
    assert_eq!(step_at_offset(&reader(&log), offset), 3);
    // The records up to the step's last one, verbatim; then the fork's own.
    let copied = usize::try_from(offset).unwrap() + 1;
    let ours = raw_records(&made.log);
    assert_eq!(ours[..copied], raw_records(&log)[..copied]);
    assert_eq!(ours[copied]["type"], "forked", "{:?}", ours[copied]);
    // It is listed in the worktree (resumable there), not in the original.
    let here = tautly_session::list_here_in(&repo.home(), &made.worktree).unwrap();
    assert_eq!(here.len(), 1);
    assert_eq!(here[0].id, made.session_id);
    // The fork's checkpoints have refs of their own.
    let refs = repo
        .git(&["for-each-ref", "--format=%(refname)", "refs/tautly"])
        .unwrap();
    assert!(refs.contains(&made.session_id), "{refs}");
    // A resumed fork continues in its worktree with the copied context.
    let mut run = Run::start(EngineBuilder {
        resume: Some(made.session_id.clone()),
        ..EngineBuilder {
            root: made.worktree.clone(),
            ..repo.builder(&MockProvider::new(vec![text_step("again")]))
        }
    });
    run.turn_with("three").await;
    assert_eq!(run.engine.current_session().0, made.session_id);
    run.stop().await;
    let turns: Vec<u32> = records(&made.log)
        .iter()
        .filter_map(|r| match r {
            SessionRecord::TurnStart { turn, .. } => Some(*turn),
            _ => None,
        })
        .collect();
    assert_eq!(turns, [1, 2], "{turns:?}");
}

#[tokio::test]
async fn fork_shares_blob_dir_with_parent() {
    let repo = Repo::git_repo();
    let (_, log) = two_turn_session(&repo).await;
    let made = fork(&repo, &log, 3).await.unwrap();
    assert_eq!(made.log.parent(), log.parent());
    let child = reader(&made.log);
    let blob = records(&made.log)
        .into_iter()
        .find_map(|r| match r {
            SessionRecord::ToolResult { result_blob, .. } => Some(result_blob),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        child.read_blob(&blob).unwrap(),
        reader(&log).read_blob(&blob).unwrap()
    );
}

#[tokio::test]
async fn model_override_applied_before_first_message() {
    let repo = Repo::git_repo();
    let (_, log) = two_turn_session(&repo).await;
    let made = worktree::fork_to_worktree(
        repo.sandbox(),
        &repo.home(),
        &log,
        ForkRequest {
            at: Some(3),
            model: Some(ModelId::from(OTHER)),
            label: Some("try-other".into()),
            ..ForkRequest::default()
        },
    )
    .await
    .unwrap();
    let mock = MockProvider::new(vec![text_step("from the other")]);
    let mut run = Run::start(EngineBuilder {
        resume: Some(made.session_id.clone()),
        root: made.worktree.clone(),
        ..repo.builder(&mock)
    });
    run.turn_with("three").await;
    run.stop().await;
    let recs = records(&made.log);
    let forked = recs
        .iter()
        .position(|r| matches!(r, SessionRecord::Forked))
        .unwrap();
    let set = recs
        .iter()
        .rposition(|r| matches!(r, SessionRecord::ModelSet { model } if model.as_str() == OTHER))
        .expect("the model override is logged");
    let first_turn = recs
        .iter()
        .skip(forked)
        .position(|r| matches!(r, SessionRecord::TurnStart { .. }))
        .map(|i| i + forked)
        .unwrap();
    assert!(forked < set && set < first_turn, "{recs:?}");
    let request = recs[first_turn..]
        .iter()
        .find_map(|r| match r {
            SessionRecord::Request { model, .. } => Some(model.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(request.as_str(), OTHER);
    assert_eq!(
        tautly_core::record::session_label(&reader(&made.log))
            .unwrap()
            .as_deref(),
        Some("try-other")
    );
}

#[tokio::test]
async fn worktree_hooks_not_run() {
    let repo = Repo::git_repo();
    let (_, log) = two_turn_session(&repo).await;
    let marker = repo.home().join("hook-ran");
    let hook = format!("#!/bin/sh\necho ran > '{}'\n", marker.display());
    for dir in [".git/hooks", "hooks"] {
        let path = repo.root().join(dir).join("post-checkout");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &hook).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    // A hook path the repository configures is not followed either.
    repo.git(&["config", "core.hooksPath", "hooks"]).unwrap();
    // Git itself would run it (the hook works).
    let probe = repo.home().join("probe");
    repo.git(&[
        "worktree",
        "add",
        "--detach",
        probe.to_str().unwrap(),
        "HEAD",
    ])
    .unwrap();
    assert!(marker.exists(), "the test hook does not run");
    std::fs::remove_file(&marker).unwrap();

    let made = fork(&repo, &log, 3).await.unwrap();
    assert!(made.worktree.join("a.txt").exists());
    assert!(!marker.exists(), "a hook ran");
}

#[tokio::test]
async fn non_git_root_refuses() {
    let repo = Repo::plain();
    let (_, log) = two_turn_session(&repo).await;
    let refused = fork(&repo, &log, 3).await;
    assert!(matches!(refused, Err(WtError::NotGit(_))), "{refused:?}");
    assert_eq!(logs(&repo.home()).len(), 1, "no session was made");
    assert!(!repo.home().join("worktrees").exists());
}

#[tokio::test]
async fn partial_state_refused_with_note() {
    // A session that stopped mid-turn (its log cut after a command's result): no
    // checkpoint follows that step, so its files are not known exactly.
    let repo = Repo::git_repo();
    let mock = MockProvider::new(vec![
        tools_step(vec![bash("b1", "printf 'cmd\\n' > c.txt")]),
        text_step("done"),
    ]);
    let mut run = Run::start(repo.builder(&mock));
    run.turn_with("go").await;
    let log = run.engine.current_session().1;
    run.stop().await;
    let recs = records(&log);
    let cut = recs
        .iter()
        .rposition(|r| matches!(r, SessionRecord::ToolResult { .. }))
        .unwrap();
    let text = std::fs::read_to_string(&log).unwrap();
    let kept: Vec<&str> = text.lines().take(cut + 2).collect();
    std::fs::write(&log, kept.join("\n") + "\n").unwrap();
    let refused = fork(&repo, &log, 2).await;
    match refused {
        Err(WtError::Partial(note)) => assert!(note.contains("command effects"), "{note}"),
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert_eq!(logs(&repo.home()).len(), 1, "no session was made");
    assert!(!repo.home().join("worktrees").exists());
}

#[tokio::test]
async fn copied_records_resolve_under_the_new_root_only() {
    // Review Focus 14: a record path that would leave the root is refused before
    // anything is made.
    let repo = Repo::git_repo();
    let (_, log) = two_turn_session(&repo).await;
    let text = std::fs::read_to_string(&log).unwrap();
    let mut lines: Vec<Value> = text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let edit = lines
        .iter_mut()
        .find(|v| v["data"]["type"] == "edit")
        .unwrap();
    edit["data"]["data"]["path"] = json!("../escape.txt");
    let out: Vec<String> = lines.iter().map(Value::to_string).collect();
    std::fs::write(&log, out.join("\n") + "\n").unwrap();
    let refused = fork(&repo, &log, 3).await;
    assert!(matches!(refused, Err(WtError::Path(_))), "{refused:?}");
    assert_eq!(logs(&repo.home()).len(), 1, "no session was made");
    assert!(!repo.home().join("worktrees").exists());
    assert!(!repo.root().parent().unwrap().join("escape.txt").exists());
}

#[tokio::test]
async fn an_unknown_step_is_refused_before_anything_is_made() {
    let repo = Repo::git_repo();
    let (_, log) = two_turn_session(&repo).await;
    let refused = fork(&repo, &log, 9).await;
    assert!(
        matches!(
            refused,
            Err(WtError::Tt(tautly_core::TtError::NoSuchStep {
                at: 9,
                last: 6
            }))
        ),
        "{refused:?}"
    );
    assert!(!repo.home().join("worktrees").exists());
}
