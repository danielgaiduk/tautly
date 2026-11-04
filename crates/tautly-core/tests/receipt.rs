//! Turn receipts and the savings meter (spec 7.11.1): the builder's arithmetic, the
//! `TurnReceipt` event the engine emits before every `TurnFinished`, the ledger row it
//! leaves, and the configuration that controls the REPL footer.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;
use tautly_core::{
    CliOverrides, Command, Config, Engine, EngineBuilder, Event, FinishReason, Ledger,
    ReceiptBuilder, Savings, UsageRole,
};
use tautly_provider::{
    Catalog, MockProvider, MockTurn, ModelId, Price, ProviderEvent, RawUsage, StopReason,
};
use tautly_sandbox::Mode;
use tautly_tools::{SaveKind, Saved, ToolMeta};
use tempfile::TempDir;
use tokio::sync::mpsc;

const MODEL: &str = "mock/test";
const WAIT: Duration = Duration::from_secs(30);
const PRICE_TOML: &str = "input = 3.0\noutput = 15.0\ncache_read = 0.3\ncache_write = 3.75";
const PRICE: Price = Price {
    input: 3.0,
    output: 15.0,
    cache_read: 0.3,
    cache_write: 3.75,
};

fn usage(input: u64, output: u64, cache_read: u64, cache_write: u64) -> RawUsage {
    RawUsage {
        input,
        output,
        cache_read,
        cache_write,
    }
}

fn builder() -> ReceiptBuilder {
    ReceiptBuilder::new(1, ModelId::from(MODEL), Instant::now())
}

fn saved_meta(kind: SaveKind, bytes: u32) -> ToolMeta {
    let mut meta = ToolMeta::default();
    meta.saved = Some(Saved { kind, bytes });
    meta
}

/// The receipt fields a test looks at.
#[derive(Debug)]
struct Receipt {
    turn: u32,
    tokens_in: u64,
    tokens_out: u64,
    cache_read: u64,
    cache_write: u64,
    side_tokens: u64,
    cost_usd: Option<f64>,
    wall_ms: u64,
    cache_hit_ratio: Option<f64>,
    steps: u32,
    tool_calls: u32,
    saved: Savings,
    estimated: bool,
}

fn receipt(ev: &Event) -> Receipt {
    match ev {
        Event::TurnReceipt {
            turn,
            tokens_in,
            tokens_out,
            cache_read,
            cache_write,
            side_tokens,
            cost_usd,
            wall_ms,
            cache_hit_ratio,
            steps,
            tool_calls,
            saved,
            estimated,
            ..
        } => Receipt {
            estimated: *estimated,
            turn: *turn,
            tokens_in: *tokens_in,
            tokens_out: *tokens_out,
            cache_read: *cache_read,
            cache_write: *cache_write,
            side_tokens: *side_tokens,
            cost_usd: *cost_usd,
            wall_ms: *wall_ms,
            cache_hit_ratio: *cache_hit_ratio,
            steps: *steps,
            tool_calls: *tool_calls,
            saved: saved.clone(),
        },
        other => panic!("not a receipt: {other:?}"),
    }
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
}

// ---- the builder ---------------------------------------------------------------------

#[test]
fn receipt_sums_main_and_side_and_reports_side_tokens() {
    let mut b = builder();
    b.request(2, 0);
    b.usage(UsageRole::Main, &usage(1000, 200, 300, 40), Some(0.01));
    b.usage(UsageRole::Side, &usage(500, 50, 10, 0), Some(0.002));
    b.request(3, 0);
    b.usage(UsageRole::Main, &usage(100, 20, 0, 0), Some(0.001));
    b.tool(&ToolMeta::default());
    let r = receipt(&b.finish(Some(&PRICE), Instant::now()));
    assert_eq!(r.turn, 1);
    assert_eq!((r.tokens_in, r.tokens_out), (1600, 270));
    assert_eq!((r.cache_read, r.cache_write), (310, 40));
    assert_eq!(r.side_tokens, 550);
    assert!(close(r.cost_usd.unwrap(), 0.013), "{r:?}");
    assert_eq!((r.steps, r.tool_calls), (2, 1));
    assert_eq!(r.saved.cache_read_tokens, 310);

    // One unpriced entry makes the cost unknown.
    let mut b = builder();
    b.usage(UsageRole::Main, &usage(10, 1, 0, 0), Some(0.5));
    b.usage(UsageRole::Side, &usage(10, 1, 0, 0), None);
    assert_eq!(receipt(&b.finish(None, Instant::now())).cost_usd, None);
}

