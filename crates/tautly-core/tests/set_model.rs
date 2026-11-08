//! Mid-session model switch (spec 7.11.5): `Command::SetModel` against scripted providers
//! (`MockProvider`) built by an injected provider factory, so no test reads a real key.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tautly_core::record::session_model;
use tautly_core::{
    Command, Config, Engine, EngineBuilder, ErrorKind, Event, FinishReason, MutationReason,
    ProviderFactory, ProviderSetupError, RoutingConfig, SessionRecord, UsageRole, provider_label,
};
use tautly_provider::mock::{call, tools_step};
use tautly_provider::{
    CanonicalRequest, Catalog, Content, MockProvider, MockTurn, ModelId, Provider, ProviderError,
    ProviderEvent, RawUsage, StopReason,
};
use tautly_sandbox::Mode;
use tautly_session::SessionReader;
use tautly_testkit::{Fx, Run, WAIT, start};

/// A catalog entry: explicit prompt cache, `price_input` USD per million input tokens
/// (`None`: unpriced).
fn entry(id: &str, window: u32, edit_format: &str, price_input: Option<f64>) -> String {
    let mut s = format!(
        r#"[models."{id}"]
context_window = {window}
max_output = 4000
parallel_tools = true
reasoning = "budget"
cache = "explicit"
images = false
edit_format = "{edit_format}"
"#
    );
    if let Some(p) = price_input {
        s.push_str(&format!(
            "[models.\"{id}\".price]\ninput = {p}\noutput = {p}\ncache_read = 0.3\ncache_write = 3.75\n"
        ));
    }
    s
}

/// `entry` marked `tier = "small"` (a side-model candidate, `routing.rs`).
fn small(entry: String) -> String {
    entry.replace(
        "max_output = 4000\n",
        "max_output = 4000\ntier = \"small\"\n",
    )
}

fn test_catalog() -> Catalog {
    let toml = [
        entry("anthropic/a1", 200_000, "str_replace", Some(3.0)),
        // Anthropic's first small model in id order: a1's side model without routing.
        small(entry(
            "anthropic/a0-side",
            200_000,
            "str_replace",
            Some(3.0),
        )),
        // Small itself, so without routing it is its own side model.
        small(entry("anthropic/a2", 32_000, "str_replace", Some(3.0))),
        // A side model of Anthropic's: a short summary cap and a high price.
        entry("anthropic/side", 200_000, "str_replace", Some(1000.0))
            .replace("max_output = 4000", "max_output = 1000"),
        entry("openai/o1", 200_000, "patch", Some(3.0)),
        entry("openai/small", 128_000, "patch", Some(3.0)),
        small(entry("openai/tiny", 32_000, "patch", Some(3.0))),
        // OpenAI's first small model in id order (before the built-in `openai/gpt-5-mini`):
        // the side model of any OpenAI main model that is not small itself. A short summary
        // cap and a high price.
        small(entry("openai/a-side", 200_000, "patch", Some(1000.0)))
            .replace("max_output = 4000", "max_output = 1000"),
        entry("openai/mini", 32_000, "patch", Some(3.0)),
        entry("openai/free", 200_000, "patch", None),
        entry("anthropic/pricey", 200_000, "str_replace", Some(1000.0)),
    ]
    .concat();
    Catalog::from_toml(&toml).unwrap()
}

fn config(model: &str) -> Config {
    let mut c = Config {
        model: ModelId::from(model),
        catalog_overrides: Some(test_catalog()),
        ..Config::default()
    };
    c.permissions.mode = Mode::Ask;
    c
}

fn m(id: &str) -> ModelId {
    ModelId::from(id)
}

/// A text answer without usage.
fn reply(text: &str) -> MockTurn {
    MockTurn::Events(vec![
        ProviderEvent::TextDelta(text.into()),
        ProviderEvent::Done {
            stop: StopReason::EndTurn,
        },
    ])
}

/// A text answer billing `input` tokens.
fn billed(text: &str, input: u64) -> MockTurn {
    MockTurn::Events(vec![
        ProviderEvent::TextDelta(text.into()),
        ProviderEvent::Usage(RawUsage {
            input,
            output: 0,
            cache_read: 0,
            cache_write: 0,
        }),
        ProviderEvent::Done {
            stop: StopReason::EndTurn,
        },
    ])
}

/// Every model gets the same scripted provider.
fn factory(mock: &MockProvider) -> ProviderFactory {
    let mock = mock.clone();
    Arc::new(move |_: &ModelId| Ok(Arc::new(mock.clone()) as Arc<dyn Provider>))
}

trait FxExt {
    fn switching_builder(&self, cfg: Config, mock: &MockProvider) -> EngineBuilder;
}

impl FxExt for Fx {
    /// The mock serves the main model, the side model and every switch, so no test looks
    /// up a key (the OS keychain can stall a lookup).
    fn switching_builder(&self, cfg: Config, mock: &MockProvider) -> EngineBuilder {
        EngineBuilder {
            provider: Some(Arc::new(mock.clone())),
            side_provider: Some(Arc::new(mock.clone())),
            provider_factory: Some(factory(mock)),
            data_home: Some(self.home.path().to_path_buf()),
            ..EngineBuilder::new(cfg, self.dir.path())
        }
    }
}

trait RunExt {
    async fn turn(&mut self) -> Vec<Event>;
    async fn turn_from_start(&mut self, text: &str) -> Vec<Event>;
    async fn set_model(&mut self, model: &str) -> Vec<Event>;
}

impl RunExt for Run {
    /// Events through `TurnFinished`, from `TurnStarted`.
    async fn turn(&mut self) -> Vec<Event> {
        let all = self
            .until(|e| matches!(e, Event::TurnFinished { .. }))
            .await;
        let from = all
            .iter()
            .position(|e| matches!(e, Event::TurnStarted))
            .unwrap_or(0);
        all[from..].to_vec()
    }

    async fn turn_from_start(&mut self, text: &str) -> Vec<Event> {
        self.say(text).await;
        self.turn().await
    }

    /// `SetModel`, then the events up to its answer (`ModelChanged` or an `Error`).
    async fn set_model(&mut self, model: &str) -> Vec<Event> {
        self.send(Command::SetModel {
            id: None,
            model: m(model),
        })
        .await;
        self.until(|e| matches!(e, Event::ModelChanged { .. } | Event::Error { .. }))
            .await
    }
}

fn finish_reason(events: &[Event]) -> FinishReason {
    match events.last() {
        Some(Event::TurnFinished { reason, .. }) => *reason,
        other => panic!("turn did not finish: {other:?}"),
    }
}

