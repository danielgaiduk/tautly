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
            .send(Command::SendMessage {
                id: None,
                text: text.into(),
            })
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
    // cp5 review C M7, M9: the step it branched at, and the parent's hash and tags.
    assert_eq!(header.parent_step, Some(3));
    let parent_header = reader(&log).header().clone();
    assert!(parent_header.config_hash.is_some());
    assert_eq!(header.config_hash, parent_header.config_hash);
    assert_eq!(header.tags, parent_header.tags);
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

/// Whether this git runs hooks declared in configuration (git 2.54 and later); see the
/// copy in `tests/checkpoint.rs`.
fn config_hooks_supported() -> bool {
    let text = git_in(Path::new("."), &["version"]).unwrap_or_default();
    let text = text.trim();
    let v: Vec<u32> = text
        .trim_start_matches("git version ")
        .split(['.', ' '])
        .take(2)
        .filter_map(|p| p.parse().ok())
        .collect();
    let ok = v.len() == 2 && (v[0], v[1]) >= (2, 54);
    if !ok {
        assert!(
            std::env::var_os("TAUTLY_REQUIRE_CONFIG_HOOKS").is_none(),
            "{text} predates config-based hooks"
        );
        eprintln!("note: skipped, {text} predates config-based hooks (git 2.54)");
    }
    ok
}