#[test]
fn cache_hit_ratio_formula() {
    let mut b = builder();
    b.usage(UsageRole::Main, &usage(200, 10, 800, 0), None);
    let r = receipt(&b.finish(None, Instant::now()));
    assert!(close(r.cache_hit_ratio.unwrap(), 0.8), "{r:?}");

    let r = receipt(&builder().finish(None, Instant::now()));
    assert_eq!(r.cache_hit_ratio, None);
}

#[test]
fn context_gc_saving_counts_elided_tokens_per_later_request() {
    // GC frees 3,000 tokens before request 2 of 3: requests 2 and 3 each go without them.
    let mut b = builder();
    b.request(2, 0);
    b.request(3, 3000);
    b.request(4, 3000);
    let r = receipt(&b.finish(None, Instant::now()));
    assert_eq!(r.saved.context_gc, 6000);
    assert_eq!(r.steps, 3);
}

#[test]
fn a_step_sent_again_counts_once_and_its_requests_each_count_gc() {
    // Step 2 overflowed and was sent again after a compaction: one step, two requests.
    let mut b = builder();
    b.request(2, 1000);
    b.request(2, 1000);
    b.request(3, 1000);
    let r = receipt(&b.finish(None, Instant::now()));
    assert_eq!(r.steps, 2);
    assert_eq!(r.saved.context_gc, 3000);
}

#[test]
fn tool_savings_are_bytes_over_four_by_kind() {
    let mut b = builder();
    b.tool(&saved_meta(SaveKind::Index, 4000));
    b.tool(&saved_meta(SaveKind::Index, 400));
    b.tool(&saved_meta(SaveKind::DiffRead, 800));
    b.tool(&saved_meta(SaveKind::Filter, 1203));
    b.tool(&ToolMeta::default());
    let r = receipt(&b.finish(None, Instant::now()));
    assert_eq!(
        (r.saved.index, r.saved.diff_reads, r.saved.filters),
        (1100, 200, 300)
    );
    assert_eq!(r.tool_calls, 5);
}

#[test]
fn cache_usd_formula_can_be_negative() {
    // A cold, write-heavy turn: 10,000 tokens written to the cache, 1,000 read.
    let mut b = builder();
    b.usage(UsageRole::Main, &usage(500, 100, 1000, 10_000), None);
    let r = receipt(&b.finish(Some(&PRICE), Instant::now()));
    let expected = (1000.0 * (3.0 - 0.3) - 10_000.0 * (3.75 - 3.0)) / 1e6;
    let got = r.saved.cache_usd.unwrap();
    assert!(close(got, expected), "{got} != {expected}");
    assert!(got < 0.0);

    // A warm turn saves.
    let mut b = builder();
    b.usage(UsageRole::Main, &usage(500, 100, 50_000, 0), None);
    assert!(
        receipt(&b.finish(Some(&PRICE), Instant::now()))
            .saved
            .cache_usd
            .unwrap()
            > 0.0
    );
}

#[test]
fn cache_usd_counts_only_the_main_models_cache_tokens() {
    // The side model's cache tokens are priced at its own rates, never the main model's:
    // they are left out of `cache_usd` (and still counted in the token fields).
    let mut b = builder();
    b.usage(UsageRole::Main, &usage(500, 100, 1000, 0), None);
    b.usage(UsageRole::Side, &usage(500, 100, 90_000, 70_000), None);
    let r = receipt(&b.finish(Some(&PRICE), Instant::now()));
    let expected = 1000.0 * (3.0 - 0.3) / 1e6;
    assert!(close(r.saved.cache_usd.unwrap(), expected), "{r:?}");
    assert_eq!((r.cache_read, r.cache_write), (91_000, 70_000));
    assert_eq!(r.saved.cache_read_tokens, 91_000);
}