fn records(engine: &Engine) -> Vec<SessionRecord> {
    let reader = SessionReader::<SessionRecord>::open(engine.session_path()).unwrap();
    reader.records().map(|r| r.unwrap().1).collect()
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

/// A compaction summary request: no tools, one message holding the transcript.
fn is_summary(req: &CanonicalRequest) -> bool {
    req.tools.is_empty()
        && req.messages.len() == 1
        && serde_json::to_string(&req.messages[0].content)
            .unwrap()
            .contains("<transcript>")
}

/// The `required` list of the `edit` tool's schema.
fn edit_required(req: &CanonicalRequest) -> Value {
    let edit = req.tools.iter().find(|t| t.name == "edit").expect("edit");
    edit.schema["required"].clone()
}

fn reasoning_items(req: &CanonicalRequest) -> Vec<Content> {
    req.messages
        .iter()
        .flat_map(|m| &m.content)
        .filter(|c| {
            matches!(
                c,
                Content::Reasoning { .. } | Content::RedactedReasoning { .. }
            )
        })
        .cloned()
        .collect()
}

fn tail_text(req: &CanonicalRequest) -> String {
    serde_json::to_string(&req.messages.last().unwrap().content).unwrap()
}

fn first_text(req: &CanonicalRequest) -> String {
    serde_json::to_string(&req.messages.first().unwrap().content).unwrap()
}

fn signed_thought(text: &str, sig: &str, answer: &str) -> MockTurn {
    MockTurn::Events(vec![
        ProviderEvent::ReasoningDelta(text.into()),
        ProviderEvent::ReasoningSignature(sig.into()),
        ProviderEvent::TextDelta(answer.into()),
        ProviderEvent::Done {
            stop: StopReason::EndTurn,
        },
    ])
}

#[tokio::test]
async fn set_model_idle_applies_immediately_and_emits_model_changed() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![reply("one")]);
    let mut run = start(fx.switching_builder(config("anthropic/a1"), &mock));

    let answer = run.set_model("openai/o1").await;
    assert_eq!(
        answer.last(),
        Some(&Event::ModelChanged {
            id: None,
            model: m("openai/o1"),
            provider: "openai".into(),
        }),
        "{answer:?}"
    );
    // Applied before any message: the first request already goes to the new model.
    let events = run.turn_from_start("hi").await;
    assert_eq!(finish_reason(&events), FinishReason::Completed);
    assert_eq!(mock.requests()[0].model, m("openai/o1"));

    let recs = records(&run.engine);
    let set = recs
        .iter()
        .position(|r| matches!(r, SessionRecord::ModelSet { model } if *model == m("openai/o1")))
        .expect("no ModelSet record");
    let request = recs
        .iter()
        .position(|r| matches!(r, SessionRecord::Request { .. }))
        .unwrap();
    assert!(set < request);
    assert!(matches!(
        &recs[request],
        SessionRecord::Request { model, .. } if *model == m("openai/o1")
    ));
}

#[tokio::test]
async fn set_model_during_turn_applies_at_next_user_turn() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![
        tools_step(vec![call(
            "w1",
            "write",
            json!({"path": "out.txt", "content": "x"}),
        )]),
        reply("written"),
        reply("second"),
    ]);
    let mut run = start(EngineBuilder {
        interactive: true,
        ..fx.switching_builder(config("anthropic/a1"), &mock)
    });
    run.say("write it").await;
    let events = run
        .until(|e| matches!(e, Event::ToolApprovalRequested { .. }))
        .await;
    let Some(Event::ToolApprovalRequested { request_id, .. }) = events.last() else {
        unreachable!()
    };
    // Mid-turn: queued, not applied.
    run.send(Command::SetModel {
        id: None,
        model: m("openai/o1"),
    })
    .await;
    run.send(Command::Approve {
        request_id: request_id.clone(),
        session_rule: None,
    })
    .await;
    let rest = run.turn().await;
    assert_eq!(finish_reason(&rest), FinishReason::Completed, "{rest:?}");
    assert!(
        !rest.iter().any(|e| matches!(e, Event::ModelChanged { .. })),
        "{rest:?}"
    );
    let after = run.until(|e| matches!(e, Event::ModelChanged { .. })).await;
    assert!(matches!(
        after.last(),
        Some(Event::ModelChanged { model, .. }) if *model == m("openai/o1")
    ));
    let events = run.turn_from_start("again").await;
    assert_eq!(finish_reason(&events), FinishReason::Completed);
    let models: Vec<ModelId> = mock.requests().iter().map(|r| r.model.clone()).collect();
    assert_eq!(
        models,
        [m("anthropic/a1"), m("anthropic/a1"), m("openai/o1")]
    );
}

#[tokio::test]
async fn unknown_model_or_missing_key_errors_and_keeps_model() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![reply("still a1")]);
    let ok = factory(&mock);
    // No key for OpenAI.
    let keyless: ProviderFactory = Arc::new(move |model: &ModelId| {
        if model.provider() == "openai" {
            Err(ProviderSetupError::MissingKey {
                provider: provider_label(model),
                how: "set OPENAI_API_KEY".into(),
                note: None,
            })
        } else {
            ok(model)
        }
    });
    let mut run = start(EngineBuilder {
        provider_factory: Some(keyless),
        ..fx.switching_builder(config("anthropic/a1"), &mock)
    });

    for (model, hint_has) in [
        ("acme/x", "<provider>/<model>"),
        ("compat:nowhere/llama", "[[providers.compat]]"),
        ("openai/o1", "tautly auth add openai"),
    ] {
        let answer = run.set_model(model).await;
        match answer.last() {
            Some(Event::Error {
                id: _,
                kind: ErrorKind::ProviderFatal,
                retryable: false,
                hint: Some(hint),
                message,
            }) => {
                assert!(hint.contains(hint_has), "{model}: {hint}");
                assert!(message.contains(model), "{model}: {message}");
            }
            other => panic!("{model}: {other:?}"),
        }
    }
    let events = run.turn_from_start("hi").await;
    assert_eq!(finish_reason(&events), FinishReason::Completed);
    assert_eq!(mock.requests()[0].model, m("anthropic/a1"));
    assert!(
        !records(&run.engine)
            .iter()
            .any(|r| matches!(r, SessionRecord::ModelSet { .. }))
    );
}

#[tokio::test]
async fn edit_format_follows_new_model_unless_configured() {
    // The built-in catalog: Claude edits with str_replace, GPT-5 with patch.
    let fx = Fx::new();
    let mock = MockProvider::new(vec![reply("one"), reply("two")]);
    let cfg = Config {
        catalog_overrides: None,
        ..config("anthropic/claude-sonnet-5-5")
    };
    let mut run = start(fx.switching_builder(cfg, &mock));
    run.turn_from_start("hi").await;
    run.set_model("openai/gpt-5").await;
    run.turn_from_start("again").await;
    let reqs = mock.requests();
    assert_eq!(
        edit_required(&reqs[0]),
        json!(["path", "old_text", "new_text"])
    );
    assert_eq!(edit_required(&reqs[1]), json!(["patch"]));
    // The tools changed, so the prefix inputs are logged again (H5).
    let tools_hashes: Vec<String> = records(&run.engine)
        .into_iter()
        .filter_map(|r| match r {
            SessionRecord::PrefixApplied { tools_hash, .. } => Some(tools_hash),
            _ => None,
        })
        .collect();
    assert_eq!(tools_hashes.len(), 2, "{tools_hashes:?}");
    assert_ne!(tools_hashes[0], tools_hashes[1]);

    // Configured: the override for the new model keeps str_replace.
    let fx = Fx::new();
    let mock = MockProvider::new(vec![reply("one")]);
    let override_toml = entry("openai/gpt-5", 400_000, "str_replace", Some(1.25));
    let cfg = Config {
        catalog_overrides: Some(Catalog::from_toml(&override_toml).unwrap()),
        ..config("anthropic/claude-sonnet-5-5")
    };
    let mut run = start(fx.switching_builder(cfg, &mock));
    run.set_model("openai/gpt-5").await;
    run.turn_from_start("hi").await;
    assert_eq!(
        edit_required(&mock.requests()[0]),
        json!(["path", "old_text", "new_text"])
    );
}

/// History long enough to compact (more than the 4 most recent turns) whose first
/// message (about 24k tokens) puts a 32k window over its 80% threshold.
async fn fill_for_compaction(run: &mut Run) {
    let big = format!("BIG-MARKER {}", "lorem ipsum dolor sit amet ".repeat(3555));
    run.turn_from_start(&big).await;
    run.turn_from_start("two").await;
    run.turn_from_start("three").await;
}

