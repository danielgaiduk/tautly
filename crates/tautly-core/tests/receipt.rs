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
            ..
        } => Receipt {
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
    b.request(0);
    b.usage(UsageRole::Main, &usage(1000, 200, 300, 40), Some(0.01));
    b.usage(UsageRole::Side, &usage(500, 50, 10, 0), Some(0.002));
    b.request(0);
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
    b.request(0);
    b.request(3000);
    b.request(3000);
    let r = receipt(&b.finish(None, Instant::now()));
    assert_eq!(r.saved.context_gc, 6000);
    assert_eq!(r.steps, 3);
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
fn total_usd_prices_gc_at_cache_read_and_is_none_without_prices() {
    let run = |price: Option<&Price>| {
        let mut b = builder();
        b.request(0);
        b.request(10_000);
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

    fn start(&self, cfg: Config, mock: &MockProvider) -> Run {
        let (engine, events) = EngineBuilder {
            provider: Some(Arc::new(mock.clone())),
            data_home: Some(self.home.path().to_path_buf()),
            ..EngineBuilder::new(cfg, self.dir.path())
        }
        .build()
        .unwrap();
        Run { engine, events }
    }

    fn ledger(&self) -> Ledger {
        Ledger::open(&self.home.path().join("ledger.sqlite")).unwrap()
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
