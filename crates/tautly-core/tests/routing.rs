//! Side-model routing (spec 7.5, Task 27): which model does side work, where compaction
//! summaries and `web_fetch` answers go, and how side usage is priced, recorded and counted.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{Value, json};
use tautly_core::routing::{SideRunner, SideUsage, side_model_id};
use tautly_core::{
    BudgetLimits, Command, Config, Engine, EngineBuilder, Event, FinishReason, RoutingConfig,
    UsageRole,
};
use tautly_provider::{
    CanonicalRequest, Catalog, MockProvider, MockTurn, ModelId, ProviderError, ProviderEvent,
    RawUsage, SideModel, StopReason,
};
use tautly_sandbox::{
    Child, FileMeta, HttpResponse, LocalSandbox, Mode, Sandbox, SandboxError, SpawnSpec, Url,
};
use tempfile::TempDir;
use tokio::sync::mpsc;

const WAIT: Duration = Duration::from_secs(30);

// ---- which model -------------------------------------------------------------------

fn cfg_for(model: &str) -> Config {
    Config {
        model: ModelId::from(model),
        ..Config::default()
    }
}

#[test]
fn default_side_model_is_small_tier_of_same_provider() {
    let catalog = Catalog::builtin();
    for (main, side) in [
        ("anthropic/claude-sonnet-5-5", "anthropic/claude-haiku-5-5"),
        ("anthropic/claude-opus-5-5", "anthropic/claude-haiku-5-5"),
        ("openai/gpt-5", "openai/gpt-5-mini"),
        ("google/gemini-3-pro", "google/gemini-3-flash"),
    ] {
        assert_eq!(
            side_model_id(&cfg_for(main), &catalog),
            ModelId::from(side),
            "main {main}"
        );
    }
}

fn catalog_of(models: &[(&str, Option<&str>)]) -> Catalog {
    let mut toml = String::new();
    for (id, tier) in models {
        let tier = tier
            .map(|t| format!("tier = \"{t}\"\n"))
            .unwrap_or_default();
        toml.push_str(&format!(
            "[models.\"{id}\"]\n{tier}context_window = 100000\nmax_output = 4000\n\
             parallel_tools = true\nreasoning = \"none\"\ncache = \"none\"\nimages = false\n\
             edit_format = \"str_replace\"\n"
        ));
    }
    Catalog::from_toml(&toml).unwrap()
}

#[test]
fn falls_back_to_main_model_when_no_small_tier() {
    let catalog = catalog_of(&[("mock/big", None), ("mock/large", Some("large"))]);
    assert_eq!(
        side_model_id(&cfg_for("mock/big"), &catalog),
        ModelId::from("mock/big")
    );
    // A model the catalog does not know at all.
    assert_eq!(
        side_model_id(&cfg_for("mock/unknown"), &catalog),
        ModelId::from("mock/unknown")
    );
}

#[test]
fn small_tier_is_looked_up_within_the_main_models_provider() {
    // Another provider's small model is never picked: the key and the data policy are the
    // main provider's.
    let catalog = catalog_of(&[
        ("anthropic/haiku", Some("small")),
        ("compat:a/tiny", Some("small")),
        ("compat:b/huge", None),
    ]);
    assert_eq!(
        side_model_id(&cfg_for("openai/gpt"), &catalog),
        ModelId::from("openai/gpt")
    );
    // Two compat endpoints are two providers.
    assert_eq!(
        side_model_id(&cfg_for("compat:b/huge"), &catalog),
        ModelId::from("compat:b/huge")
    );
    assert_eq!(
        side_model_id(&cfg_for("compat:a/other"), &catalog),
        ModelId::from("compat:a/tiny")
    );
    // A main model that is small itself stays its own side model.
    let catalog = catalog_of(&[("mock/a", Some("small")), ("mock/b", Some("small"))]);
    assert_eq!(
        side_model_id(&cfg_for("mock/b"), &catalog),
        ModelId::from("mock/b")
    );
}

#[test]
fn config_side_model_overrides_default() {
    let mut cfg = cfg_for("anthropic/claude-sonnet-5-5");
    cfg.routing = RoutingConfig {
        side_model: Some(ModelId::from("openai/gpt-5-mini")),
    };
    assert_eq!(
        side_model_id(&cfg, &Catalog::builtin()),
        ModelId::from("openai/gpt-5-mini")
    );
}

// ---- the runner ----------------------------------------------------------------------

