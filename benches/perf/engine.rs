//! Engine gates: a flood of commands during a turn is refused past the queues' capacity
//! with bounded memory and log (CP5-A M1), and a session's log does not repeat what its
//! blobs hold (the log-size gate of the format freeze, CP5-C M6).
//!
//! `cargo test --release -p tautly-perf --test engine -- --ignored --test-threads=1`

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::{Value, json};
use tautly_core::{Command, Config, Engine, EngineBuilder, Event};
use tautly_perf::{MB, report, rss};
use tautly_provider::{Catalog, MockProvider, MockTurn, ModelId, ProviderEvent, StopReason};
use tautly_sandbox::Mode;
use tempfile::TempDir;
use tokio::sync::mpsc;

const MODEL: &str = "mock/test";

fn config(mode: Mode) -> Config {
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

fn build(
    dir: &Path,
    home: &Path,
    mode: Mode,
    mock: &MockProvider,
) -> (Engine, mpsc::Receiver<Event>) {
    EngineBuilder {
        provider: Some(Arc::new(mock.clone())),
        data_home: Some(home.to_path_buf()),
        interactive: true,
        ..EngineBuilder::new(config(mode), dir)
    }
    .build()
    .unwrap()
}

fn file_len(p: &Path) -> u64 {
    std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "perf gate"]
async fn a_command_flood_is_refused_with_bounded_memory_and_log() {
    let (dir, home) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    let mock = MockProvider::new(vec![tools_step(vec![call(
        "w1",
        "write",
        json!({"path": "o.txt", "content": "x"}),
    )])]);
    let (engine, mut events) = build(dir.path(), home.path(), Mode::Ask, &mock);
    engine
        .send(Command::SendMessage {
            id: None,
            text: "first".into(),
        })
        .await
        .unwrap();
    // The turn waits for an approval from here on.
    loop {
        if let Event::ToolApprovalRequested { .. } = events.recv().await.unwrap() {
            break;
        }
    }
    let log = engine.session_path().to_path_buf();
    let (rss_before, log_before) = (rss(), file_len(&log));
    // A client that keeps reading: the refusals have somewhere to go.
    let refused = Arc::new(AtomicUsize::new(0));
    let reader = {
        let refused = Arc::clone(&refused);
        tokio::spawn(async move {
            while let Some(ev) = events.recv().await {
                if matches!(&ev, Event::Error { message, .. } if message.contains("too many commands"))
                {
                    refused.fetch_add(1, Ordering::Relaxed);
                }
            }
        })
    };
    let text = "y".repeat(1000);
    for i in 0..1000 {
        engine
            .send(Command::Pin {
                text: format!("pin {i} {text}"),
            })
            .await
            .unwrap();
        engine
            .send(Command::SendMessage {
                id: Some(format!("m{i}")),
                text: text.clone(),
            })
            .await
            .unwrap();
    }
    let rss_grown = rss().saturating_sub(rss_before);
    let log_grown = file_len(&log).saturating_sub(log_before);
    engine.send(Command::Shutdown).await.unwrap();
    reader.await.unwrap();
    let refused = refused.load(Ordering::Relaxed);
    report(&format!(
        "command flood: 2,000 commands of 1 KB during a turn, {refused} refusals seen, log +{} KiB, RSS +{:.1} MB",
        log_grown / 1024,
        rss_grown as f64 / MB as f64
    ));
    // Two queues of 64 take 128 commands; the rest is refused (a refusal the client is too
    // slow for is dropped and counted, so the count seen is a lower bound).
    assert!(refused > 0);
    // Only what the queues took is logged: 128 commands of about 1 KB.
    assert!(log_grown < 512 * 1024, "{log_grown}");
    assert!(rss_grown < 16 * MB, "{rss_grown}");
}

/// The `blobs/` directory next to the session's log.
fn blobs_dir(log: &Path) -> PathBuf {
    log.parent().unwrap().join("blobs")
}

fn dir_size(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|e| {
            let p = e.path();
            if p.is_dir() {
                dir_size(&p)
            } else {
                file_len(&p)
            }
        })
        .sum()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "perf gate"]
async fn ten_large_writes_are_logged_once() {
    let (dir, home) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    let contents: Vec<String> = (0..10)
        .map(|i| {
            (0..2500)
                .map(|l| format!("line {l:04} of file {i}: {}\n", "z".repeat(58)))
                .collect()
        })
        .collect();
    let mut script: Vec<MockTurn> = contents
        .iter()
        .enumerate()
        .map(|(i, c)| {
            tools_step(vec![call(
                &format!("w{i}"),
                "write",
                json!({"path": format!("big{i}.txt"), "content": c}),
            )])
        })
        .collect();
    script.push(text_step("done"));
    let mock = MockProvider::new(script);
    let (engine, mut events) = build(dir.path(), home.path(), Mode::Yolo, &mock);
    let log = engine.session_path().to_path_buf();
    let blobs_before = dir_size(&blobs_dir(&log));
    engine
        .send(Command::SendMessage {
            id: None,
            text: "write ten files".into(),
        })
        .await
        .unwrap();
    loop {
        match events.recv().await.unwrap() {
            Event::TurnFinished { .. } => break,
            Event::ToolFinished {
                is_error: true,
                output,
                ..
            } => panic!("{output}"),
            _ => {}
        }
    }
    engine.send(Command::Shutdown).await.unwrap();
    while events.recv().await.is_some() {}

    let blobs = blobs_dir(&log);
    let mut post_images = 0u64;
    for c in &contents {
        let hash = blake3::hash(c.as_bytes()).to_hex().to_string();
        let blob = blobs.join(&hash[..2]).join(&hash[2..]);
        assert!(blob.is_file(), "no post-image blob for a write");
        post_images += c.len() as u64;
    }
    let jsonl = file_len(&log);
    let added = dir_size(&blobs) - blobs_before;
    let rest = jsonl + added - post_images;
    let limit = post_images * 13 / 10 + 64 * 1024;
    report(&format!(
        "log size: ten 200 KB writes, .jsonl {} KiB + blobs {} KiB - post-images {} KiB = {} KiB (limit {} KiB)",
        jsonl / 1024,
        added / 1024,
        post_images / 1024,
        rest / 1024,
        limit / 1024
    ));
    assert!(rest < limit, "{rest} >= {limit}");
}
