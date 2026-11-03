//! Mid-session model switch (spec 7.11.5): `Command::SetModel` against scripted providers
//! (`MockProvider`) built by an injected provider factory, so no test reads a real key.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tautly_core::record::session_model;
use tautly_core::{
    Command, Config, Engine, EngineBuilder, ErrorKind, Event, FinishReason, MutationReason,
    ProviderFactory, ProviderSetupError, RoutingConfig, SessionRecord, provider_label,
};
use tautly_provider::{
    CanonicalRequest, Catalog, Content, MockProvider, MockTurn, ModelId, Provider, ProviderError,
    ProviderEvent, RawUsage, StopReason,
};
use tautly_sandbox::Mode;
use tautly_session::SessionReader;
use tempfile::TempDir;
use tokio::sync::mpsc;

const WAIT: Duration = Duration::from_secs(30);

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

fn test_catalog() -> Catalog {
    let toml = [
        entry("anthropic/a1", 200_000, "str_replace", Some(3.0)),
        entry("anthropic/a2", 200_000, "str_replace", Some(3.0)),
        entry("openai/o1", 200_000, "patch", Some(3.0)),
        entry("openai/small", 128_000, "patch", Some(3.0)),
        entry("openai/tiny", 32_000, "patch", Some(3.0)),
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

fn call(id: &str, name: &str, args: Value) -> ProviderEvent {
    ProviderEvent::ToolCall {
        id: id.into(),
        name: name.into(),
        input_json: args.to_string(),
    }
}

/// A response with tool calls, without usage (mock numbers would skew the estimator).
fn tools_step(calls: Vec<ProviderEvent>) -> MockTurn {
    let mut evs = calls;
    evs.push(ProviderEvent::Done {
        stop: StopReason::ToolUse,
    });
    MockTurn::Events(evs)
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

    fn write(&self, rel: &str, text: &str) {
        std::fs::write(self.dir.path().join(rel), text).unwrap();
    }

    fn builder(&self, cfg: Config, mock: &MockProvider) -> EngineBuilder {
        EngineBuilder {
            provider: Some(Arc::new(mock.clone())),
            provider_factory: Some(factory(mock)),
            data_home: Some(self.home.path().to_path_buf()),
            ..EngineBuilder::new(cfg, self.dir.path())
        }
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
    async fn send(&self, cmd: Command) {
        self.engine.send(cmd).await.unwrap();
    }

    async fn say(&self, text: &str) {
        self.send(Command::SendMessage { text: text.into() }).await;
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

    async fn turn_with(&mut self, text: &str) -> Vec<Event> {
        self.say(text).await;
        self.turn().await
    }

    /// `SetModel`, then the events up to its answer (`ModelChanged` or an `Error`).
    async fn set_model(&mut self, model: &str) -> Vec<Event> {
        self.send(Command::SetModel { model: m(model) }).await;
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
    let mut run = start(fx.builder(config("anthropic/a1"), &mock));

    let answer = run.set_model("openai/o1").await;
    assert_eq!(
        answer.last(),
        Some(&Event::ModelChanged {
            model: m("openai/o1"),
            provider: "openai".into(),
        }),
        "{answer:?}"
    );
    // Applied before any message: the first request already goes to the new model.
    let events = run.turn_with("hi").await;
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
        ..fx.builder(config("anthropic/a1"), &mock)
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
        model: m("openai/o1"),
    })
    .await;
    run.send(Command::Approve {
        request_id: request_id.clone(),
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
    let events = run.turn_with("again").await;
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
        ..fx.builder(config("anthropic/a1"), &mock)
    });

    for (model, hint_has) in [
        ("acme/x", "<provider>/<model>"),
        ("anthropic/not-in-the-catalog", "catalog"),
        ("compat:nowhere/llama", "[[providers.compat]]"),
        ("openai/o1", "tautly auth add openai"),
    ] {
        let answer = run.set_model(model).await;
        match answer.last() {
            Some(Event::Error {
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
    let events = run.turn_with("hi").await;
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
    let mut run = start(fx.builder(cfg, &mock));
    run.turn_with("hi").await;
    run.set_model("openai/gpt-5").await;
    run.turn_with("again").await;
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
    let mut run = start(fx.builder(cfg, &mock));
    run.set_model("openai/gpt-5").await;
    run.turn_with("hi").await;
    assert_eq!(
        edit_required(&mock.requests()[0]),
        json!(["path", "old_text", "new_text"])
    );
}

/// History long enough to compact (more than the 4 most recent turns) whose first
/// message (about 24k tokens) puts a 32k window over its 80% threshold.
async fn fill_for_compaction(run: &mut Run) {
    let big = format!("BIG-MARKER {}", "lorem ipsum dolor sit amet ".repeat(3555));
    run.turn_with(&big).await;
    run.turn_with("two").await;
    run.turn_with("three").await;
}

#[tokio::test]
async fn side_model_recomputed_unless_routing_set() {
    for (routing, side) in [(None, "openai/tiny"), (Some("openai/mini"), "openai/mini")] {
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
        let mut run = start(fx.builder(cfg, &mock));
        fill_for_compaction(&mut run).await;
        run.set_model("openai/tiny").await;
        let events = run.turn_with("four").await;
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
        assert_eq!(summary.model, m(side), "routing {routing:?}");
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
    let mut run = start(fx.builder(config("anthropic/a1"), &mock));
    let t1 = run.turn_with("read it twice").await;
    assert_eq!(finish_reason(&t1), FinishReason::Completed, "{t1:?}");
    run.send(Command::Pin {
        text: "PIN-MARKER".into(),
    })
    .await;

    // Warm cache, same model: the superseded read stays and the pin waits in the tail.
    let t2 = run.turn_with("anything").await;
    assert!(
        !t2.iter()
            .any(|e| matches!(e, Event::ContextCollected { .. })),
        "{t2:?}"
    );
    let warm = mock.requests()[3].clone();
    assert!(tail_text(&warm).contains("PIN-MARKER"));

    run.set_model("anthropic/a2").await;
    let t3 = run.turn_with("after the switch").await;
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
    let mut run = start(fx.builder(config("anthropic/a1"), &mock));
    // About 150k tokens: 5 messages of 120 KB (4 bytes a token).
    let filler = "lorem ipsum dolor sit amet ".repeat(120_000 / 27);
    for i in 0..5 {
        let events = run.turn_with(&format!("M{i} {filler}")).await;
        assert!(
            !events.iter().any(|e| matches!(e, Event::Compacted { .. })),
            "compacted before the switch: {events:?}"
        );
    }
    run.set_model("openai/small").await;
    let events = run.turn_with("go on").await;
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
    let mut run = start(fx.builder(config("anthropic/a1"), &mock));
    run.turn_with("one").await;
    // Same provider: the signed thinking travels on.
    run.set_model("anthropic/a2").await;
    run.turn_with("two").await;
    // Another provider: Anthropic's thinking is stripped.
    run.set_model("openai/o1").await;
    run.turn_with("three").await;
    // And back: OpenAI's reasoning item is stripped.
    run.set_model("anthropic/a1").await;
    run.turn_with("four").await;

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
    let mut run = start(fx.builder(config("anthropic/a1"), &mock));
    run.turn_with("one").await;
    run.set_model("anthropic/a2").await;
    let events = run.turn_with("two").await;

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
}

#[tokio::test]
async fn model_set_survives_resume() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![]);
    let mut run = start(fx.builder(config("anthropic/a1"), &mock));
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
    let mut run = start(fx.builder(cfg, &mock));
    let t1 = run.turn_with("one").await;
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
    let t2 = run.turn_with("two").await;
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
    let t3 = run.turn_with("three").await;
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
    let mut run = start(fx.builder(cfg, &mock));
    run.turn_with("one").await;

    run.set_model("openai/free").await;
    let first = run.until(|e| matches!(e, Event::Warning { .. })).await;
    assert!(warnings(&first)[0].contains("no known price"));
    // Unpriced: only tokens count, so 5000 tokens are no USD spend.
    let t2 = run.turn_with("two").await;
    assert_eq!(finish_reason(&t2), FinishReason::Completed);

    // Back to the priced model: USD is enforced again, the spend so far unchanged.
    run.set_model("anthropic/pricey").await;
    let t3 = run.turn_with("three").await;
    assert!(
        warnings(&t3)
            .iter()
            .any(|w| w.contains("session budget exhausted") && w.contains("$0.0600 of $0.0500")),
        "{t3:?}"
    );
    let t4 = run.turn_with("four").await;
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