#[tokio::test]
async fn side_model_recomputed_unless_routing_set() {
    // (switch to, routing.side_model, the model the summary names). Without routing the
    // side model follows the main one (`routing::side_model_id`): a1's side model is
    // `anthropic/a0-side`, still served by Anthropic after the switch to the small
    // `anthropic/a2`, so only a recompute names a2; `openai/mini` is not small, so its
    // provider's first small model summarizes.
    for (to, routing, side) in [
        ("openai/tiny", None, "openai/tiny"),
        ("anthropic/a2", None, "anthropic/a2"),
        ("openai/mini", None, "openai/a-side"),
        ("openai/tiny", Some("openai/mini"), "openai/mini"),
    ] {
        let fx = Fx::new();
        let mut script = vec![reply("one"), reply("two"), reply("three")];
        script.push(reply("SUMMARY-TEXT"));
        script.push(reply("done"));
        let mock = MockProvider::new(script);
        let cfg = Config {
            routing: RoutingConfig {
                side_model: routing.map(m),
            },
            ..config("anthropic/a1")
        };
        let mut run = start(fx.switching_builder(cfg, &mock));
        fill_for_compaction(&mut run).await;
        run.set_model(to).await;
        let events = run.turn_from_start("four").await;
        assert_eq!(
            finish_reason(&events),
            FinishReason::Completed,
            "{events:?}"
        );
        let summary = mock
            .requests()
            .into_iter()
            .find(is_summary)
            .expect("no summary request");
        assert_eq!(summary.model, m(side), "to {to}, routing {routing:?}");
    }
}

#[tokio::test]
async fn after_a_switch_the_summary_goes_to_the_recomputed_side_model_under_its_id() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![
        reply("one"),
        reply("two"),
        reply("three"),
        // openai/a-side: 1000 USD per million tokens, so 100 tokens cost 0.1.
        billed("SUMMARY-TEXT", 100),
        reply("done"),
    ]);
    // No routing: a1's side model is `anthropic/a0-side` until the switch.
    let mut run = start(fx.switching_builder(config("anthropic/a1"), &mock));
    fill_for_compaction(&mut run).await;
    // openai/mini has a 32k window (the next step compacts) and is not small, so the
    // recomputed side model is OpenAI's small `openai/a-side`.
    run.set_model("openai/mini").await;
    let events = run.turn_from_start("four").await;
    assert_eq!(
        finish_reason(&events),
        FinishReason::Completed,
        "{events:?}"
    );
    let summary = mock
        .requests()
        .into_iter()
        .find(is_summary)
        .expect("no summary request");
    assert_eq!(summary.model, m("openai/a-side"));
    assert_eq!(summary.max_output, 1000, "sized with the side model's cap");
    // Priced with the side model's price, not mini's (3 USD per million tokens).
    let side_costs: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            Event::Usage {
                role: UsageRole::Side,
                cost_usd,
                ..
            } => Some(*cost_usd),
            _ => None,
        })
        .collect();
    assert_eq!(side_costs.len(), 1, "{events:?}");
    assert!(
        (side_costs[0].unwrap() - 0.1).abs() < 1e-9,
        "{side_costs:?}"
    );
    // Recorded under the side model's id.
    let rows = ledger_rows_with(&fx, "openai/a-side").await;
    let side_rows: Vec<_> = rows.iter().filter(|(m, _)| m == "openai/a-side").collect();
    assert_eq!(side_rows.len(), 1, "{rows:?}");
    assert!((side_rows[0].1.unwrap() - 0.1).abs() < 1e-9, "{rows:?}");
    // The old side model was never charged.
    assert!(
        rows.iter().all(|(m, _)| m != "anthropic/a0-side"),
        "{rows:?}"
    );
}

