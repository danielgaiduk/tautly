//! A four-hour session on a looping mock (spec 9.1): every user turn reads, greps and
//! edits, the small context window makes it compact again and again, and the RSS stays
//! within ±10% of its reading at the 10-minute mark.
//!
//! The mock answers at once, so simulated time is counted in turns: a user turn every
//! 30 s, 20 turns per 10 minutes, 480 turns in all (the session's own timers never come
//! into it, so nothing is gained from pausing Tokio's clock).
//!
//! `cargo test --release -p tautly-perf --test soak -- --ignored`

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};

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
/// Live bytes of the Rust heap: this binary's allocator counts them.
static LIVE: AtomicI64 = AtomicI64::new(0);

struct Counting;

// SAFETY: every call is passed to the system allocator unchanged; only a counter moves.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        LIVE.fetch_add(l.size() as i64, Ordering::Relaxed);
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        LIVE.fetch_sub(l.size() as i64, Ordering::Relaxed);
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        LIVE.fetch_add(new as i64 - l.size() as i64, Ordering::Relaxed);
        unsafe { System.realloc(p, l, new) }
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

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
    let mut heap: Vec<(f64, f64)> = Vec::new();
    // Linux only: which kind of resident memory grows (at 10 min and at the end).
    let mut splits: Vec<String> = Vec::new();
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
            if samples.is_empty() || turn == TURNS {
                splits.push(rss_split());
            }
            samples.push((turn / TURNS_PER_10_MIN * 10, rss()));
            heap.push((
                (turn / TURNS_PER_10_MIN * 10) as f64,
                LIVE.load(Ordering::Relaxed) as f64,
            ));
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
    // A linear leak can hide inside ±10% of RSS over 4 h (Task 33 review I3): fit a line to
    // the live Rust heap from the first hour on (after warm-up) and project it over 4 h.
    // The heap, not RSS: RSS also holds what SQLite allocates (outside this allocator, its
    // page caches fill up to cache_size) and memory the system allocator keeps.
    let late: Vec<(f64, f64)> = heap.iter().copied().filter(|p| p.0 >= 60.0).collect();
    let n = late.len() as f64;
    let (mx, my) = (
        late.iter().map(|p| p.0).sum::<f64>() / n,
        late.iter().map(|p| p.1).sum::<f64>() / n,
    );
    let slope = late.iter().map(|p| (p.0 - mx) * (p.1 - my)).sum::<f64>()
        / late.iter().map(|p| (p.0 - mx).powi(2)).sum::<f64>();
    let heap_base = heap[0].1.max(1.0);
    let growth = slope * 240.0 / heap_base;
    if splits.iter().any(|s| !s.is_empty()) {
        report(&format!("soak: RSS split {}", splits.join(" -> ")));
    }
    report(&format!(
        "soak: {TURNS} turns (4 h simulated), {compactions} compactions, {tool_errors} tool errors; \
         RSS MB {}; worst deviation from the 10-minute mark {:.1}%; live heap {:.2} -> {:.2} MB, \
         fitted growth {:.1}% per 4 h",
        readings.join(", "),
        worst * 100.0,
        heap_base / MB as f64,
        heap.last().unwrap().1 / MB as f64,
        growth * 100.0
    ));
    // Before the fix the heap grew about 0.4 KB per tool result: +160% over the 4 h.
    assert!(
        growth < 0.10,
        "the live heap grows linearly: {:.1}% per 4 h",
        growth * 100.0
    );
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

/// `RssAnon`, `RssFile` and `RssShmem` of this process (Linux), else empty.
fn rss_split() -> String {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    status
        .lines()
        .filter(|l| {
            ["RssAnon:", "RssFile:", "RssShmem:"]
                .iter()
                .any(|k| l.starts_with(k))
        })
        .map(|l| l.split_whitespace().take(2).collect::<Vec<_>>().join(" "))
        .collect::<Vec<_>>()
        .join(" ")
}