fn usage(input: u64, output: u64) -> RawUsage {
    RawUsage {
        input,
        output,
        cache_read: 0,
        cache_write: 0,
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

fn runner(mock: &MockProvider) -> (SideRunner, mpsc::Receiver<SideUsage>) {
    let (tx, rx) = mpsc::channel(8);
    (
        SideRunner::new(
            Arc::new(mock.clone()),
            ModelId::from("mock/small"),
            tx,
            16_000,
        ),
        rx,
    )
}

#[tokio::test]
async fn the_runner_sends_one_bare_request_and_reports_its_usage() {
    let mock = MockProvider::new(vec![text_step("  the answer \n", usage(120, 8))]);
    let (side, mut rx) = runner(&mock);
    // About three bytes a token of the 16,000-token window, less room for the answer.
    assert_eq!(side.max_input_bytes(), (16_000 - 2_500) * 3);
    let answer = side.complete("SYSTEM", "USER", 321).await.unwrap();
    assert_eq!(answer, "the answer");
    let reqs = mock.requests();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].model, ModelId::from("mock/small"));
    assert!(reqs[0].tools.is_empty());
    assert_eq!(reqs[0].reasoning, None);
    assert!(reqs[0].cache_breakpoints.is_empty());
    assert_eq!(reqs[0].max_output, 321);
    let text = serde_json::to_string(&reqs[0]).unwrap();
    assert!(text.contains("SYSTEM") && text.contains("USER"), "{text}");
    let reported = rx.try_recv().unwrap();
    assert_eq!(
        reported,
        SideUsage {
            usage: usage(120, 8),
            estimated: false
        }
    );
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn the_runner_reports_what_a_broken_request_streamed_and_returns_the_error() {
    let mock = MockProvider::new(vec![MockTurn::DropAfter(vec![ProviderEvent::TextDelta(
        "partial answer".into(),
    )])]);
    let (side, mut rx) = runner(&mock);
    let err = side.complete("S", "U", 100).await.unwrap_err();
    assert!(err.is_transient(), "{err:?}");
    let reported = rx.try_recv().unwrap();
    assert!(reported.estimated);
    assert!(reported.usage.output > 0 && reported.usage.input > 0);

    // Nothing streamed, nothing billed: an error before any content reports nothing.
    let mock = MockProvider::new(vec![MockTurn::Error(ProviderError::Auth("no key".into()))]);
    let (side, mut rx) = runner(&mock);
    assert!(matches!(
        side.complete("S", "U", 100).await,
        Err(ProviderError::Auth(_))
    ));
    assert!(rx.try_recv().is_err());
}

// ---- the engine ------------------------------------------------------------------------

const MAIN: &str = "mock/main";
const SMALL: &str = "mock/small";
const MAIN_PRICE: &str = "input = 3.0\noutput = 15.0\ncache_read = 0.3\ncache_write = 3.75";
const SMALL_PRICE: &str = "input = 1.0\noutput = 5.0\ncache_read = 0.1\ncache_write = 1.25";

fn model_toml(id: &str, tier: Option<&str>, price: Option<&str>) -> String {
    sized_toml(id, tier, price, 16000, 1000)
}

fn sized_toml(
    id: &str,
    tier: Option<&str>,
    price: Option<&str>,
    window: u32,
    max_output: u32,
) -> String {
    let tier = tier
        .map(|t| format!("tier = \"{t}\"\n"))
        .unwrap_or_default();
    let price = price
        .map(|p| format!("[models.\"{id}\".price]\n{p}\n"))
        .unwrap_or_default();
    format!(
        "[models.\"{id}\"]\n{tier}context_window = {window}\nmax_output = {max_output}\n\
         parallel_tools = true\nreasoning = \"none\"\ncache = \"none\"\nimages = false\n\
         edit_format = \"str_replace\"\n{price}"
    )
}

/// A main model and a small one of the same provider, priced as given.
fn config(small_price: Option<&str>) -> Config {
    let toml = format!(
        "{}{}",
        model_toml(MAIN, None, Some(MAIN_PRICE)),
        model_toml(SMALL, Some("small"), small_price)
    );
    let mut c = Config {
        model: ModelId::from(MAIN),
        catalog_overrides: Some(Catalog::from_toml(&toml).unwrap()),
        ..Config::default()
    };
    c.permissions.mode = Mode::Yolo;
    c
}

fn call(id: &str, name: &str, args: Value) -> ProviderEvent {
    ProviderEvent::ToolCall {
        id: id.into(),
        name: name.into(),
        input_json: args.to_string(),
    }
}

/// A response with tool calls; it reports no usage (the mock's numbers would calibrate the
/// token estimator far off).
fn tools_step(calls: Vec<ProviderEvent>) -> MockTurn {
    let mut evs = calls;
    evs.push(ProviderEvent::Done {
        stop: StopReason::ToolUse,
    });
    MockTurn::Events(evs)
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

    fn builder(&self, cfg: Config, main: &MockProvider, side: &MockProvider) -> EngineBuilder {
        EngineBuilder {
            provider: Some(Arc::new(main.clone())),
            side_provider: Some(Arc::new(side.clone())),
            data_home: Some(self.home.path().to_path_buf()),
            ..EngineBuilder::new(cfg, self.dir.path())
        }
    }

    /// `(model, cost_usd, input, output)` of every ledger row of reported usage (the
    /// estimates for the main steps that report none are left out), once there are `n`.
    async fn ledger_rows(&self, n: usize) -> Vec<(String, Option<f64>, u64, u64)> {
        let end = Instant::now() + WAIT;
        loop {
            let rows = self.read_ledger();
            if rows.len() >= n {
                assert_eq!(rows.len(), n, "{rows:?}");
                return rows;
            }
            assert!(Instant::now() < end, "the ledger never reached {n} rows");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn read_ledger(&self) -> Vec<(String, Option<f64>, u64, u64)> {
        let path = self.home.path().join("ledger.sqlite");
        let Ok(conn) =
            rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        else {
            return Vec::new();
        };
        let Ok(mut stmt) = conn.prepare(
            "SELECT model, cost_usd, input, output FROM usage WHERE estimated = 0 ORDER BY id",
        ) else {
            return Vec::new();
        };
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }
}

struct Run {
    engine: Engine,
    events: mpsc::Receiver<Event>,
}

fn start(b: EngineBuilder) -> Run {
    let (engine, events) = b.build().unwrap();
    Run { engine, events }
}

impl Run {
    /// Events through `TurnFinished`.
    async fn turn_with(&mut self, text: &str) -> Vec<Event> {
        self.engine
            .send(Command::SendMessage { text: text.into() })
            .await
            .unwrap();
        let mut out = Vec::new();
        loop {
            let ev = tokio::time::timeout(WAIT, self.events.recv())
                .await
                .expect("timed out waiting for an event")
                .expect("event channel closed");
            let done = matches!(ev, Event::TurnFinished { .. });
            out.push(ev);
            if done {
                return out;
            }
        }
    }
}

fn finish_reason(events: &[Event]) -> FinishReason {
    match events.last() {
        Some(Event::TurnFinished { reason, .. }) => *reason,
        other => panic!("turn did not finish: {other:?}"),
    }
}

fn side_usages(events: &[Event]) -> Vec<(u64, u64, Option<f64>)> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::Usage {
                role: UsageRole::Side,
                tokens_in,
                tokens_out,
                cost_usd,
                ..
            } => Some((*tokens_in, *tokens_out, *cost_usd)),
            _ => None,
        })
        .collect()
}