/// The rows of the cost ledger as (model, cost), once a row for `model` is there (rows
/// are written in the background).
async fn ledger_rows_with(fx: &Fx, model: &str) -> Vec<(String, Option<f64>)> {
    let path = fx.home.path().join("ledger.sqlite");
    tokio::time::timeout(WAIT, async {
        loop {
            if let Ok(conn) = rusqlite::Connection::open(&path)
                && let Ok(mut stmt) = conn.prepare("SELECT model, cost_usd FROM usage")
            {
                let rows: Vec<(String, Option<f64>)> = stmt
                    .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                    .unwrap()
                    .map(Result::unwrap)
                    .collect();
                if rows.iter().any(|(m, _)| m == model) {
                    return rows;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no ledger row for {model}"))
}

#[tokio::test]
async fn a_side_model_summary_is_sized_priced_and_recorded_as_the_side_model() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![
        reply("one"),
        reply("two"),
        reply("three"),
        // anthropic/side: 1000 USD per million tokens, so 100 tokens cost 0.1.
        billed("SUMMARY-TEXT", 100),
        reply("done"),
    ]);
    let cfg = Config {
        routing: RoutingConfig {
            side_model: Some(m("anthropic/side")),
        },
        ..config("anthropic/a1")
    };
    let mut run = start(fx.switching_builder(cfg, &mock));
    fill_for_compaction(&mut run).await;
    // anthropic/a2 has a 32k window: the next step compacts.
    run.set_model("anthropic/a2").await;
    let events = run.turn_from_start("four").await;
    assert_eq!(
        finish_reason(&events),
        FinishReason::Completed,
        "{events:?}"
    );
    let summary = mock
        .requests()
        .into_iter()
        .find(is_summary)
        .expect("no summary request");
    assert_eq!(summary.model, m("anthropic/side"));
    // Sized with the side model's output cap (1000), not the main model's (4000).
    assert_eq!(summary.max_output, 1000);
    // Priced with the side model's price, not a2's (3 USD per million tokens).
    let side_cost = events.iter().find_map(|e| match e {
        Event::Usage {
            role: UsageRole::Side,
            cost_usd,
            ..
        } => Some(*cost_usd),
        _ => None,
    });
    let cost = side_cost.flatten().expect("no priced side usage");
    assert!((cost - 0.1).abs() < 1e-9, "{cost}");
    // And recorded under the side model's id.
    let rows = ledger_rows_with(&fx, "anthropic/side").await;
    let side_rows: Vec<_> = rows.iter().filter(|(m, _)| m == "anthropic/side").collect();
    assert_eq!(side_rows.len(), 1, "{rows:?}");
    assert!((side_rows[0].1.unwrap() - 0.1).abs() < 1e-9, "{rows:?}");
}

/// Spec 4.7: a reasoning change that alters an Anthropic request's thinking parameters
/// starts a cold epoch, like a model switch; on another provider, or when the parameters
/// stay the same, the cache stays warm.
#[tokio::test]
async fn a_reasoning_change_on_anthropic_starts_a_cold_epoch() {
    use tautly_core::ReasoningLevel;
    for (model, level, cold_expected) in [
        ("anthropic/a1", ReasoningLevel::High, true),
        // `auto` already resolves to `medium`: nothing changes on the wire.
        ("anthropic/a1", ReasoningLevel::Medium, false),
        ("openai/o1", ReasoningLevel::High, false),
    ] {
        let fx = Fx::new();
        let small: String = (0..20).map(|i| format!("line {i}\n")).collect();
        fx.write("small.txt", &small);
        let read = || call("r", "read", json!({"path": "small.txt", "full": true}));
        let mock = MockProvider::new(vec![
            tools_step(vec![read()]),
            tools_step(vec![read()]),
            reply("read twice"),
            reply("after"),
        ]);
        let mut run = start(fx.switching_builder(config(model), &mock));
        let t1 = run.turn_from_start("read it twice").await;
        assert_eq!(finish_reason(&t1), FinishReason::Completed, "{t1:?}");
        run.send(Command::SetReasoning { level }).await;
        let t2 = run.turn_from_start("after the change").await;
        let cold = t2.iter().any(|e| {
            matches!(
                e,
                Event::ContextCollected {
                    reason: MutationReason::ColdCache,
                    ..
                }
            )
        });
        assert_eq!(cold, cold_expected, "{model} {level}: {t2:?}");
    }
}

#[tokio::test]
async fn first_request_after_switch_is_cold_epoch() {
    let fx = Fx::new();
    let small: String = (0..20).map(|i| format!("line {i}\n")).collect();
    fx.write("small.txt", &small);
    let read = || call("r", "read", json!({"path": "small.txt", "full": true}));
    let mock = MockProvider::new(vec![
        tools_step(vec![read()]),
        tools_step(vec![read()]),
        reply("read twice"),
        reply("warm"),
        reply("cold"),
    ]);
    let mut run = start(fx.switching_builder(config("anthropic/a1"), &mock));
    let t1 = run.turn_from_start("read it twice").await;
    assert_eq!(finish_reason(&t1), FinishReason::Completed, "{t1:?}");
    run.send(Command::Pin {
        text: "PIN-MARKER".into(),
    })
    .await;

    // Warm cache, same model: the superseded read stays and the pin waits in the tail.
    let t2 = run.turn_from_start("anything").await;
    assert!(
        !t2.iter()
            .any(|e| matches!(e, Event::ContextCollected { .. })),
        "{t2:?}"
    );
    let warm = mock.requests()[3].clone();
    assert!(tail_text(&warm).contains("PIN-MARKER"));

    run.set_model("anthropic/a2").await;
    let t3 = run.turn_from_start("after the switch").await;
    assert!(
        t3.iter().any(|e| matches!(
            e,
            Event::ContextCollected {
                reason: MutationReason::ColdCache,
                ..
            }
        )),
        "{t3:?}"
    );
    let cold = mock.requests()[4].clone();
    assert!(!tail_text(&cold).contains("PIN-MARKER"));
    assert!(first_text(&cold).contains("PIN-MARKER"));
    assert!(
        records(&run.engine)
            .iter()
            .any(|r| matches!(r, SessionRecord::PinsFolded))
    );
}

#[tokio::test]
async fn smaller_window_forces_compaction_at_next_step() {
    let fx = Fx::new();
    let mut script: Vec<MockTurn> = (0..5).map(|i| reply(&format!("ok {i}"))).collect();
    script.push(reply("SUMMARY-TEXT"));
    script.push(reply("done"));
    let mock = MockProvider::new(script);
    let mut run = start(fx.switching_builder(config("anthropic/a1"), &mock));
    // About 150k tokens: 5 messages of 120 KB (4 bytes a token).
    let filler = "lorem ipsum dolor sit amet ".repeat(120_000 / 27);
    for i in 0..5 {
        let events = run.turn_from_start(&format!("M{i} {filler}")).await;
        assert!(
            !events.iter().any(|e| matches!(e, Event::Compacted { .. })),
            "compacted before the switch: {events:?}"
        );
    }
    run.set_model("openai/small").await;
    let events = run.turn_from_start("go on").await;
    assert_eq!(
        finish_reason(&events),
        FinishReason::Completed,
        "{events:?}"
    );
    let compacted = events
        .iter()
        .position(|e| {
            matches!(
                e,
                Event::Compacted {
                    reason: MutationReason::Forced,
                    ..
                }
            )
        })
        .unwrap_or_else(|| panic!("no forced compaction: {events:?}"));
    // Before the step's request: its answer (ContextStatus) comes after.
    let answered = events
        .iter()
        .rposition(|e| matches!(e, Event::ContextStatus { .. }))
        .unwrap();
    assert!(compacted < answered);
    let reqs = mock.requests();
    assert!(is_summary(&reqs[5]));
    assert_eq!(reqs[6].model, m("openai/small"));
    assert!(
        !serde_json::to_string(&reqs[6].messages)
            .unwrap()
            .contains("M0 ")
    );
}

#[tokio::test]
async fn foreign_reasoning_stripped_same_provider_kept() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![
        signed_thought("THINK-1", "SIG-1", "answer one"),
        reply("two"),
        MockTurn::Events(vec![
            ProviderEvent::RedactedReasoning("ENC-OPENAI".into()),
            ProviderEvent::TextDelta("three".into()),
            ProviderEvent::Done {
                stop: StopReason::EndTurn,
            },
        ]),
        reply("four"),
    ]);
    let mut run = start(fx.switching_builder(config("anthropic/a1"), &mock));
    run.turn_from_start("one").await;
    // Same provider: the signed thinking travels on.
    run.set_model("anthropic/a2").await;
    run.turn_from_start("two").await;
    // Another provider: Anthropic's thinking is stripped.
    run.set_model("openai/o1").await;
    run.turn_from_start("three").await;
    // And back: OpenAI's reasoning item is stripped.
    run.set_model("anthropic/a1").await;
    run.turn_from_start("four").await;

    let reqs = mock.requests();
    assert_eq!(
        reasoning_items(&reqs[1]),
        [Content::Reasoning {
            text: "THINK-1".into(),
            signature: Some("SIG-1".into()),
        }]
    );
    assert_eq!(reasoning_items(&reqs[2]), []);
    assert_eq!(reasoning_items(&reqs[3]), []);
    // The answers themselves stay.
    let last = serde_json::to_string(&reqs[3].messages).unwrap();
    assert!(last.contains("answer one") && last.contains("three"));
}

#[tokio::test]
async fn reasoning_rejection_strips_and_retries_once() {
    let fx = Fx::new();
    let rejected = || {
        MockTurn::Error(ProviderError::InvalidRequest(
            "messages.1.content.0: invalid `signature` in `thinking` block".into(),
        ))
    };
    let mock = MockProvider::new(vec![
        signed_thought("THINK-1", "SIG-1", "answer one"),
        rejected(),
        rejected(),
    ]);
    let mut run = start(fx.switching_builder(config("anthropic/a1"), &mock));
    run.turn_from_start("one").await;
    run.set_model("anthropic/a2").await;
    let events = run.turn_from_start("two").await;

    assert_eq!(finish_reason(&events), FinishReason::Error, "{events:?}");
    let reqs = mock.requests();
    assert_eq!(reqs.len(), 3, "retried exactly once");
    assert_eq!(reasoning_items(&reqs[1]).len(), 1);
    assert_eq!(reasoning_items(&reqs[2]), []);
    assert!(
        warnings(&events).iter().any(|w| w.contains("reasoning")),
        "{events:?}"
    );
    let errors: Vec<&Event> = events
        .iter()
        .filter(|e| matches!(e, Event::Error { .. }))
        .collect();
    assert_eq!(errors.len(), 1, "{events:?}");
    assert!(matches!(
        errors[0],
        Event::Error { kind: ErrorKind::ProviderFatal, message, .. } if message.contains("signature")
    ));
    // The strip is in the log, between the refused request and its retry, so a rebuild
    // removes the reasoning at the same point.
    let recs = records(&run.engine);
    let requests: Vec<usize> = recs
        .iter()
        .enumerate()
        .filter(|(_, r)| matches!(r, SessionRecord::Request { .. }))
        .map(|(i, _)| i)
        .collect();
    let stripped: Vec<usize> = recs
        .iter()
        .enumerate()
        .filter(|(_, r)| matches!(r, SessionRecord::ReasoningStripped))
        .map(|(i, _)| i)
        .collect();
    assert_eq!(stripped.len(), 1, "{recs:?}");
    assert!(
        requests[1] < stripped[0] && stripped[0] < requests[2],
        "{recs:?}"
    );
}

fn rejected_reasoning() -> MockTurn {
    MockTurn::Error(ProviderError::InvalidRequest(
        "messages.1.content.0: invalid `signature` in `thinking` block".into(),
    ))
}

#[tokio::test]
async fn reasoning_rejection_retry_that_succeeds_completes_without_reasoning() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![
        signed_thought("THINK-1", "SIG-1", "answer one"),
        rejected_reasoning(),
        reply("two"),
        reply("three"),
    ]);
    let mut run = start(fx.switching_builder(config("anthropic/a1"), &mock));
    run.turn_from_start("one").await;
    run.set_model("anthropic/a2").await;
    let t2 = run.turn_from_start("two").await;
    assert_eq!(finish_reason(&t2), FinishReason::Completed, "{t2:?}");
    assert!(
        !t2.iter().any(|e| matches!(e, Event::Error { .. })),
        "{t2:?}"
    );
    let t3 = run.turn_from_start("three").await;
    assert_eq!(finish_reason(&t3), FinishReason::Completed, "{t3:?}");

    let reqs = mock.requests();
    assert_eq!(reqs.len(), 4);
    assert_eq!(reasoning_items(&reqs[1]).len(), 1);
    // The retry and everything after it go without the reasoning; the answer stays.
    assert_eq!(reasoning_items(&reqs[2]), []);
    assert_eq!(reasoning_items(&reqs[3]), []);
    assert!(
        serde_json::to_string(&reqs[3].messages)
            .unwrap()
            .contains("answer one")
    );
    let stripped = records(&run.engine)
        .iter()
        .filter(|r| matches!(r, SessionRecord::ReasoningStripped))
        .count();
    assert_eq!(stripped, 1);
}