#[test]
fn total_usd_prices_gc_at_cache_read_and_is_none_without_prices() {
    let run = |price: Option<&Price>| {
        let mut b = builder();
        b.request(2, 0);
        b.request(3, 10_000);
        b.tool(&saved_meta(SaveKind::Index, 40_000));
        b.tool(&saved_meta(SaveKind::DiffRead, 4_000));
        b.tool(&saved_meta(SaveKind::Filter, 4_000));
        b.usage(UsageRole::Main, &usage(1000, 100, 2000, 0), None);
        receipt(&b.finish(price, Instant::now()))
    };
    let r = run(Some(&PRICE));
    let s = &r.saved;
    assert_eq!(
        (s.index, s.diff_reads, s.filters, s.context_gc),
        (10_000, 1_000, 1_000, 10_000)
    );
    let cache = 2000.0 * (3.0 - 0.3) / 1e6;
    assert!(close(s.cache_usd.unwrap(), cache));
    let expected = 12_000.0 * 3.0 / 1e6 + 10_000.0 * 0.3 / 1e6 + cache;
    assert!(close(s.total_usd.unwrap(), expected), "{s:?}");

    let r = run(None);
    assert_eq!((r.saved.cache_usd, r.saved.total_usd), (None, None));
    assert_eq!(r.saved.index, 10_000, "token estimates need no price");
}

// ---- through the engine ----------------------------------------------------------------

fn config(max_turns: u32) -> Config {
    let catalog = Catalog::from_toml(&format!(
        "[models.\"{MODEL}\"]\ncontext_window = 200000\nmax_output = 8000\n\
         parallel_tools = true\nreasoning = \"none\"\ncache = \"none\"\nimages = false\n\
         edit_format = \"str_replace\"\n[models.\"{MODEL}\".price]\n{PRICE_TOML}\n"
    ))
    .unwrap();
    let mut c = Config {
        model: ModelId::from(MODEL),
        catalog_overrides: Some(catalog),
        max_turns,
        ..Config::default()
    };
    c.permissions.mode = Mode::Yolo;
    c
}

fn tools_step(calls: Vec<ProviderEvent>, u: RawUsage) -> MockTurn {
    let mut evs = calls;
    evs.push(ProviderEvent::Usage(u));
    evs.push(ProviderEvent::Done {
        stop: StopReason::ToolUse,
    });
    MockTurn::Events(evs)
}

fn bash(id: &str, cmd: &str) -> ProviderEvent {
    ProviderEvent::ToolCall {
        id: id.into(),
        name: "bash".into(),
        input_json: json!({ "command": cmd }).to_string(),
    }
}