fn warnings(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::Warning { message } => Some(message.clone()),
            _ => None,
        })
        .collect()
}

/// A long first prompt and four reads in a small window: compaction happens at the fifth
/// step, summarizing the first turn.
fn compaction_script() -> Vec<MockTurn> {
    let mut script: Vec<MockTurn> = (1..=4)
        .map(|i| {
            tools_step(vec![call(
                &format!("r{i}"),
                "read",
                json!({"path": format!("f{i}.txt")}),
            )])
        })
        .collect();
    script.push(text_step("done", usage(100, 10)));
    script
}

fn long_prompt() -> String {
    format!(
        "PROMPT-MARKER {}",
        "lorem ipsum dolor sit amet ".repeat(1500)
    )
}

fn compaction_fx() -> Fx {
    let fx = Fx::new();
    for i in 1..=4 {
        std::fs::write(
            fx.dir.path().join(format!("f{i}.txt")),
            format!("file {i}\n"),
        )
        .unwrap();
    }
    fx
}

fn request_text(req: &CanonicalRequest) -> String {
    serde_json::to_string(&req.messages).unwrap()
}

#[tokio::test]
async fn compaction_summary_requested_from_side_model_not_main() {
    let fx = compaction_fx();
    let main = MockProvider::new(compaction_script());
    let side = MockProvider::new(vec![text_step("SUMMARY-TEXT", usage(2000, 300))]);
    let mut run = start(fx.builder(config(Some(SMALL_PRICE)), &main, &side));
    let events = run.turn_with(&long_prompt()).await;
    assert_eq!(
        finish_reason(&events),
        FinishReason::Completed,
        "{events:?}"
    );

    // The side provider got exactly the summary request, for the small model, bare.
    let side_reqs = side.requests();
    assert_eq!(side_reqs.len(), 1);
    assert_eq!(side_reqs[0].model, ModelId::from(SMALL));
    assert!(side_reqs[0].tools.is_empty());
    assert_eq!(side_reqs[0].reasoning, None);
    assert!(request_text(&side_reqs[0]).contains("PROMPT-MARKER"));

    // The main provider never saw a summary request: five steps, all for the main model.
    let main_reqs = main.requests();
    assert_eq!(main_reqs.len(), 5);
    assert!(main_reqs.iter().all(|r| r.model == ModelId::from(MAIN)));
    assert!(
        main_reqs
            .iter()
            .all(|r| !request_text(r).contains("<transcript>")),
        "a summary request went to the main model"
    );
    // The summary is in the history of the step that follows.
    assert!(request_text(&main_reqs[4]).contains("SUMMARY-TEXT"));
    assert!(!request_text(&main_reqs[4]).contains("PROMPT-MARKER"));
    assert!(events.iter().any(
        |e| matches!(e, Event::Compacted { removed_turns: 1, summary, .. } if summary == "SUMMARY-TEXT")
    ));
}