#[tokio::test]
async fn a_cross_provider_switch_does_not_arm_the_reasoning_retry() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![
        signed_thought("THINK-1", "SIG-1", "answer one"),
        rejected_reasoning(),
        reply("never sent"),
    ]);
    let mut run = start(fx.switching_builder(config("anthropic/a1"), &mock));
    run.turn_from_start("one").await;
    // Another provider: the reasoning is stripped before the request, so a refusal that
    // names it is not about carried-over reasoning and is not retried.
    run.set_model("openai/o1").await;
    let events = run.turn_from_start("two").await;
    assert_eq!(finish_reason(&events), FinishReason::Error, "{events:?}");
    assert_eq!(mock.requests().len(), 2, "no retry");
    assert!(
        !warnings(&events).iter().any(|w| w.contains("refused")),
        "{events:?}"
    );
    assert!(
        !records(&run.engine)
            .iter()
            .any(|r| matches!(r, SessionRecord::ReasoningStripped))
    );
}

#[tokio::test]
async fn model_set_survives_resume() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![]);
    let mut run = start(fx.switching_builder(config("anthropic/a1"), &mock));
    run.set_model("openai/o1").await;
    run.set_model("acme/nope").await;
    run.set_model("openai/small").await;
    let path: PathBuf = run.engine.session_path().to_path_buf();
    run.send(Command::Shutdown).await;
    drop(run);
    // Wait for the actor to write its last records.
    let reader = tokio::time::timeout(WAIT, async {
        loop {
            let reader = SessionReader::<SessionRecord>::open(&path).unwrap();
            let sets = reader
                .records()
                .filter(|r| matches!(r, Ok((_, SessionRecord::ModelSet { .. }))))
                .count();
            if sets == 2 {
                return reader;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("ModelSet records never landed");
    assert_eq!(reader.header().model.as_deref(), Some("anthropic/a1"));
    assert_eq!(session_model(&reader).unwrap(), Some(m("openai/small")));
}

#[tokio::test]
async fn budget_carries_over_and_unpriced_model_warns() {
    let fx = Fx::new();
    // anthropic/pricey: 1000 USD per million tokens, so 30 tokens cost 0.03.
    let mock = MockProvider::new(vec![
        billed("one", 30),
        billed("two", 1000),
        billed("three", 60),
    ]);
    let mut cfg = config("anthropic/pricey");
    cfg.budget.session.max_usd = Some(0.10);
    let mut run = start(fx.switching_builder(cfg, &mock));
    let t1 = run.turn_from_start("one").await;
    assert!(warnings(&t1).is_empty(), "{t1:?}");

    let answer = run.set_model("openai/free").await;
    // The warning follows the answer.
    let more = run.until(|e| matches!(e, Event::Warning { .. })).await;
    let w = warnings(&more);
    assert!(
        w.iter()
            .any(|w| w.contains("openai/free") && w.contains("no known price")),
        "{answer:?} {more:?}"
    );
    let t2 = run.turn_from_start("two").await;
    assert!(t2.iter().any(|e| matches!(
        e,
        Event::Usage {
            cost_usd: None,
            tokens_in: 1000,
            ..
        }
    )));
    assert!(warnings(&t2).is_empty(), "{t2:?}");

    run.set_model("anthropic/pricey").await;
    let t3 = run.turn_from_start("three").await;
    // 0.03 before the switch plus 0.06 now: the spend carried over.
    assert!(
        warnings(&t3)
            .iter()
            .any(|w| w.contains("session budget 90% used") && w.contains("$0.0900 of $0.1000")),
        "{t3:?}"
    );
}

#[tokio::test]
async fn switch_to_unpriced_model_rewarns_and_back_restores_usd_budget() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![
        billed("one", 30),
        billed("two", 5000),
        billed("three", 30),
        billed("never sent", 1),
    ]);
    let mut cfg = config("anthropic/pricey");
    cfg.budget.session.max_usd = Some(0.05);
    let mut run = start(fx.switching_builder(cfg, &mock));
    run.turn_from_start("one").await;

    run.set_model("openai/free").await;
    let first = run.until(|e| matches!(e, Event::Warning { .. })).await;
    assert!(warnings(&first)[0].contains("no known price"));
    // Unpriced: only tokens count, so 5000 tokens are no USD spend.
    let t2 = run.turn_from_start("two").await;
    assert_eq!(finish_reason(&t2), FinishReason::Completed);

    // Back to the priced model: USD is enforced again, the spend so far unchanged.
    run.set_model("anthropic/pricey").await;
    let t3 = run.turn_from_start("three").await;
    assert!(
        warnings(&t3)
            .iter()
            .any(|w| w.contains("session budget exhausted") && w.contains("$0.0600 of $0.0500")),
        "{t3:?}"
    );
    let t4 = run.turn_from_start("four").await;
    assert_eq!(finish_reason(&t4), FinishReason::Budget);
    assert_eq!(mock.requests().len(), 3);

    // The warning comes again for an unpriced model.
    run.set_model("openai/free").await;
    let again = run.until(|e| matches!(e, Event::Warning { .. })).await;
    assert!(
        warnings(&again)
            .iter()
            .any(|w| w.contains("openai/free") && w.contains("no known price")),
        "{again:?}"
    );
}

#[tokio::test]
async fn an_uncatalogued_model_switches_with_conservative_defaults_and_a_warning() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![reply("one")]);
    let mut run = start(fx.switching_builder(config("anthropic/a1"), &mock));
    let answer = run.set_model("anthropic/not-in-the-catalog").await;
    assert!(
        matches!(
            answer.last(),
            Some(Event::ModelChanged { model, .. }) if *model == m("anthropic/not-in-the-catalog")
        ),
        "{answer:?}"
    );
    assert!(
        warnings(&answer)
            .iter()
            .any(|w| w.contains("anthropic/not-in-the-catalog")
                && w.contains("conservative defaults")
                && w.contains("catalog_overrides")),
        "{answer:?}"
    );
    let events = run.turn_from_start("hi").await;
    assert_eq!(finish_reason(&events), FinishReason::Completed);
    let req = &mock.requests()[0];
    assert_eq!(req.model, m("anthropic/not-in-the-catalog"));
    // The conservative defaults: a 32k window, hashline edits.
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::ContextStatus { budget: 32_000, .. }))
    );
    assert!(
        records(&run.engine).iter().any(
            |r| matches!(r, SessionRecord::ModelSet { model } if *model == m("anthropic/not-in-the-catalog"))
        )
    );
}

