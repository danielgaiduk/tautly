//! A four-hour session on a looping mock (spec 9.1): every user turn reads, greps and
//! edits, the small context window makes it compact again and again, and the RSS stays
//! within ±10% of its reading at the 10-minute mark.
//!
//! The mock answers at once, so simulated time is counted in turns: a user turn every
//! 30 s, 20 turns per 10 minutes, 480 turns in all (the session's own timers never come
//! into it, so nothing is gained from pausing Tokio's clock).
//!
//! `cargo test --release -p tautly-perf --test soak -- --ignored`

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures::StreamExt as _;
use futures::stream::BoxStream;
use serde_json::json;
use tautly_core::{Command, Config, EngineBuilder, Event};
use tautly_perf::{MB, report, rss};
use tautly_provider::{
    CanonicalRequest, Catalog, ModelId, Provider, ProviderError, ProviderEvent, StopReason,
};
use tautly_sandbox::Mode;
use tokio_util::sync::CancellationToken;

const MODEL: &str = "mock/test";
const TURNS_PER_10_MIN: usize = 20;
const TURNS: usize = 24 * TURNS_PER_10_MIN;

/// Answers each user turn with a read, a grep, an edit and a closing text, forever; a
/// request without tools (a compaction summary or the session's episode) gets a short
/// text. Keeps nothing per request.
#[derive(Default)]
struct Looping {
    main: AtomicUsize,
}

fn call(id: String, name: &str, args: serde_json::Value) -> ProviderEvent {
    ProviderEvent::ToolCall {
        id,
        name: name.into(),
        input_json: args.to_string(),
    }
}

impl Provider for Looping {
    fn name(&self) -> &str {
        "mock"
    }

    fn stream(
        &self,
        req: CanonicalRequest,
        _cancel: CancellationToken,
    ) -> BoxStream<'static, Result<ProviderEvent, ProviderError>> {
        let done = |stop| ProviderEvent::Done { stop };
        let evs = if req.tools.is_empty() {
            vec![
                ProviderEvent::TextDelta(
                    "Summary: the agent reads, greps and edits src/lib.rs.".into(),
                ),
                done(StopReason::EndTurn),
            ]
        } else {
            let k = self.main.fetch_add(1, Ordering::SeqCst);
            let (turn, step) = (k / 4, k % 4);
            let (from, to) = if turn % 2 == 0 {
                ("1", "2")
            } else {
                ("2", "1")
            };
            let id = format!("c{k}");
            match step {
                0 => vec![
                    call(id, "read", json!({"path": "src/lib.rs"})),
                    done(StopReason::ToolUse),
                ],
                1 => vec![
                    call(id, "grep", json!({"pattern": "VALUE"})),
                    done(StopReason::ToolUse),
                ],
                2 => vec![
                    call(
                        id,
                        "edit",
                        json!({"path": "src/lib.rs",
                               "old_text": format!("VALUE: u32 = {from};"),
                               "new_text": format!("VALUE: u32 = {to};")}),
                    ),
                    done(StopReason::ToolUse),
                ],
                _ => vec![
                    ProviderEvent::TextDelta(
                        format!("Turn {turn} done: VALUE is {to} now. ").repeat(20),
                    ),
                    done(StopReason::EndTurn),
                ],
            }
        };
        futures::stream::iter(evs.into_iter().map(Ok)).boxed()
    }
}

fn config() -> Config {
    let catalog = Catalog::from_toml(&format!(
        r#"[models."{MODEL}"]
context_window = 12000
max_output = 1000
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
    c.permissions.mode = Mode::Yolo;
    c
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "perf gate"]
async fn four_hour_session_keeps_rss_flat() {
    let (dir, home) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    let body: String = (0..60)
        .map(|i| format!("pub fn f{i}() -> u32 {{ VALUE + {i} }}\n"))
        .collect();
    std::fs::write(
        dir.path().join("src/lib.rs"),
        format!("pub const VALUE: u32 = 1;\n{body}"),
    )
    .unwrap();
    let provider = Arc::new(Looping::default());
    let (engine, mut events) = EngineBuilder {
        provider: Some(provider.clone()),
        side_provider: Some(provider),
        data_home: Some(home.path().to_path_buf()),
        ..EngineBuilder::new(config(), dir.path())
    }
    .build()
    .unwrap();

    let (mut compactions, mut tool_errors) = (0, 0);
    let mut samples: Vec<(usize, u64)> = Vec::new();
    for turn in 1..=TURNS {
        engine
            .send(Command::SendMessage {
                id: None,
                text: format!("turn {turn}: bump VALUE"),
            })
            .await
            .unwrap();
        loop {
            match events.recv().await.expect("the session ended") {
                Event::TurnFinished { .. } => break,
                Event::Compacted { .. } => compactions += 1,
                Event::ToolFinished { is_error: true, .. } => tool_errors += 1,
                _ => {}
            }
        }
        if turn % TURNS_PER_10_MIN == 0 {
            samples.push((turn / TURNS_PER_10_MIN * 10, rss()));
        }
    }
    engine.send(Command::Shutdown).await.unwrap();
    while events.recv().await.is_some() {}

    let base = samples[0].1;
    let worst = samples
        .iter()
        .map(|&(_, r)| (r as f64 - base as f64).abs() / base as f64)
        .fold(0.0, f64::max);
    let readings: Vec<String> = samples
        .iter()
        .map(|(min, r)| format!("{min}m {:.1}", *r as f64 / MB as f64))
        .collect();
    report(&format!(
        "soak: {TURNS} turns (4 h simulated), {compactions} compactions, {tool_errors} tool errors; \
         RSS MB {}; worst deviation from the 10-minute mark {:.1}%",
        readings.join(", "),
        worst * 100.0
    ));
    assert!(
        compactions >= 4,
        "the session should compact repeatedly: {compactions}"
    );
    assert_eq!(tool_errors, 0);
    assert!(
        worst <= 0.10,
        "RSS left ±10% of the 10-minute mark: {readings:?}"
    );
}