/// The tokens and cost of the `Usage` events of `role`.
fn spent(events: &[Event], role: UsageRole) -> (u64, f64) {
    events
        .iter()
        .filter_map(|e| match e {
            Event::Usage {
                role: r,
                tokens_in,
                tokens_out,
                cost_usd,
                ..
            } if *r == role => Some((tokens_in + tokens_out, cost_usd.unwrap_or(0.0))),
            _ => None,
        })
        .fold((0, 0.0), |(t, c), (t2, c2)| (t + t2, c + c2))
}

/// Make the cost ledger unusable: resume then restores the spend from the log alone.
fn break_ledger(fx: &Fx) {
    let ledger = fx.home.path().join("ledger.sqlite");
    let _ = std::fs::remove_file(&ledger);
    for ext in ["-wal", "-shm"] {
        let mut p = ledger.clone().into_os_string();
        p.push(ext);
        let _ = std::fs::remove_file(PathBuf::from(p));
    }
    std::fs::create_dir(&ledger).unwrap();
}

/// Review C M2: side-model usage is in the session log (not only the ledger), and a
/// resume without the ledger prices each logged usage by the model it was spent on.
#[tokio::test]
async fn a_resume_without_the_ledger_counts_side_usage_priced_by_its_own_model() {
    let fx = compaction_fx();
    let main = MockProvider::new(compaction_script());
    let side = MockProvider::new(vec![text_step("SUMMARY-TEXT", usage(2000, 300))]);
    let mut run = start(fx.builder(config(Some(SMALL_PRICE)), &main, &side));
    let id = run.engine.session_id().to_string();
    let events = run.turn_with(&long_prompt()).await;
    assert_eq!(finish_reason(&events), FinishReason::Completed);
    let (main_tokens, main_usd) = spent(&events, UsageRole::Main);
    let (side_tokens, side_usd) = spent(&events, UsageRole::Side);
    assert_eq!(side_tokens, 2300);
    run.engine.send(Command::Shutdown).await.unwrap();
    while run.events.recv().await.is_some() {}
    break_ledger(&fx);

    let resume = |mock: &MockProvider| EngineBuilder {
        resume: Some(id.clone()),
        ..fx.builder(config(Some(SMALL_PRICE)), mock, &MockProvider::new(vec![]))
    };
    let budget = |max_usd: Option<f64>, max_tokens: Option<u64>| Command::SetBudget {
        scope: tautly_core::BudgetScope::Session,
        max_usd,
        max_tokens,
    };
    // Tokens: the main steps' alone are within the limit, with the side request's not.
    let mock = MockProvider::new(vec![text_step("never sent", usage(1, 1))]);
    let mut run = start(resume(&mock));
    run.engine
        .send(budget(None, Some(main_tokens + 1)))
        .await
        .unwrap();
    let events = run.turn_with("again").await;
    assert_eq!(finish_reason(&events), FinishReason::Budget, "{events:?}");
    assert!(mock.requests().is_empty());
    run.engine.send(Command::Shutdown).await.unwrap();
    while run.events.recv().await.is_some() {}

    // Cost: just above what was spent, each response at its own model's price (the side
    // request priced as the main model would be past it).
    let mock = MockProvider::new(vec![text_step("sent", usage(1, 1))]);
    let mut run = start(resume(&mock));
    let limit = (main_usd + side_usd) * 1.01;
    run.engine.send(budget(Some(limit), None)).await.unwrap();
    let events = run.turn_with("again").await;
    assert_eq!(
        finish_reason(&events),
        FinishReason::Completed,
        "{events:?}"
    );
    assert_eq!(mock.requests().len(), 1);
    run.engine.send(Command::Shutdown).await.unwrap();
    while run.events.recv().await.is_some() {}
}