/// CP5 review C1: a hook the configuration declares (`hook.<name>.*`) runs neither when
/// the fork's worktree is added and checked out nor when the fork is deleted.
#[tokio::test]
async fn config_declared_hooks_not_run_on_fork_or_delete() {
    if !config_hooks_supported() {
        return;
    }
    let repo = Repo::git_repo();
    let (_, log) = two_turn_session(&repo).await;
    let marker = repo.home().join("hook-ran");
    let touch = format!("touch '{}'", marker.display());
    repo.git(&["config", "hook.x.command", &touch]).unwrap();
    for e in [
        "reference-transaction",
        "post-index-change",
        "post-checkout",
    ] {
        repo.git(&["config", "--add", "hook.x.event", e]).unwrap();
    }
    let made = fork(&repo, &log, 3).await.unwrap();
    assert!(made.worktree.join("a.txt").exists());
    delete(&repo, &repo.root(), &made.session_id, false)
        .await
        .unwrap();
    assert!(!made.worktree.exists());
    assert!(!marker.exists(), "a configured hook ran");
    // Control: git itself runs it.
    repo.git(&["update-ref", "refs/heads/control", "HEAD"])
        .unwrap();
    assert!(marker.exists(), "the hook was not set up to run");
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

// ------------------------------------------------------------- fix round 1 (Task 50)

/// A filter script that leaves a marker `<home>/filter-<name>-<op>` and passes the content
/// through; its path.
fn filter_script(repo: &Repo, name: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = repo.home().join(format!("{name}.sh"));
    let marker = repo.home().join(format!("filter-{name}"));
    let script = format!("#!/bin/sh\ntouch '{}-'\"$1\"\ncat\n", marker.display());
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// The markers filter scripts left.
fn filter_markers(repo: &Repo) -> Vec<String> {
    let mut out: Vec<String> = std::fs::read_dir(repo.home())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("filter-"))
        .collect();
    out.sort();
    out
}

/// `sessions delete` of `id` from the work tree `root`.
async fn delete(
    repo: &Repo,
    root: &Path,
    id: &str,
    force: bool,
) -> Result<tautly_core::sessions::Deleted, tautly_core::sessions::DeleteError> {
    let sb = LocalSandbox::new(root.to_path_buf()).unwrap();
    tautly_core::sessions::delete_with(&repo.home(), &sb, id, force).await
}

#[tokio::test]
async fn worktree_scoped_filters_never_run_on_fork_or_delete() {
    // Review C1: a filter the configuration defines only for linked worktrees
    // (`includeIf gitdir:**/worktrees/**`, and a worktree's own `config.worktree`) is
    // switched off as well: the worktree is checked out, and checked for changes, in its
    // own configuration's context.
    let repo = Repo::git_repo();
    repo.write(".gitattributes", "*.txt filter=wt\n*.md filter=wt2\n");
    repo.write("n.md", "note\n");
    repo.git(&["add", "-A"]).unwrap();
    repo.git(&["commit", "-q", "-m", "attributes"]).unwrap();
    let (_, log) = two_turn_session(&repo).await;
    let wt = filter_script(&repo, "wt");
    let wt2 = filter_script(&repo, "wt2");
    let include = repo.home().join("worktrees.inc");
    std::fs::write(
        &include,
        format!(
            "[filter \"wt\"]\n\tsmudge = '{0}' smudge\n\tclean = '{0}' clean\n\trequired = true\n",
            wt.display()
        ),
    )
    .unwrap();
    repo.git(&[
        "config",
        "includeIf.gitdir:**/worktrees/**.path",
        include.to_str().unwrap(),
    ])
    .unwrap();
    // Git itself would run it in a linked worktree.
    let probe = repo.home().join("probe");
    repo.git(&[
        "worktree",
        "add",
        "--detach",
        probe.to_str().unwrap(),
        "HEAD",
    ])
    .unwrap();
    assert_eq!(filter_markers(&repo), ["filter-wt-smudge"]);
    repo.git(&["worktree", "remove", "--force", probe.to_str().unwrap()])
        .unwrap();
    for m in filter_markers(&repo) {
        std::fs::remove_file(repo.home().join(m)).unwrap();
    }

    let made = fork(&repo, &log, 3).await.unwrap();
    assert_eq!(
        filter_markers(&repo),
        Vec::<String>::new(),
        "a filter ran at fork"
    );
    assert_eq!(
        std::fs::read_to_string(made.worktree.join("a.txt")).unwrap(),
        "one\n"
    );
    // The worktree's own configuration defines another, for the delete's check.
    repo.git(&["config", "extensions.worktreeConfig", "true"])
        .unwrap();
    git_in(
        &made.worktree,
        &[
            "config",
            "--worktree",
            "filter.wt2.clean",
            &format!("'{}' clean", wt2.display()),
        ],
    )
    .unwrap();
    // Rewritten later: the check must look at their content again.
    std::thread::sleep(Duration::from_millis(1100));
    for f in ["a.txt", "n.md"] {
        let p = made.worktree.join(f);
        let text = std::fs::read(&p).unwrap();
        std::fs::write(&p, text).unwrap();
    }
    // Git itself would run the worktree's clean filter.
    git_in(&made.worktree, &["status", "--porcelain"]).unwrap();
    assert!(
        filter_markers(&repo).contains(&"filter-wt2-clean".to_string()),
        "{:?}",
        filter_markers(&repo)
    );
    for m in filter_markers(&repo) {
        std::fs::remove_file(repo.home().join(m)).unwrap();
    }
    std::thread::sleep(Duration::from_millis(1100));
    for f in ["a.txt", "n.md"] {
        let p = made.worktree.join(f);
        let text = std::fs::read(&p).unwrap();
        std::fs::write(&p, text).unwrap();
    }
    let done = delete(&repo, &repo.root(), &made.session_id, false)
        .await
        .unwrap();
    assert_eq!(done.worktree.as_deref(), Some(made.worktree.as_path()));
    assert!(!made.worktree.exists());
    assert_eq!(
        filter_markers(&repo),
        Vec::<String>::new(),
        "a filter ran at delete"
    );
}

#[tokio::test]
async fn delete_from_inside_the_fork_refuses_and_keeps_everything() {
    // Review I1.
    let repo = Repo::git_repo();
    let (_, log) = two_turn_session(&repo).await;
    let made = fork(&repo, &log, 3).await.unwrap();
    let refused = delete(&repo, &made.worktree, &made.session_id, true).await;
    match refused {
        Err(e @ tautly_core::sessions::DeleteError::InsideWorktree(_)) => {
            assert!(e.to_string().contains("another worktree"), "{e}");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert!(made.worktree.exists());
    assert!(made.log.exists());
}

#[tokio::test]
async fn delete_refuses_while_a_live_session_uses_the_worktree() {
    // Review I2: a session (not the fork's own) running in the fork's worktree.
    let repo = Repo::git_repo();
    let (_, log) = two_turn_session(&repo).await;
    let made = fork(&repo, &log, 3).await.unwrap();
    let live = Run::start(EngineBuilder {
        root: made.worktree.clone(),
        ..repo.builder(&MockProvider::new(vec![]))
    });
    let refused = delete(&repo, &repo.root(), &made.session_id, true).await;
    assert!(
        matches!(
            refused,
            Err(tautly_core::sessions::DeleteError::WorktreeBusy(_))
        ),
        "{refused:?}"
    );
    assert!(made.worktree.exists());
    assert!(made.log.exists());
    live.stop().await;
    let done = delete(&repo, &repo.root(), &made.session_id, false)
        .await
        .unwrap();
    assert!(done.worktree.is_some());
    assert!(!made.worktree.exists());
}

#[tokio::test]
async fn delete_names_ignored_files_and_needs_force_for_them() {
    // Review M8 (and M2: decided from tautly's own status, not git's message).
    let repo = Repo::git_repo();
    repo.write(".gitignore", "*.log\n");
    repo.git(&["add", "-A"]).unwrap();
    repo.git(&["commit", "-q", "-m", "ignore"]).unwrap();
    let (_, log) = two_turn_session(&repo).await;
    let made = fork(&repo, &log, 3).await.unwrap();
    std::fs::write(made.worktree.join("notes.log"), "mine\n").unwrap();
    let refused = delete(&repo, &repo.root(), &made.session_id, false).await;
    match refused {
        Err(tautly_core::sessions::DeleteError::Worktree(WtError::Ignored { files, .. })) => {
            assert_eq!(files, ["notes.log"]);
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert!(made.worktree.join("notes.log").exists());
    // A modified file is a change.
    std::fs::remove_file(made.worktree.join("notes.log")).unwrap();
    std::fs::write(made.worktree.join("a.txt"), "edited\n").unwrap();
    let refused = delete(&repo, &repo.root(), &made.session_id, false).await;
    assert!(
        matches!(
            refused,
            Err(tautly_core::sessions::DeleteError::Worktree(
                WtError::Dirty(_)
            ))
        ),
        "{refused:?}"
    );
    delete(&repo, &repo.root(), &made.session_id, true)
        .await
        .unwrap();
    assert!(!made.worktree.exists());
}

#[tokio::test]
async fn a_dir_inside_a_worktree_of_the_repository_is_refused() {
    // Review M3.
    let repo = Repo::git_repo();
    let (_, log) = two_turn_session(&repo).await;
    let at = |dir: PathBuf| ForkRequest {
        at: Some(3),
        dir: Some(dir),
        ..ForkRequest::default()
    };
    let status = repo.git(&["status", "--porcelain"]).unwrap();
    let inside = repo.root().join("inner");
    let refused =
        worktree::fork_to_worktree(repo.sandbox(), &repo.home(), &log, at(inside.clone())).await;
    assert!(
        matches!(refused, Err(WtError::InsideRepository(_))),
        "{refused:?}"
    );
    assert!(!inside.exists());
    let made = fork(&repo, &log, 3).await.unwrap();
    let nested = made.worktree.join("deeper/wt");
    let refused =
        worktree::fork_to_worktree(repo.sandbox(), &repo.home(), &log, at(nested.clone())).await;
    assert!(
        matches!(refused, Err(WtError::InsideRepository(_))),
        "{refused:?}"
    );
    assert!(!made.worktree.join("deeper").exists());
    assert_eq!(repo.git(&["status", "--porcelain"]).unwrap(), status);
}

// ------------------------------------------------------------- fix round 2 (Task 50)

#[tokio::test]
async fn a_populated_submodule_refuses_delete_and_its_filter_never_runs() {
    // Re-review N1: a submodule's own configuration (its git dir under the fork's admin
    // directory) is never read by tautly's status, and a worktree holding one is not
    // removed without --force, as git itself refuses.
    let repo = Repo::git_repo();
    let (_, log) = two_turn_session(&repo).await;
    let made = fork(&repo, &log, 3).await.unwrap();
    let source = tempfile::tempdir().unwrap();
    let src = source.path().canonicalize().unwrap();
    git_in(&src, &["init", "-q", "-b", "main"]).unwrap();
    std::fs::write(src.join(".gitattributes"), "*.txt filter=sf\n").unwrap();
    std::fs::write(src.join("s.txt"), "sub\n").unwrap();
    git_in(&src, &["add", "-A"]).unwrap();
    git_in(&src, &["commit", "-q", "-m", "sub"]).unwrap();
    git_in(
        &made.worktree,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "-q",
            src.to_str().unwrap(),
            "sub",
        ],
    )
    .unwrap();
    let sf = filter_script(&repo, "sf");
    git_in(
        &made.worktree.join("sub"),
        &[
            "config",
            "filter.sf.clean",
            &format!("'{}' clean", sf.display()),
        ],
    )
    .unwrap();
    let touch = || {
        std::thread::sleep(Duration::from_millis(1100));
        let p = made.worktree.join("sub/s.txt");
        let text = std::fs::read(&p).unwrap();
        std::fs::write(&p, text).unwrap();
    };
    // Git itself would run it.
    touch();
    git_in(&made.worktree, &["status", "--porcelain"]).unwrap();
    assert_eq!(filter_markers(&repo), ["filter-sf-clean"]);
    std::fs::remove_file(repo.home().join("filter-sf-clean")).unwrap();
    touch();
    let refused = delete(&repo, &repo.root(), &made.session_id, false).await;
    match refused {
        Err(tautly_core::sessions::DeleteError::Worktree(WtError::Submodules {
            paths, ..
        })) => {
            assert_eq!(paths, ["sub"]);
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert!(made.worktree.join("sub/s.txt").exists());
    assert_eq!(
        filter_markers(&repo),
        Vec::<String>::new(),
        "the submodule's filter ran"
    );
    delete(&repo, &repo.root(), &made.session_id, true)
        .await
        .unwrap();
    assert!(!made.worktree.exists());
    assert_eq!(filter_markers(&repo), Vec::<String>::new());
}

#[tokio::test]
async fn a_commit_only_the_forks_head_reaches_refuses_delete() {
    // Re-review N3: the fork's HEAD is detached; a commit made there would be lost.
    let repo = Repo::git_repo();
    let (_, log) = two_turn_session(&repo).await;
    let made = fork(&repo, &log, 3).await.unwrap();
    git_in(
        &made.worktree,
        &["commit", "-q", "--allow-empty", "-m", "mine"],
    )
    .unwrap();
    let head = git_in(&made.worktree, &["rev-parse", "HEAD"]).unwrap();
    let head = head.trim();
    let refused = delete(&repo, &repo.root(), &made.session_id, false).await;
    match refused {
        Err(tautly_core::sessions::DeleteError::Worktree(
            ref e @ WtError::Unreachable { ref commit, .. },
        )) => {
            assert_eq!(commit, head);
            assert!(
                e.to_string().contains(&format!("git branch <name> {head}")),
                "{e}"
            );
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert!(made.worktree.exists());
    // Once a branch holds it, it goes.
    repo.git(&["branch", "keep", head]).unwrap();
    delete(&repo, &repo.root(), &made.session_id, false)
        .await
        .unwrap();
    assert!(!made.worktree.exists());
    assert_eq!(repo.git(&["rev-parse", "keep"]).unwrap().trim(), head);
}

#[tokio::test]
async fn a_symlink_dir_is_refused_and_left_alone() {
    // Re-review N2: a link given as the directory (dangling or not) is never followed or
    // removed.
    let repo = Repo::git_repo();
    let (_, log) = two_turn_session(&repo).await;
    let target = repo.home().join("target");
    std::fs::create_dir(&target).unwrap();
    for (name, to) in [
        ("live", target.clone()),
        ("dangling", repo.home().join("nowhere")),
    ] {
        let link = repo.home().join(name);
        std::os::unix::fs::symlink(&to, &link).unwrap();
        let refused = worktree::fork_to_worktree(
            repo.sandbox(),
            &repo.home(),
            &log,
            ForkRequest {
                at: Some(3),
                dir: Some(link.clone()),
                ..ForkRequest::default()
            },
        )
        .await;
        assert!(matches!(refused, Err(WtError::DirIsLink(_))), "{refused:?}");
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }
    assert_eq!(std::fs::read_dir(&target).unwrap().count(), 0);
}

#[tokio::test]
async fn a_default_dir_inside_the_repository_names_dir_and_tautly_home() {
    // Re-review N5: the data home is inside the project's own work tree.
    let repo = Repo::git_repo();
    let (_, log) = two_turn_session(&repo).await;
    let home = repo.root().join(".data");
    let refused = worktree::fork_to_worktree(
        repo.sandbox(),
        &home,
        &log,
        ForkRequest {
            at: Some(3),
            ..ForkRequest::default()
        },
    )
    .await;
    let e = refused.expect_err("refused");
    let text = e.to_string();
    assert!(
        text.contains("--dir") && text.contains("TAUTLY_HOME"),
        "{text}"
    );
}