/// The `used` of the first `ContextStatus` of a turn.
fn used(events: &[Event]) -> u32 {
    events
        .iter()
        .find_map(|e| match e {
            Event::ContextStatus { used, .. } => Some(*used),
            _ => None,
        })
        .expect("no ContextStatus")
}

#[tokio::test]
async fn the_estimator_calibration_resets_when_the_provider_changes() {
    let fx = Fx::new();
    // A billed count far above the estimate drives the learned ratio to its 2.0 limit.
    let mock = MockProvider::new(vec![billed("one", 1_000_000), reply("two"), reply("three")]);
    let mut run = start(fx.switching_builder(config("anthropic/a1"), &mock));
    run.turn_from_start("one").await;
    // Same provider: the ratio carries on.
    run.set_model("anthropic/a2").await;
    let kept = used(&run.turn_from_start("two").await);
    // Another provider (another tokenizer): it starts again at 1.0, about half the
    // estimate of the same context.
    run.set_model("openai/o1").await;
    let reset = used(&run.turn_from_start("three").await);
    assert!(reset * 3 < kept * 2, "kept {kept}, reset {reset}");
}

#[tokio::test]
async fn shutdown_does_not_wait_for_a_slow_provider_build() {
    use std::sync::Mutex;
    let fx = Fx::new();
    let mock = MockProvider::new(vec![]);
    let (entered_tx, entered_rx) = std::sync::mpsc::channel::<()>();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let (entered_tx, release_rx) = (Mutex::new(entered_tx), Mutex::new(release_rx));
    let built = mock.clone();
    // Stands for a keychain prompt nobody answers.
    let slow: ProviderFactory = Arc::new(move |_: &ModelId| {
        let _ = entered_tx.lock().unwrap().send(());
        let _ = release_rx.lock().unwrap().recv();
        Ok(Arc::new(built.clone()) as Arc<dyn Provider>)
    });
    let mut run = start(EngineBuilder {
        provider_factory: Some(slow),
        ..fx.switching_builder(config("anthropic/a1"), &mock)
    });
    run.send(Command::SetModel {
        id: None,
        model: m("openai/o1"),
    })
    .await;
    tokio::task::spawn_blocking(move || entered_rx.recv_timeout(WAIT))
        .await
        .unwrap()
        .expect("the factory never ran");
    run.send(Command::Shutdown).await;
    let mut rest = Vec::new();
    let closed = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(ev) = run.events.recv().await {
            rest.push(ev);
        }
    })
    .await;
    // Let the abandoned build finish before the runtime goes away.
    let _ = release_tx.send(());
    assert!(
        closed.is_ok(),
        "shutdown waited for the provider build: {rest:?}"
    );
    assert!(
        !rest.iter().any(|e| matches!(e, Event::ModelChanged { .. })),
        "{rest:?}"
    );
    assert!(
        rest.iter().any(|e| matches!(
            e,
            Event::Error { kind: ErrorKind::ProviderFatal, message, .. }
                if message.contains("openai/o1") && message.contains("shutting down")
        )),
        "{rest:?}"
    );
    assert!(
        !records(&run.engine)
            .iter()
            .any(|r| matches!(r, SessionRecord::ModelSet { .. }))
    );
}

#[tokio::test]
async fn a_replay_during_a_provider_build_is_refused_and_answers_echo_ids() {
    // Review CP5-A M9 (refused, not queued behind the switch) and M4 (correlation ids).
    use std::sync::Mutex;
    let fx = Fx::new();
    let mock = MockProvider::new(vec![]);
    let (entered_tx, entered_rx) = std::sync::mpsc::channel::<()>();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let (entered_tx, release_rx) = (Mutex::new(entered_tx), Mutex::new(release_rx));
    let built = mock.clone();
    let slow: ProviderFactory = Arc::new(move |_: &ModelId| {
        let _ = entered_tx.lock().unwrap().send(());
        let _ = release_rx.lock().unwrap().recv();
        Ok(Arc::new(built.clone()) as Arc<dyn Provider>)
    });
    let mut run = start(EngineBuilder {
        provider_factory: Some(slow),
        ..fx.switching_builder(config("anthropic/a1"), &mock)
    });
    run.send(Command::SetModel {
        id: Some("sm-1".into()),
        model: m("openai/o1"),
    })
    .await;
    tokio::task::spawn_blocking(move || entered_rx.recv_timeout(WAIT))
        .await
        .unwrap()
        .expect("the factory never ran");
    run.send(Command::Replay {
        id: Some("r-1".into()),
        session_id: None,
        speed: 0.0,
        from_step: None,
    })
    .await;
    // Time for the actor to take the replay while the build still waits (the commands
    // that arrive meanwhile are handled once it ends).
    tokio::time::sleep(Duration::from_millis(300)).await;
    let _ = release_tx.send(());
    let (mut changed, mut busy) = (false, false);
    while !(changed && busy) {
        match run.next().await {
            Event::ModelChanged { id, .. } => {
                assert_eq!(id.as_deref(), Some("sm-1"));
                changed = true;
            }
            Event::Error { id, message, .. } => {
                assert_eq!(message, tautly_core::replay::REPLAY_BUSY);
                assert_eq!(id.as_deref(), Some("r-1"));
                busy = true;
            }
            _ => {}
        }
    }
    // Nothing was queued: no replay follows.
    run.send(Command::Shutdown).await;
    while let Some(ev) = run.events.recv().await {
        assert!(!matches!(ev, Event::ReplayFinished { .. }), "{ev:?}");
    }
}

// ----------------------------------------------------------- resume and fork after a switch

trait FxExt2 {
    fn resumed(&self, cfg: Config, mock: &MockProvider, id: &str) -> EngineBuilder;
}

impl FxExt2 for Fx {
    fn resumed(&self, cfg: Config, mock: &MockProvider, id: &str) -> EngineBuilder {
        EngineBuilder {
            resume: Some(id.to_string()),
            ..self.switching_builder(cfg, mock)
        }
    }
}