fn text_step(text: &str, u: RawUsage) -> MockTurn {
    MockTurn::Events(vec![
        ProviderEvent::TextDelta(text.into()),
        ProviderEvent::Usage(u),
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

    fn root(&self) -> PathBuf {
        self.dir.path().canonicalize().unwrap()
    }

    fn builder(&self, cfg: Config, mock: &MockProvider) -> EngineBuilder {
        EngineBuilder {
            provider: Some(Arc::new(mock.clone())),
            data_home: Some(self.home.path().to_path_buf()),
            ..EngineBuilder::new(cfg, self.dir.path())
        }
    }

    fn start(&self, cfg: Config, mock: &MockProvider) -> Run {
        launch(self.builder(cfg, mock))
    }

    fn write(&self, rel: &str, text: &str) {
        std::fs::write(self.dir.path().join(rel), text).unwrap();
    }

    fn ledger(&self) -> Ledger {
        Ledger::open(&self.home.path().join("ledger.sqlite")).unwrap()
    }
}

struct Run {
    engine: Engine,
    events: mpsc::Receiver<Event>,
}

fn launch(b: EngineBuilder) -> Run {
    let (engine, events) = b.build().unwrap();
    Run { engine, events }
}

impl Run {
    /// Shut down and wait for the event stream to close (the log is released).
    async fn stop(mut self) {
        self.send(Command::Shutdown).await;
        let closed =
            tokio::time::timeout(WAIT, async { while self.events.recv().await.is_some() {} }).await;
        assert!(closed.is_ok(), "the engine did not stop");
    }

    async fn send(&self, cmd: Command) {
        self.engine.send(cmd).await.unwrap();
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

    async fn turn_with(&mut self, text: &str) -> Vec<Event> {
        self.send(Command::SendMessage { text: text.into() }).await;
        self.until(|e| matches!(e, Event::TurnFinished { .. }))
            .await
    }
}

/// The turn's receipt, which must come right before its `TurnFinished` (with `reason`),
/// and agree with the turn's `Usage` events.
fn check_receipt(events: &[Event], reason: FinishReason) -> Receipt {
    let n = events.len();
    assert!(
        matches!(&events[n - 1], Event::TurnFinished { reason: r, .. } if *r == reason),
        "{events:?}"
    );
    let r = receipt(&events[n - 2]);
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, Event::TurnReceipt { .. }))
            .count(),
        1
    );
    let (mut tin, mut tout) = (0, 0);
    for e in events {
        if let Event::Usage {
            tokens_in,
            tokens_out,
            ..
        } = e
        {
            tin += tokens_in;
            tout += tokens_out;
        }
    }
    assert_eq!((r.tokens_in, r.tokens_out), (tin, tout), "{events:?}");
    r
}

#[tokio::test]
async fn receipt_precedes_turn_finished_for_every_reason() {
    // Completed, twice: the receipt's turn is the 1-based user turn.
    let fx = Fx::new();
    let mock = MockProvider::new(vec![
        tools_step(vec![bash("b1", "true")], usage(100, 10, 0, 0)),
        text_step("done", usage(120, 5, 0, 0)),
        text_step("again", usage(10, 1, 0, 0)),
    ]);
    let mut run = fx.start(config(100), &mock);
    let r = check_receipt(&run.turn_with("go").await, FinishReason::Completed);
    assert_eq!((r.turn, r.steps, r.tool_calls), (1, 2, 1));
    assert_eq!((r.tokens_in, r.tokens_out), (220, 15));
    let expected = (220.0 * 3.0 + 15.0 * 15.0) / 1e6;
    assert!(close(r.cost_usd.unwrap(), expected), "{r:?}");
    let r = check_receipt(&run.turn_with("more").await, FinishReason::Completed);
    assert_eq!(r.turn, 2);

    // Cancelled while the request stalls: the estimated usage still counts.
    let fx = Fx::new();
    let mock = MockProvider::new(vec![MockTurn::Stall]);
    let mut run = fx.start(config(100), &mock);
    run.send(Command::SendMessage { text: "go".into() }).await;
    run.until(|e| matches!(e, Event::TurnStarted)).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    run.send(Command::Cancel).await;
    let events = run.until(|e| matches!(e, Event::TurnFinished { .. })).await;
    let r = check_receipt(&events, FinishReason::Cancelled);
    assert!(r.tokens_in > 0, "{r:?}");

    // Budget: the first response uses the turn's tokens up.
    let fx = Fx::new();
    let mut cfg = config(100);
    cfg.budget.turn.max_tokens = Some(100);
    let mock = MockProvider::new(vec![
        tools_step(vec![bash("b1", "true")], usage(100, 10, 0, 0)),
        text_step("never sent", usage(1, 1, 0, 0)),
    ]);
    let mut run = fx.start(cfg, &mock);
    let r = check_receipt(&run.turn_with("go").await, FinishReason::Budget);
    assert_eq!((r.tokens_in, r.steps), (100, 1));

    // Max turns: one step allowed, and it asked for a tool.
    let fx = Fx::new();
    let mock = MockProvider::new(vec![
        tools_step(vec![bash("b1", "true")], usage(50, 5, 0, 0)),
        text_step("never sent", usage(1, 1, 0, 0)),
    ]);
    let mut run = fx.start(config(1), &mock);
    let r = check_receipt(&run.turn_with("go").await, FinishReason::MaxTurns);
    assert_eq!((r.steps, r.tool_calls), (1, 1));
}

