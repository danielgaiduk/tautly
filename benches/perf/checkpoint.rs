//! Checkpoint timings (Task 47): a snapshot of a clean 10,000-file repository and the
//! reuse of an unchanged tree, median of 20, and the per-step latency checkpoints add to
//! a session. Recorded in the job summary; not gated until the numbers are stable on the
//! CI runners (the targets are < 300 ms and < 5 ms).
//!
//! `cargo test --release -p tautly-perf --test checkpoint -- --ignored --test-threads=1`

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;
use tautly_core::{CheckpointConfig, Checkpointer, Command, Config, EngineBuilder, Event};
use tautly_perf::gen_repo::generate;
use tautly_perf::{median, ms, report};
use tautly_provider::{Catalog, MockProvider, MockTurn, ModelId, ProviderEvent, StopReason};
use tautly_sandbox::{LocalSandbox, Mode, Sandbox};
use tempfile::TempDir;

const MODEL: &str = "mock/test";

fn git(root: &Path, args: &[&str]) {
    let ok = std::process::Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_AUTHOR_NAME", "perf")
        .env("GIT_AUTHOR_EMAIL", "perf@example.com")
        .env("GIT_COMMITTER_NAME", "perf")
        .env("GIT_COMMITTER_EMAIL", "perf@example.com")
        .status()
        .unwrap()
        .success();
    assert!(ok, "git {args:?}");
}

/// A committed repository of `files` generated files.
fn repo(files: usize) -> TempDir {
    let dir = TempDir::new().unwrap();
    generate(dir.path(), files, 4);
    git(dir.path(), &["init", "-q"]);
    git(dir.path(), &["add", "-A"]);
    git(dir.path(), &["commit", "-q", "-m", "generated"]);
    dir
}

fn patient() -> CheckpointConfig {
    CheckpointConfig {
        timeout_ms: 120_000,
        ..CheckpointConfig::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "perf measurement"]
async fn snapshot_of_a_clean_10k_repo_and_reuse() {
    let dir = repo(10_000);
    let root = dir.path().canonicalize().unwrap();
    let sb: Arc<dyn Sandbox> = Arc::new(LocalSandbox::new(root.clone()).unwrap());
    let mut cp = Checkpointer::new(sb, root, "perf".into(), patient()).unwrap();
    cp.snapshot_now().await.unwrap();
    let mut taken = Vec::new();
    for _ in 0..20 {
        let t = Instant::now();
        cp.snapshot_now().await.unwrap();
        taken.push(t.elapsed());
    }
    let mut reused = Vec::new();
    cp.snapshot(1, false).await.unwrap();
    for _ in 0..20 {
        let t = Instant::now();
        let r = cp.snapshot(1, false).await.unwrap();
        reused.push(t.elapsed());
        assert!(r.reused);
    }
    report(&format!(
        "checkpoint: clean 10,000-file repo {} (target < 300 ms), unchanged-tree reuse {} (target < 5 ms), median of 20",
        ms(median(taken)),
        ms(median(reused))
    ));
}

fn config(checkpoints: bool) -> Config {
    let catalog = Catalog::from_toml(&format!(
        r#"[models."{MODEL}"]
context_window = 1000000
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
    c.checkpoints = patient();
    c.checkpoints.enabled = checkpoints;
    c
}

/// Wall time per step of 20 turns, each one `write` and a closing text, in a committed
/// 1,000-file repository.
async fn per_step(checkpoints: bool) -> Duration {
    let (dir, home) = (repo(1_000), TempDir::new().unwrap());
    let mut script = Vec::new();
    for i in 0..20 {
        script.push(MockTurn::Events(vec![
            ProviderEvent::ToolCall {
                id: format!("w{i}"),
                name: "write".into(),
                input_json: json!({"path": format!("out{i}.txt"), "content": "x\n"}).to_string(),
            },
            ProviderEvent::Done {
                stop: StopReason::ToolUse,
            },
        ]));
        script.push(MockTurn::Events(vec![
            ProviderEvent::TextDelta("done".into()),
            ProviderEvent::Done {
                stop: StopReason::EndTurn,
            },
        ]));
    }
    let mock = MockProvider::new(script);
    let (engine, mut events) = EngineBuilder {
        provider: Some(Arc::new(mock)),
        data_home: Some(home.path().to_path_buf()),
        ..EngineBuilder::new(config(checkpoints), dir.path())
    }
    .build()
    .unwrap();
    let t = Instant::now();
    for i in 0..20 {
        engine
            .send(Command::SendMessage {
                id: None,
                text: format!("write {i}"),
            })
            .await
            .unwrap();
        while !matches!(events.recv().await.unwrap(), Event::TurnFinished { .. }) {}
    }
    let took = t.elapsed();
    engine.send(Command::Shutdown).await.unwrap();
    while events.recv().await.is_some() {}
    // Two steps per turn.
    took / 40
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "perf measurement"]
async fn per_step_latency_added_by_checkpoints() {
    let off = per_step(false).await;
    let on = per_step(true).await;
    report(&format!(
        "checkpoint: per-step latency {} with checkpoints, {} without (+{} per step, 1,000-file repo)",
        ms(on),
        ms(off),
        ms(on.saturating_sub(off))
    ));
}