/// Re-review N2: a `Shutdown` sent while a resumed session restores its model (a keychain
/// prompt nobody answers) stops it before the session-start checkpoint and MCP servers
/// start.
#[tokio::test]
async fn a_shutdown_while_a_resumed_session_sets_up_its_model_starts_nothing() {
    use std::sync::Mutex;
    let fx = Fx::new();
    // A git work tree, so checkpoints would be taken.
    let git = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(fx.dir.path())
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .status()
        .unwrap();
    assert!(git.success());
    let mock = MockProvider::new(vec![reply("one")]);
    let mut run = start(fx.switching_builder(config("anthropic/a1"), &mock));
    run.turn_from_start("one").await;
    run.set_model("openai/o1").await;
    let id = run.engine.session_id().to_string();
    let path = run.engine.session_path().to_path_buf();
    run.stop().await;
    let logged = SessionReader::<SessionRecord>::open(&path)
        .unwrap()
        .records()
        .count();

    let (entered_tx, entered_rx) = std::sync::mpsc::channel::<()>();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let (entered_tx, release_rx) = (Mutex::new(entered_tx), Mutex::new(release_rx));
    let built = mock.clone();
    let slow: ProviderFactory = Arc::new(move |_: &ModelId| {
        let _ = entered_tx.lock().unwrap().send(());
        let _ = release_rx.lock().unwrap().recv();
        Ok(Arc::new(built.clone()) as Arc<dyn Provider>)
    });
    let marker = fx.home.path().join("mcp-started");
    let mut cfg = config("anthropic/a1");
    cfg.mcp.servers.insert(
        "probe".into(),
        tautly_core::ServerSpec::Stdio {
            command: "/bin/sh".into(),
            args: vec![
                "-c".into(),
                format!("touch '{}'; sleep 30", marker.display()),
            ],
            env: Default::default(),
            cwd: None,
        },
    );
    let mut run = start(EngineBuilder {
        provider_factory: Some(slow),
        ..fx.resumed(cfg, &mock, &id)
    });
    tokio::task::spawn_blocking(move || entered_rx.recv_timeout(WAIT))
        .await
        .unwrap()
        .expect("the resumed model was never set up");
    run.send(Command::Shutdown).await;
    let mut rest = Vec::new();
    let closed = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(ev) = run.events.recv().await {
            rest.push(ev);
        }
    })
    .await;
    let _ = release_tx.send(());
    assert!(closed.is_ok(), "shutdown waited for the model setup");
    assert!(
        !rest
            .iter()
            .any(|e| matches!(e, Event::McpServerStatus { .. })),
        "{rest:?}"
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!marker.exists(), "an MCP server was started");
    let after: Vec<SessionRecord> = SessionReader::<SessionRecord>::open(&path)
        .unwrap()
        .records()
        .skip(logged)
        .map(|r| r.unwrap().1)
        .collect();
    assert!(
        !after.iter().any(|r| matches!(
            r,
            SessionRecord::Checkpoint { .. } | SessionRecord::CheckpointSkipped { .. }
        )),
        "{after:?}"
    );
    assert!(after.contains(&SessionRecord::Command(Command::Shutdown)));
    // Nor is the model it was on given up: a later resume is on it again.
    assert!(
        !after
            .iter()
            .any(|r| matches!(r, SessionRecord::ModelSet { .. })),
        "{after:?}"
    );
}

/// A session resumed after a switch is on the model it switched to, with that model's
/// tools (`patch` edits) and without the reasoning the switch stripped: its next request
/// is the one the live session sent.
#[tokio::test]
async fn a_resumed_session_continues_on_the_model_it_switched_to() {
    let script = || {
        vec![
            signed_thought("THINK-1", "SIG-1", "answer one"),
            reply("two"),
            reply("three"),
        ]
    };
    let live_fx = Fx::new();
    let live_mock = MockProvider::new(script());
    let mut live = start(live_fx.switching_builder(config("anthropic/a1"), &live_mock));
    live.turn_from_start("one").await;
    live.set_model("openai/o1").await;
    live.turn_from_start("two").await;
    live.turn_from_start("three").await;
    live.stop().await;
    let expected = live_mock.requests()[2].clone();
    assert_eq!(expected.model, m("openai/o1"));

    let fx = Fx::new();
    let mock = MockProvider::new(script());
    let mut run = start(fx.switching_builder(config("anthropic/a1"), &mock));
    run.turn_from_start("one").await;
    run.set_model("openai/o1").await;
    run.turn_from_start("two").await;
    let id = run.engine.session_id().to_string();
    run.stop().await;

    let mock = MockProvider::new(vec![reply("three")]);
    let mut run = start(fx.resumed(config("anthropic/a1"), &mock, &id));
    let events = run.turn_from_start("three").await;
    assert_eq!(
        finish_reason(&events),
        FinishReason::Completed,
        "{events:?}"
    );
    let got = &mock.requests()[0];
    assert_eq!(got.model, expected.model);
    assert_eq!(edit_required(got), json!(["patch"]));
    assert_eq!(reasoning_items(got), []);
    assert_eq!(got.tools, expected.tools);
    assert_eq!(got.messages, expected.messages);
    run.stop().await;
}

/// A resumed session does not send again the reasoning a model refused (`ReasoningStripped`
/// is replayed where it was logged).
#[tokio::test]
async fn a_resumed_session_does_not_resend_the_reasoning_a_model_refused() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![
        signed_thought("THINK-1", "SIG-1", "answer one"),
        rejected_reasoning(),
        reply("two"),
    ]);
    let mut run = start(fx.switching_builder(config("anthropic/a1"), &mock));
    run.turn_from_start("one").await;
    run.set_model("anthropic/a2").await;
    let t2 = run.turn_from_start("two").await;
    assert_eq!(finish_reason(&t2), FinishReason::Completed, "{t2:?}");
    let id = run.engine.session_id().to_string();
    run.stop().await;

    let mock = MockProvider::new(vec![reply("three")]);
    let mut run = start(fx.resumed(config("anthropic/a1"), &mock, &id));
    run.turn_from_start("three").await;
    let got = &mock.requests()[0];
    assert_eq!(got.model, m("anthropic/a2"));
    assert_eq!(reasoning_items(got), []);
    let text = serde_json::to_string(&got.messages).unwrap();
    assert!(
        text.contains("answer one") && text.contains("two"),
        "{text}"
    );
    run.stop().await;
}

/// A session resumed right after a switch within one provider (no request since) still
/// gets the one retry without reasoning.
#[tokio::test]
async fn a_session_resumed_right_after_a_switch_still_retries_without_reasoning() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![signed_thought("THINK-1", "SIG-1", "answer one")]);
    let mut run = start(fx.switching_builder(config("anthropic/a1"), &mock));
    run.turn_from_start("one").await;
    run.set_model("anthropic/a2").await;
    let id = run.engine.session_id().to_string();
    run.stop().await;

    let mock = MockProvider::new(vec![rejected_reasoning(), reply("two")]);
    let mut run = start(fx.resumed(config("anthropic/a1"), &mock, &id));
    let events = run.turn_from_start("two").await;
    assert_eq!(
        finish_reason(&events),
        FinishReason::Completed,
        "{events:?}"
    );
    let reqs = mock.requests();
    assert_eq!(reqs.len(), 2);
    assert_eq!(reqs[0].model, m("anthropic/a2"));
    assert_eq!(reasoning_items(&reqs[0]).len(), 1);
    assert_eq!(reasoning_items(&reqs[1]), []);
    run.stop().await;
}

/// A branch cut before a later switch is on the model of its cut, not the session's. (A
/// switch between turns is part of the cut at the step before it, as step n + 1 was built
/// after it.)
#[tokio::test]
async fn a_fork_before_a_later_switch_is_on_the_model_of_its_cut() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![reply("one"), reply("two"), reply("three")]);
    let mut run = start(fx.switching_builder(config("anthropic/a1"), &mock));
    run.turn_from_start("one").await;
    run.set_model("openai/o1").await;
    run.turn_from_start("two").await;
    run.set_model("anthropic/a2").await;
    // Steps: 1 "one", 2 its answer, (switch to o1), 3 "two", 4 its answer, (switch to a2).
    run.send(Command::Fork {
        id: None,
        at: Some(2),
        label: None,
    })
    .await;
    let events = run
        .until(|e| matches!(e, Event::SessionChanged { .. } | Event::Error { .. }))
        .await;
    assert!(
        matches!(events.last(), Some(Event::SessionChanged { .. })),
        "{events:?}"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            Event::ModelChanged { model, .. } if *model == m("openai/o1")
        )),
        "{events:?}"
    );
    run.turn_from_start("three").await;
    let got = mock.requests().last().unwrap().clone();
    assert_eq!(got.model, m("openai/o1"));
    assert_eq!(edit_required(&got), json!(["patch"]));
    let text = serde_json::to_string(&got.messages).unwrap();
    assert!(text.contains("one") && !text.contains("two"), "{text}");
    run.stop().await;
}

// ------------------------------------------------------- reasoning: history edits, resume