#[tokio::test]
async fn wall_ms_spans_turn_started_to_finished() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![
        tools_step(vec![bash("b1", "sleep 0.3")], usage(10, 1, 0, 0)),
        text_step("done", usage(10, 1, 0, 0)),
    ]);
    let mut run = fx.start(config(100), &mock);
    let sent = Instant::now();
    let events = run.turn_with("go").await;
    let elapsed = sent.elapsed().as_millis() as u64;
    let r = check_receipt(&events, FinishReason::Completed);
    assert!(
        (300..=elapsed).contains(&r.wall_ms),
        "wall_ms {} not within 300..={elapsed}",
        r.wall_ms
    );
}

#[tokio::test]
async fn receipt_row_written_to_ledger() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![
        tools_step(vec![bash("b1", "true")], usage(100, 10, 30, 5)),
        text_step("done", usage(120, 5, 0, 0)),
    ]);
    let mut run = fx.start(config(100), &mock);
    let events = run.turn_with("go").await;
    let r = check_receipt(&events, FinishReason::Completed);
    let session = run.engine.session_id().to_string();
    let end = Instant::now() + WAIT;
    let rows = loop {
        let rows = fx.ledger().session_receipts(&session).unwrap_or_default();
        if !rows.is_empty() {
            break rows;
        }
        assert!(Instant::now() < end, "no receipt row arrived");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(rows.len(), 1, "{rows:?}");
    let row = &rows[0];
    assert_eq!(row.turn, 1);
    assert_eq!(row.model, MODEL);
    assert_eq!(row.project, tautly_session::paths::project_id(&fx.root()));
    assert_eq!(row.root, fx.root());
    assert_eq!(
        (
            row.tokens_in,
            row.tokens_out,
            row.cache_read,
            row.cache_write
        ),
        (r.tokens_in, r.tokens_out, r.cache_read, r.cache_write)
    );
    assert_eq!((row.steps, row.tool_calls, row.wall_ms), (2, 1, r.wall_ms));
    assert!(close(row.cost_usd.unwrap(), r.cost_usd.unwrap()));
    assert_eq!(row.saved, r.saved);
}

fn call(id: &str, name: &str, args: serde_json::Value) -> ProviderEvent {
    ProviderEvent::ToolCall {
        id: id.into(),
        name: name.into(),
        input_json: args.to_string(),
    }
}

/// Four reads of small files, each its own step (enough history to compact).
fn four_reads(fx: &Fx) -> Vec<MockTurn> {
    (1..=4)
        .map(|i| {
            fx.write(&format!("f{i}.txt"), &format!("file {i}\n"));
            tools_step(
                vec![call(
                    &format!("r{i}"),
                    "read",
                    json!({ "path": format!("f{i}.txt") }),
                )],
                usage(10, 1, 0, 0),
            )
        })
        .collect()
}

#[tokio::test]
async fn an_overflow_retry_is_one_step() {
    let fx = Fx::new();
    let mut script = four_reads(&fx);
    script.push(text_step("first done", usage(10, 1, 0, 0)));
    // The second turn's step overflows, is compacted and sent again: still one step.
    script.push(MockTurn::Error(
        tautly_provider::ProviderError::ContextOverflow("too long".into()),
    ));
    script.push(text_step("SUMMARY-ONE", usage(10, 1, 0, 0)));
    script.push(text_step("second done", usage(10, 1, 0, 0)));
    let mock = MockProvider::new(script);
    let mut run = fx.start(config(100), &mock);
    check_receipt(&run.turn_with("first").await, FinishReason::Completed);
    let events = run.turn_with("second").await;
    assert!(
        events.iter().any(|e| matches!(e, Event::Compacted { .. })),
        "{events:?}"
    );
    let r = check_receipt(&events, FinishReason::Completed);
    assert_eq!(r.steps, 1, "{r:?}");
}