#[tokio::test]
async fn side_usage_priced_and_recorded_with_side_model_id() {
    let fx = compaction_fx();
    let main = MockProvider::new(compaction_script());
    let side = MockProvider::new(vec![text_step("SUMMARY-TEXT", usage(2000, 300))]);
    let mut run = start(fx.builder(config(Some(SMALL_PRICE)), &main, &side));
    let events = run.turn_with(&long_prompt()).await;
    assert_eq!(finish_reason(&events), FinishReason::Completed);

    // 2000 in + 300 out at the small model's 1.0 / 5.0 (the main model's would be 0.0105).
    let expect = 2000.0 * 1.0 / 1e6 + 300.0 * 5.0 / 1e6;
    let sides = side_usages(&events);
    assert_eq!(sides.len(), 1, "{sides:?}");
    assert_eq!((sides[0].0, sides[0].1), (2000, 300));
    assert!((sides[0].2.unwrap() - expect).abs() < 1e-12, "{sides:?}");

    let rows = fx.ledger_rows(2).await;
    let side_row = rows.iter().find(|r| r.0 == SMALL).expect("a side row");
    assert_eq!((side_row.2, side_row.3), (2000, 300));
    assert!((side_row.1.unwrap() - expect).abs() < 1e-12, "{rows:?}");
    let main_row = rows.iter().find(|r| r.0 == MAIN).expect("a main row");
    assert_eq!((main_row.2, main_row.3), (100, 10));
    assert!(
        (main_row.1.unwrap() - (100.0 * 3.0 + 10.0 * 15.0) / 1e6).abs() < 1e-12,
        "{rows:?}"
    );
}

#[tokio::test]
async fn an_unpriced_side_model_has_no_cost_and_the_usd_budget_warns_once() {
    let fx = compaction_fx();
    let main = MockProvider::new(compaction_script());
    let side = MockProvider::new(vec![text_step("SUMMARY-TEXT", usage(2000, 300))]);
    let mut cfg = config(None);
    cfg.budget.session = BudgetLimits {
        max_usd: Some(100.0),
        max_tokens: None,
    };
    let mut run = start(fx.builder(cfg, &main, &side));
    let events = run.turn_with(&long_prompt()).await;
    assert_eq!(finish_reason(&events), FinishReason::Completed);

    let sides = side_usages(&events);
    assert_eq!(sides.len(), 1);
    assert_eq!(sides[0].2, None, "no price, no cost");
    let rows = fx.ledger_rows(2).await;
    let side_row = rows.iter().find(|r| r.0 == SMALL).unwrap();
    assert_eq!(side_row.1, None);

    let w: Vec<String> = warnings(&events)
        .into_iter()
        .filter(|w| w.contains(SMALL))
        .collect();
    assert_eq!(w.len(), 1, "{w:?}");
    assert!(w[0].contains("no known price"), "{}", w[0]);
}

#[tokio::test]
async fn side_usage_has_role_side_and_counts_toward_budget_and_ledger() {
    let fx = compaction_fx();
    let mut script = compaction_script();
    script.push(text_step("never sent", usage(1, 1)));
    let main = MockProvider::new(script);
    let side = MockProvider::new(vec![text_step("SUMMARY-TEXT", usage(2000, 300))]);
    let mut run = start(fx.builder(config(Some(SMALL_PRICE)), &main, &side));
    let events = run.turn_with(&long_prompt()).await;
    assert_eq!(finish_reason(&events), FinishReason::Completed);
    let tokens = |role: UsageRole| -> u64 {
        events
            .iter()
            .filter_map(|e| match e {
                Event::Usage {
                    role: r,
                    tokens_in,
                    tokens_out,
                    ..
                } if *r == role => Some(tokens_in + tokens_out),
                _ => None,
            })
            .sum()
    };
    assert_eq!(tokens(UsageRole::Side), 2300);
    // The main steps' tokens alone are within this limit; with the side request's they
    // are past it, so the next turn sends nothing.
    run.engine
        .send(Command::SetBudget {
            scope: tautly_core::BudgetScope::Session,
            max_usd: None,
            max_tokens: Some(tokens(UsageRole::Main) + 1),
        })
        .await
        .unwrap();
    let next = run.turn_with("again").await;
    assert_eq!(finish_reason(&next), FinishReason::Budget, "{next:?}");
    assert_eq!(main.requests().len(), 5);
    let rows = fx.ledger_rows(2).await;
    assert_eq!(rows.iter().filter(|r| r.0 == SMALL).count(), 1);
}

/// Index of the first `Usage` of `role` in `events`.
fn first_usage(events: &[Event], role: UsageRole) -> Option<usize> {
    events
        .iter()
        .position(|e| matches!(e, Event::Usage { role: r, .. } if *r == role))
}

