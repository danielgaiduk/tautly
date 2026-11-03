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
        SideRunner {
            provider: Arc::new(mock.clone()),
            model: ModelId::from("mock/small"),
            usage: tx,
        },
        rx,
    )
}

#[tokio::test]
async fn the_runner_sends_one_bare_request_and_reports_its_usage() {
    let mock = MockProvider::new(vec![text_step("  the answer \n", usage(120, 8))]);
    let (side, mut rx) = runner(&mock);
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
    let tier = tier
        .map(|t| format!("tier = \"{t}\"\n"))
        .unwrap_or_default();
    let price = price
        .map(|p| format!("[models.\"{id}\".price]\n{p}\n"))
        .unwrap_or_default();
    format!(
        "[models.\"{id}\"]\n{tier}context_window = 16000\nmax_output = 1000\n\
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

    /// `(model, cost_usd, input, output)` of every ledger row, once there are `n`.
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
        let Ok(mut stmt) =
            conn.prepare("SELECT model, cost_usd, input, output FROM usage ORDER BY id")
        else {
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
    let main = MockProvider::new(compaction_script());
    let side = MockProvider::new(vec![text_step("SUMMARY-TEXT", usage(2000, 300))]);
    let mut cfg = config(Some(SMALL_PRICE));
    // The main model's steps report nothing but the last (110 tokens): only the side
    // request's 2,300 tokens can take the session past 80% of 2,800.
    cfg.budget.session = BudgetLimits {
        max_usd: None,
        max_tokens: Some(2800),
    };
    let mut run = start(fx.builder(cfg, &main, &side));
    let events = run.turn_with(&long_prompt()).await;
    assert_eq!(finish_reason(&events), FinishReason::Completed);

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
        .expect("a Usage with role side");
    let warn_at = events
        .iter()
        .position(|e| matches!(e, Event::Warning { message } if message.contains("session budget 82% used")))
        .unwrap_or_else(|| panic!("no 80% warning: {:?}", warnings(&events)));
    assert!(warn_at > usage_at, "{events:?}");
    // The main response's own usage came after the warning.
    assert!(events[warn_at..].iter().any(|e| matches!(
        e,
        Event::Usage {
            role: UsageRole::Main,
            ..
        }
    )));
    let rows = fx.ledger_rows(2).await;
    assert_eq!(rows.iter().filter(|r| r.0 == SMALL).count(), 1);
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