/// `tokens_freed` of the turn's one collection.
fn freed(events: &[Event]) -> u64 {
    let freed: Vec<u64> = events
        .iter()
        .filter_map(|e| match e {
            Event::ContextCollected { tokens_freed, .. } => Some(u64::from(*tokens_freed)),
            _ => None,
        })
        .collect();
    assert_eq!(freed.len(), 1, "{events:?}");
    assert!(freed[0] > 0);
    freed[0]
}

#[tokio::test]
async fn context_gc_counts_elisions_through_the_engine_and_across_a_resume() {
    let fx = Fx::new();
    let big: String = (0..400)
        .map(|i| format!("line {i}: some filler text to make this file large\n"))
        .collect();
    fx.write("big.txt", &big);
    // Request 2 re-reads the file, so request 3 starts with a collection that elides the
    // first read: requests 3 and 4 each go without its tokens.
    let mock = MockProvider::new(vec![
        tools_step(
            vec![call("r1", "read", json!({"path": "big.txt"}))],
            usage(10, 1, 0, 0),
        ),
        tools_step(
            vec![call("r2", "read", json!({"path": "big.txt", "full": true}))],
            usage(10, 1, 0, 0),
        ),
        tools_step(vec![bash("b1", "true")], usage(10, 1, 0, 0)),
        text_step("done", usage(10, 1, 0, 0)),
    ]);
    let mut run = fx.start(config(100), &mock);
    let events = run.turn_with("go").await;
    let freed = freed(&events);
    let r = check_receipt(&events, FinishReason::Completed);
    assert_eq!(r.steps, 4);
    assert_eq!(r.saved.context_gc, 2 * freed, "{r:?}");
    let id = run.engine.session_id().to_string();
    run.stop().await;

    // Resumed: the elided read is still out of the context at both requests.
    let mock = MockProvider::new(vec![
        tools_step(vec![bash("b2", "echo again")], usage(10, 1, 0, 0)),
        text_step("done again", usage(10, 1, 0, 0)),
    ]);
    let mut run = launch(EngineBuilder {
        resume: Some(id),
        ..fx.builder(config(100), &mock)
    });
    let events = run.turn_with("again").await;
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, Event::ContextCollected { .. })),
        "{events:?}"
    );
    let r = check_receipt(&events, FinishReason::Completed);
    assert_eq!(r.turn, 2);
    assert_eq!(r.saved.context_gc, 2 * freed, "{r:?}");
}

const MAIN: &str = "mock/main";
const SMALL: &str = "mock/small";