/// The 80% warning a side request causes follows that request's `Usage` (re-review N7:
/// the property `charge()` documents). The limit is sized from a first run of the same
/// script, so that the main steps before the side request stay below 80% and the side
/// request's 2,300 tokens take the session past it (with 1,150 tokens of slack either
/// way for estimate noise between the runs).
#[tokio::test]
async fn the_80_percent_warning_a_side_request_causes_follows_its_usage() {
    let fx = compaction_fx();
    let run_once = |max_tokens: Option<u64>| {
        let main = MockProvider::new(compaction_script());
        let side = MockProvider::new(vec![text_step("SUMMARY-TEXT", usage(2000, 300))]);
        let mut cfg = config(Some(SMALL_PRICE));
        cfg.budget.session = BudgetLimits {
            max_usd: None,
            max_tokens,
        };
        let mut run = start(fx.builder(cfg, &main, &side));
        async move {
            let events = run.turn_with(&long_prompt()).await;
            run.engine.send(Command::Shutdown).await.unwrap();
            while run.events.recv().await.is_some() {}
            events
        }
    };
    let measured = run_once(None).await;
    let side_at = first_usage(&measured, UsageRole::Side).expect("a side request");
    let (main_before, _) = spent(&measured[..side_at], UsageRole::Main);
    let limit = (main_before + 1150) * 5 / 4;

    let events = run_once(Some(limit)).await;
    let side_at = first_usage(&events, UsageRole::Side).expect("a side request");
    let warn_at = events
        .iter()
        .position(|e| {
            matches!(e, Event::Warning { message }
                if message.starts_with("session budget") && message.contains("% used"))
        })
        .unwrap_or_else(|| panic!("no 80% warning: {:?}", warnings(&events)));
    assert!(warn_at > side_at, "{events:?}");
}

#[tokio::test]
async fn without_a_small_tier_the_main_model_summarizes() {
    let fx = compaction_fx();
    // Same provider mock, no small model in the catalog: summary and steps share one script.
    let mut script: Vec<MockTurn> = compaction_script();
    script.insert(4, text_step("SUMMARY-TEXT", usage(2000, 300)));
    let main = MockProvider::new(script);
    let unused = MockProvider::new(vec![]);
    let mut cfg = config(Some(SMALL_PRICE));
    cfg.catalog_overrides =
        Some(Catalog::from_toml(&model_toml(MAIN, None, Some(MAIN_PRICE))).unwrap());
    let mut run = start(fx.builder(cfg, &main, &unused));
    let events = run.turn_with(&long_prompt()).await;
    assert_eq!(
        finish_reason(&events),
        FinishReason::Completed,
        "{events:?}"
    );
    assert!(unused.requests().is_empty());
    let reqs = main.requests();
    assert_eq!(reqs.len(), 6);
    assert_eq!(reqs[4].model, ModelId::from(MAIN));
    let rows = fx.ledger_rows(2).await;
    assert!(rows.iter().all(|r| r.0 == MAIN), "{rows:?}");
}

#[tokio::test]
async fn an_unusable_side_model_falls_back_to_the_main_model_with_a_warning() {
    let fx = compaction_fx();
    let mut script = compaction_script();
    script.insert(4, text_step("SUMMARY-TEXT", usage(2000, 300)));
    let main = MockProvider::new(script);
    let mut cfg = config(Some(SMALL_PRICE));
    // No adapter exists for this provider, so none can be built (no key lookup is made).
    cfg.routing.side_model = Some(ModelId::from("acme/tiny"));
    let mut run = start(EngineBuilder {
        provider: Some(Arc::new(main.clone())),
        data_home: Some(fx.home.path().to_path_buf()),
        ..EngineBuilder::new(cfg, fx.dir.path())
    });
    let events = run.turn_with(&long_prompt()).await;
    assert_eq!(
        finish_reason(&events),
        FinishReason::Completed,
        "{events:?}"
    );
    let w = warnings(&events);
    assert!(
        w.iter()
            .any(|w| w.contains("acme/tiny") && w.contains("main model")),
        "{w:?}"
    );
    assert_eq!(main.requests().len(), 6);
    assert_eq!(main.requests()[4].model, ModelId::from(MAIN));
}