/// One Anthropic model whose thinking cannot be turned off (adaptive thinking).
fn adaptive_config() -> Config {
    let toml = entry("anthropic/a1", 200_000, "str_replace", Some(3.0))
        .replace("reasoning = \"budget\"", "reasoning = \"effort_always\"");
    let mut c = config("anthropic/a1");
    c.catalog_overrides = Some(Catalog::from_toml(&toml).unwrap());
    c
}

fn thought_then_call(sig: &str, call: ProviderEvent) -> MockTurn {
    MockTurn::Events(vec![
        ProviderEvent::ReasoningDelta("thinking".into()),
        ProviderEvent::ReasoningSignature(sig.into()),
        call,
        ProviderEvent::Done {
            stop: StopReason::ToolUse,
        },
    ])
}

/// The request as the Anthropic adapter sends it is one the API takes after history
/// edits: `drop_block` binding with its beta header, signed thinking blocks first in
/// their turn, and every `tool_use` answered in the next user turn. Consecutive messages
/// of one role are one turn (the API combines them; the working state follows the tool
/// results as its own user message).
fn assert_valid_anthropic_request(req: &CanonicalRequest) {
    use tautly_provider::anthropic;
    let w = anthropic::to_wire(req);
    assert_eq!(
        w["thinking"]["block_binding"],
        json!({"prefix_mismatch_behavior": "drop_block"})
    );
    assert!(anthropic::beta_headers(req).contains(&"thinking-binding-controls-2026-08-01"));
    let msgs = w["messages"].as_array().unwrap();
    assert_eq!(msgs[0]["role"], "user");
    let mut turns: Vec<(Value, Vec<Value>)> = Vec::new();
    for m in msgs {
        let blocks = m["content"].as_array().unwrap();
        assert!(!blocks.is_empty(), "empty message: {w}");
        match turns.last_mut() {
            Some((role, all)) if *role == m["role"] => all.extend(blocks.iter().cloned()),
            _ => turns.push((m["role"].clone(), blocks.clone())),
        }
    }
    let mut open: Vec<String> = Vec::new();
    for (role, blocks) in &turns {
        let is_thinking =
            |b: &Value| matches!(b["type"].as_str(), Some("thinking" | "redacted_thinking"));
        let ids = |kind: &str, key: &str| -> Vec<String> {
            blocks
                .iter()
                .filter(|b| b["type"] == kind)
                .map(|b| b[key].as_str().unwrap().to_string())
                .collect()
        };
        if *role == "assistant" {
            let lead = blocks.iter().take_while(|b| is_thinking(b)).count();
            assert!(
                !blocks[lead..].iter().any(is_thinking),
                "thinking after content: {w}"
            );
            for b in &blocks[..lead] {
                if b["type"] == "thinking" {
                    assert!(
                        b["signature"].as_str().is_some_and(|s| !s.is_empty()),
                        "{w}"
                    );
                }
            }
            open = ids("tool_use", "id");
        } else {
            assert_eq!(
                ids("tool_result", "tool_use_id"),
                open,
                "unanswered tool_use: {w}"
            );
            open.clear();
        }
    }
}

/// Review C1: a GC of a superseded read, a pin fold (both at the cold epoch a reasoning
/// change starts) and a keep-tail compaction edit the history the thinking blocks were
/// bound to; every request after them still has a valid shape, with `drop_block`.
#[tokio::test]
async fn history_edits_leave_a_valid_adaptive_request() {
    let fx = Fx::new();
    let small: String = (0..20).map(|i| format!("line {i}\n")).collect();
    fx.write("small.txt", &small);
    let read = |id: &str| call(id, "read", json!({"path": "small.txt", "full": true}));
    let mut script = vec![
        thought_then_call("s1", read("r1")),
        thought_then_call("s2", read("r2")),
        signed_thought("t", "s3", "read twice"),
        signed_thought("t", "s4", "after the change"),
    ];
    for i in 5..=8 {
        script.push(signed_thought("t", &format!("s{i}"), "more"));
    }
    script.push(reply("SUMMARY"));
    script.push(signed_thought("t", "s9", "after compaction"));
    let mock = MockProvider::new(script);
    let mut run = start(fx.switching_builder(adaptive_config(), &mock));
    let t1 = run.turn_from_start("read it twice").await;
    assert_eq!(finish_reason(&t1), FinishReason::Completed, "{t1:?}");
    run.send(Command::Pin {
        text: "PIN-MARKER".into(),
    })
    .await;
    run.send(Command::SetReasoning {
        level: tautly_core::ReasoningLevel::High,
    })
    .await;
    let t2 = run.turn_from_start("after the change").await;
    assert!(
        t2.iter().any(|e| matches!(
            e,
            Event::ContextCollected {
                reason: MutationReason::ColdCache,
                ..
            }
        )),
        "{t2:?}"
    );
    for _ in 0..4 {
        let t = run.turn_from_start("more").await;
        assert_eq!(finish_reason(&t), FinishReason::Completed, "{t:?}");
    }
    run.send(Command::Compact { id: None }).await;
    let c = run
        .until(|e| matches!(e, Event::CompactFinished { .. }))
        .await;
    assert!(
        c.iter().any(|e| matches!(e, Event::Compacted { .. })),
        "{c:?}"
    );
    let t = run.turn_from_start("after compaction").await;
    assert_eq!(finish_reason(&t), FinishReason::Completed, "{t:?}");
    assert!(
        records(&run.engine)
            .iter()
            .any(|r| matches!(r, SessionRecord::PinsFolded))
    );

    let reqs = mock.requests();
    assert_eq!(reqs.len(), 10);
    let main: Vec<_> = reqs.iter().filter(|r| !r.tools.is_empty()).collect();
    assert_eq!(main.len(), 9);
    for r in main {
        assert_valid_anthropic_request(r);
    }
    // The last request still replays the kept turns' thinking.
    let last = tautly_provider::anthropic::to_wire(reqs.last().unwrap());
    assert!(last.to_string().contains("\"signature\":\"s8\""), "{last}");
}

/// Review M3: a resume at another reasoning level than the last request's is counted as
/// a cold cache. A resumed session has no cache clock (the last request's instant is not
/// in the log), so its first step is always a cold epoch, at any level.
#[tokio::test]
async fn a_resume_starts_a_cold_epoch_at_any_reasoning_level() {
    use tautly_core::ReasoningLevel;
    for level in [ReasoningLevel::Auto, ReasoningLevel::High] {
        let fx = Fx::new();
        let small: String = (0..20).map(|i| format!("line {i}\n")).collect();
        fx.write("small.txt", &small);
        let read = || call("r", "read", json!({"path": "small.txt", "full": true}));
        let mock = MockProvider::new(vec![
            tools_step(vec![read()]),
            tools_step(vec![read()]),
            reply("read twice"),
            reply("resumed"),
            reply("spare"),
        ]);
        let mut high = config("anthropic/a1");
        high.reasoning = ReasoningLevel::High;
        let mut run = start(fx.switching_builder(high, &mock));
        let id = run.engine.session_id().to_string();
        let t1 = run.turn_from_start("read it twice").await;
        assert_eq!(finish_reason(&t1), FinishReason::Completed, "{t1:?}");
        run.stop().await;

        let mut cfg = config("anthropic/a1");
        cfg.reasoning = level;
        let mut run = start(fx.resumed(cfg, &mock, &id));
        let t2 = run.turn_from_start("after resume").await;
        let cold = t2.iter().any(|e| {
            matches!(
                e,
                Event::ContextCollected {
                    reason: MutationReason::ColdCache,
                    ..
                }
            )
        });
        assert!(cold, "{level}: {t2:?}");
        assert_eq!(finish_reason(&t2), FinishReason::Completed, "{t2:?}");
        run.stop().await;
    }
}