#[tokio::test]
async fn side_and_estimated_usage_through_the_engine() {
    let model = |id: &str, tier: &str, price: &str| {
        format!(
            "[models.\"{id}\"]\n{tier}context_window = 16000\nmax_output = 1000\n\
             parallel_tools = true\nreasoning = \"none\"\ncache = \"none\"\nimages = false\n\
             edit_format = \"str_replace\"\n[models.\"{id}\".price]\n{price}\n"
        )
    };
    let catalog = format!(
        "{}{}",
        model(MAIN, "", PRICE_TOML),
        model(
            SMALL,
            "tier = \"small\"\n",
            "input = 1.0\noutput = 5.0\ncache_read = 0.1\ncache_write = 1.25"
        )
    );
    let mut cfg = Config {
        model: ModelId::from(MAIN),
        catalog_overrides: Some(Catalog::from_toml(&catalog).unwrap()),
        ..Config::default()
    };
    cfg.permissions.mode = Mode::Yolo;
    let fx = Fx::new();
    // The reads report no usage (charged as estimates); the long prompt makes the fifth
    // step compact, with a summary from the side model.
    let mut script: Vec<MockTurn> = four_reads(&fx)
        .into_iter()
        .map(|t| match t {
            MockTurn::Events(evs) => MockTurn::Events(
                evs.into_iter()
                    .filter(|e| !matches!(e, ProviderEvent::Usage(_)))
                    .collect(),
            ),
            other => other,
        })
        .collect();
    script.push(text_step("done", usage(100, 10, 0, 0)));
    let main = MockProvider::new(script);
    let side = MockProvider::new(vec![text_step("SUMMARY-TEXT", usage(2000, 300, 0, 0))]);
    let mut run = launch(EngineBuilder {
        side_provider: Some(Arc::new(side.clone())),
        ..fx.builder(cfg, &main)
    });
    let prompt = format!("PROMPT {}", "lorem ipsum dolor sit amet ".repeat(1500));
    let events = run.turn_with(&prompt).await;
    assert!(
        events.iter().any(|e| matches!(e, Event::Compacted { .. })),
        "{events:?}"
    );
    let r = check_receipt(&events, FinishReason::Completed);
    assert_eq!(r.side_tokens, 2300, "{r:?}");
    assert!(r.estimated, "the reads' usage was estimated");
    // Each usage priced by its own model, summed.
    let costs: Vec<(UsageRole, f64)> = events
        .iter()
        .filter_map(|e| match e {
            Event::Usage { role, cost_usd, .. } => Some((*role, cost_usd.unwrap())),
            _ => None,
        })
        .collect();
    let side_cost: f64 = costs
        .iter()
        .filter(|(role, _)| *role == UsageRole::Side)
        .map(|(_, c)| c)
        .sum();
    assert!(
        close(side_cost, (2000.0 * 1.0 + 300.0 * 5.0) / 1e6),
        "{costs:?}"
    );
    let total: f64 = costs.iter().map(|(_, c)| c).sum();
    assert!(close(r.cost_usd.unwrap(), total), "{r:?} {costs:?}");
}

#[tokio::test]
async fn tool_calls_count_only_calls_that_ran() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![
        tools_step(
            vec![bash("b1", "true"), call("x1", "no_such_tool", json!({}))],
            usage(10, 1, 0, 0),
        ),
        text_step("done", usage(10, 1, 0, 0)),
    ]);
    let mut run = fx.start(config(100), &mock);
    let r = check_receipt(&run.turn_with("go").await, FinishReason::Completed);
    assert_eq!(r.tool_calls, 1, "{r:?}");
}

// ---- configuration -----------------------------------------------------------------------

#[test]
fn project_layer_cannot_set_receipts() {
    let dir = TempDir::new().unwrap();
    let user = dir.path().join("user").join("config.toml");
    let root = dir.path().join("project");
    std::fs::create_dir_all(user.parent().unwrap()).unwrap();
    std::fs::create_dir_all(root.join(".tautly")).unwrap();

    // Default: the footer is on.
    let loaded = Config::load_from(Some(&user), &root, CliOverrides::default()).unwrap();
    assert!(loaded.config.receipts.footer);

    // The project layer is ignored, with a warning that says why (not "unknown setting").
    std::fs::write(
        root.join(".tautly").join("config.toml"),
        "[receipts]\nfooter = false\n",
    )
    .unwrap();
    let loaded = Config::load_from(Some(&user), &root, CliOverrides::default()).unwrap();
    assert!(loaded.config.receipts.footer);
    assert!(
        loaded
            .warnings
            .iter()
            .any(|w| w.contains("`receipts`") && w.contains("may not set")),
        "{:?}",
        loaded.warnings
    );

    // The user layer sets it, and the command line overrides the user layer.
    std::fs::write(&user, "[receipts]\nfooter = false\n").unwrap();
    let loaded = Config::load_from(Some(&user), &root, CliOverrides::default()).unwrap();
    assert!(!loaded.config.receipts.footer);
    let cli = CliOverrides {
        receipts_footer: Some(true),
        ..CliOverrides::default()
    };
    assert!(
        Config::load_from(Some(&user), &root, cli)
            .unwrap()
            .config
            .receipts
            .footer
    );
}