#[tokio::test]
async fn compaction_is_sized_for_the_side_models_window_and_output_cap() {
    let fx = compaction_fx();
    // The side model has the smaller window (14,000 against 16,000) and a big output cap.
    let toml = format!(
        "{}{}",
        sized_toml(MAIN, None, Some(MAIN_PRICE), 16_000, 1_000),
        sized_toml(SMALL, Some("small"), Some(SMALL_PRICE), 14_000, 6_000)
    );
    let mut cfg = config(Some(SMALL_PRICE));
    cfg.catalog_overrides = Some(Catalog::from_toml(&toml).unwrap());
    cfg.compaction_threshold = Some(0.6);
    let main = MockProvider::new(compaction_script());
    let side = MockProvider::new(vec![text_step("SUMMARY-TEXT", usage(2000, 300))]);
    let mut run = start(fx.builder(cfg, &main, &side));
    let prompt = format!(
        "PROMPT-MARKER {}",
        "lorem ipsum dolor sit amet ".repeat(1200)
    );
    let events = run.turn_with(&prompt).await;
    assert_eq!(
        finish_reason(&events),
        FinishReason::Completed,
        "{events:?}"
    );
    let reqs = side.requests();
    assert_eq!(reqs.len(), 1, "{events:?}");
    // min(side max_output, the 4k summary cap), not the main model's 1,000.
    assert_eq!(reqs[0].max_output, 4_096);
    // The transcript and the reply fit the side model's window (4 bytes a token, loosely).
    let sent = serde_json::to_string(&reqs[0]).unwrap().len() as u64 / 4;
    assert!(sent + 4_096 <= 14_000, "{sent} tokens of request");
}

#[tokio::test]
async fn a_failing_side_model_falls_back_to_the_main_model_once_and_stays_there() {
    let fx = compaction_fx();
    let mut script = compaction_script();
    script.insert(4, text_step("SUMMARY-TEXT", usage(2000, 300)));
    let main = MockProvider::new(script);
    let side = MockProvider::new(vec![MockTurn::Error(ProviderError::Auth("bad key".into()))]);
    let mut run = start(fx.builder(config(Some(SMALL_PRICE)), &main, &side));
    let events = run.turn_with(&long_prompt()).await;
    assert_eq!(
        finish_reason(&events),
        FinishReason::Completed,
        "{events:?}"
    );
    // The side model was tried once; then the main model wrote the summary.
    assert_eq!(side.requests().len(), 1);
    let reqs = main.requests();
    assert_eq!(reqs.len(), 6, "{events:?}");
    assert!(request_text(&reqs[4]).contains("<transcript>"));
    assert_eq!(reqs[4].model, ModelId::from(MAIN));
    assert!(reqs[4].tools.is_empty());
    assert!(request_text(&reqs[5]).contains("SUMMARY-TEXT"));
    let w: Vec<String> = warnings(&events)
        .into_iter()
        .filter(|w| w.contains(SMALL))
        .collect();
    assert_eq!(w.len(), 1, "{w:?}");
    assert!(
        w[0].contains("main model") && w[0].contains("bad key"),
        "{}",
        w[0]
    );
    // The summary is billed to the main model.
    let rows = fx.ledger_rows(2).await;
    assert!(
        rows.iter().all(|r| r.0 == MAIN),
        "a refused request bills nothing: {rows:?}"
    );
    assert!(
        rows.iter()
            .any(|r| r.0 == MAIN && r.2 == 2000 && r.3 == 300),
        "{rows:?}"
    );

    // Sticky: a later web_fetch question goes straight to the main model (no second try of
    // the side provider), and no second warning.
    let fx = Fx::new();
    let main = MockProvider::new(vec![
        tools_step(vec![call(
            "w1",
            "web_fetch",
            json!({"url": "https://docs.example.test/a", "question": "first?"}),
        )]),
        tools_step(vec![call(
            "w2",
            "web_fetch",
            json!({"url": "https://docs.example.test/b", "question": "second?"}),
        )]),
        text_step("ANSWER-FROM-MAIN", usage(300, 12)),
        text_step("done", usage(100, 10)),
    ]);
    let side = MockProvider::new(vec![MockTurn::Error(ProviderError::Auth("bad key".into()))]);
    let sandbox: Arc<dyn Sandbox> = Arc::new(Web(LocalSandbox::new(fx.root()).unwrap()));
    let mut run = start(EngineBuilder {
        sandbox: Some(sandbox),
        ..fx.builder(config(Some(SMALL_PRICE)), &main, &side)
    });
    let events = run.turn_with("go").await;
    assert_eq!(
        finish_reason(&events),
        FinishReason::Completed,
        "{events:?}"
    );
    assert_eq!(
        side.requests().len(),
        1,
        "the side provider was tried again"
    );
    let outputs: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            Event::ToolFinished { output, .. } => Some(output.clone()),
            _ => None,
        })
        .collect();
    assert!(outputs[0].contains("could not be answered"), "{outputs:?}");
    assert!(outputs[1].contains("ANSWER-FROM-MAIN"), "{outputs:?}");
    let answer_req = &main.requests()[2];
    assert!(answer_req.tools.is_empty());
    assert_eq!(
        warnings(&events)
            .iter()
            .filter(|w| w.contains(SMALL))
            .count(),
        1
    );
}

// ---- web_fetch through the engine ------------------------------------------------------

/// A sandbox whose network is a canned page.
struct Web(LocalSandbox);

#[async_trait]
impl Sandbox for Web {
    fn root(&self) -> &std::path::Path {
        self.0.root()
    }
    fn resolve(&self, p: &std::path::Path) -> Result<PathBuf, SandboxError> {
        self.0.resolve(p)
    }
    async fn read(&self, p: &std::path::Path) -> Result<Vec<u8>, SandboxError> {
        self.0.read(p).await
    }
    async fn metadata(&self, p: &std::path::Path) -> Result<FileMeta, SandboxError> {
        self.0.metadata(p).await
    }
    async fn write_atomic(
        &self,
        p: &std::path::Path,
        bytes: &[u8],
        mode: Option<u32>,
    ) -> Result<(), SandboxError> {
        self.0.write_atomic(p, bytes, mode).await
    }
    async fn remove_file(&self, p: &std::path::Path) -> Result<(), SandboxError> {
        self.0.remove_file(p).await
    }
    async fn spill(&self, bytes: &[u8]) -> Result<PathBuf, SandboxError> {
        self.0.spill(bytes).await
    }
    async fn spawn(&self, spec: SpawnSpec) -> Result<Child, SandboxError> {
        self.0.spawn(spec).await
    }
    fn walker(&self, base: &std::path::Path) -> Result<ignore::WalkBuilder, SandboxError> {
        self.0.walker(base)
    }
    async fn http_get(&self, _url: &Url) -> Result<HttpResponse, SandboxError> {
        Ok(HttpResponse {
            status: 200,
            headers: vec![("content-type".into(), "text/plain".into())],
            body: b"PAGE-BODY: the limit is 42 requests per minute.".to_vec(),
        })
    }
}

#[tokio::test]
async fn web_fetch_question_goes_to_the_side_model_and_its_usage_is_counted() {
    let fx = Fx::new();
    let main = MockProvider::new(vec![
        tools_step(vec![call(
            "w1",
            "web_fetch",
            json!({"url": "https://docs.example.test/limits", "question": "What is the limit?"}),
        )]),
        text_step("done", usage(100, 10)),
    ]);
    let side = MockProvider::new(vec![text_step("42 per minute.", usage(300, 12))]);
    let sandbox: Arc<dyn Sandbox> = Arc::new(Web(LocalSandbox::new(fx.root()).unwrap()));
    let mut run = start(EngineBuilder {
        sandbox: Some(sandbox),
        ..fx.builder(config(Some(SMALL_PRICE)), &main, &side)
    });
    let events = run.turn_with("how many requests are allowed?").await;
    assert_eq!(
        finish_reason(&events),
        FinishReason::Completed,
        "{events:?}"
    );

    // The model got the answer and the URL, not the page.
    let result = events
        .iter()
        .find_map(|e| match e {
            Event::ToolFinished { output, .. } => Some(output.clone()),
            _ => None,
        })
        .expect("a ToolFinished");
    assert!(result.contains("42 per minute."), "{result}");
    assert!(
        result.contains("https://docs.example.test/limits"),
        "{result}"
    );
    assert!(!result.contains("PAGE-BODY"), "{result}");

    // The side model saw the page and the question, and nothing else.
    let reqs = side.requests();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].model, ModelId::from(SMALL));
    assert!(reqs[0].tools.is_empty());
    assert_eq!(reqs[0].reasoning, None);
    let text = request_text(&reqs[0]);
    assert!(
        text.contains("PAGE-BODY") && text.contains("What is the limit?"),
        "{text}"
    );

    // Its usage is a Side Usage priced at the small model's rates, before the call's result
    // is shown, and has a ledger row under the small model's id.
    let sides = side_usages(&events);
    assert_eq!(sides.len(), 1, "{sides:?}");
    let expect = 300.0 * 1.0 / 1e6 + 12.0 * 5.0 / 1e6;
    assert!((sides[0].2.unwrap() - expect).abs() < 1e-12, "{sides:?}");
    let usage_at = events
        .iter()
        .position(|e| {
            matches!(
                e,
                Event::Usage {
                    role: UsageRole::Side,
                    ..
                }
            )
        })
        .unwrap();
    let finished_at = events
        .iter()
        .position(|e| matches!(e, Event::ToolFinished { .. }))
        .unwrap();
    assert!(usage_at < finished_at);
    let rows = fx.ledger_rows(2).await;
    assert!(
        rows.iter().any(|r| r.0 == SMALL && r.2 == 300 && r.3 == 12),
        "{rows:?}"
    );
}
