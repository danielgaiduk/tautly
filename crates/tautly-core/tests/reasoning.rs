//! Reasoning enablement (spec 4.7): the `reasoning` level resolved against the model's
//! catalog reasoning style into each main request, and `Command::SetReasoning`.

use std::sync::Arc;
use std::time::Duration;

use tautly_core::{Command, Config, Engine, EngineBuilder, Event, ReasoningLevel};
use tautly_provider::{
    Catalog, Effort, MockProvider, MockTurn, ModelId, ProviderEvent, Reasoning, ReasoningStyle,
    StopReason,
};
use tempfile::TempDir;
use tokio::sync::mpsc;

const WAIT: Duration = Duration::from_secs(30);

#[test]
fn auto_is_medium_for_models_that_reason_and_off_otherwise() {
    use ReasoningLevel::*;
    let effort = |e| Some(Reasoning::Effort(e));
    for (level, want) in [
        (Auto, effort(Effort::Medium)),
        (Low, effort(Effort::Low)),
        (Medium, effort(Effort::Medium)),
        (High, effort(Effort::High)),
        (Off, None),
    ] {
        assert_eq!(level.resolve(ReasoningStyle::Effort), want, "{level}");
    }
    for (level, want) in [
        (Auto, Some(Reasoning::Budget(12_288))),
        (Low, Some(Reasoning::Budget(4_096))),
        (Medium, Some(Reasoning::Budget(12_288))),
        (High, Some(Reasoning::Budget(32_768))),
        (Off, None),
    ] {
        assert_eq!(level.resolve(ReasoningStyle::Budget), want, "{level}");
    }
    // A model that cannot reason gets nothing, whatever was asked.
    for level in [Auto, Off, Low, Medium, High] {
        assert_eq!(level.resolve(ReasoningStyle::None), None, "{level}");
    }
}

#[test]
fn levels_parse_and_print_by_name() {
    for (name, level) in [
        ("auto", ReasoningLevel::Auto),
        ("off", ReasoningLevel::Off),
        ("low", ReasoningLevel::Low),
        ("medium", ReasoningLevel::Medium),
        ("high", ReasoningLevel::High),
    ] {
        assert_eq!(name.parse::<ReasoningLevel>(), Ok(level));
        assert_eq!(level.to_string(), name);
    }
    let err = "max".parse::<ReasoningLevel>().unwrap_err();
    assert!(
        err.contains("max") && err.contains(ReasoningLevel::NAMES),
        "{err}"
    );
}

#[test]
fn the_built_in_catalog_marks_current_anthropic_models_as_effort_style() {
    // Their API rejects a fixed thinking budget; effort is the control (spec 4.7).
    let catalog = Catalog::builtin();
    for id in [
        "anthropic/claude-sonnet-5-5",
        "anthropic/claude-opus-5-5",
        "anthropic/claude-haiku-5-5",
        "anthropic/claude-fable-5-1",
    ] {
        let caps = catalog.get(&ModelId::from(id)).caps;
        assert_eq!(caps.reasoning, ReasoningStyle::Effort, "{id}");
    }
}

// ---- the engine --------------------------------------------------------------------

fn catalog(style: &str) -> Catalog {
    Catalog::from_toml(&format!(
        "[models.\"mock/thinker\"]\ncontext_window = 100000\nmax_output = 64000\n\
         parallel_tools = true\nreasoning = \"{style}\"\ncache = \"none\"\nimages = false\n\
         edit_format = \"str_replace\"\n"
    ))
    .unwrap()
}

fn done(text: &str) -> MockTurn {
    MockTurn::Events(vec![
        ProviderEvent::TextDelta(text.into()),
        ProviderEvent::Done {
            stop: StopReason::EndTurn,
        },
    ])
}

struct Run {
    _dir: TempDir,
    _home: TempDir,
    engine: Engine,
    events: mpsc::Receiver<Event>,
    main: MockProvider,
}

fn start(style: &str, level: ReasoningLevel, turns: usize) -> Run {
    let dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let main = MockProvider::new((0..turns).map(|_| done("ok")).collect());
    let cfg = Config {
        model: ModelId::from("mock/thinker"),
        reasoning: level,
        catalog_overrides: Some(catalog(style)),
        ..Config::default()
    };
    let (engine, events) = EngineBuilder {
        provider: Some(Arc::new(main.clone())),
        side_provider: Some(Arc::new(MockProvider::new(vec![]))),
        data_home: Some(home.path().to_path_buf()),
        ..EngineBuilder::new(cfg, dir.path())
    }
    .build()
    .unwrap();
    Run {
        _dir: dir,
        _home: home,
        engine,
        events,
        main,
    }
}

impl Run {
    async fn turn(&mut self, text: &str) {
        self.engine
            .send(Command::SendMessage {
                id: None,
                text: text.into(),
            })
            .await
            .unwrap();
        loop {
            let ev = tokio::time::timeout(WAIT, self.events.recv())
                .await
                .expect("timed out")
                .expect("events closed");
            if matches!(ev, Event::TurnFinished { .. }) {
                return;
            }
        }
    }

    fn last_reasoning(&self) -> Option<Reasoning> {
        self.main.requests().last().unwrap().reasoning.clone()
    }
}

#[tokio::test]
async fn requests_carry_the_resolved_level_and_set_reasoning_changes_it() {
    let mut run = start("effort", ReasoningLevel::Auto, 4);
    run.turn("one").await;
    assert_eq!(
        run.last_reasoning(),
        Some(Reasoning::Effort(Effort::Medium))
    );

    for (level, want) in [
        (ReasoningLevel::High, Some(Reasoning::Effort(Effort::High))),
        (ReasoningLevel::Off, None),
        (ReasoningLevel::Low, Some(Reasoning::Effort(Effort::Low))),
    ] {
        run.engine
            .send(Command::SetReasoning { level })
            .await
            .unwrap();
        run.turn("again").await;
        assert_eq!(run.last_reasoning(), want, "{level}");
    }
}

#[tokio::test]
async fn budget_models_get_the_levels_budget_and_others_nothing() {
    let mut run = start("budget", ReasoningLevel::High, 1);
    run.turn("one").await;
    assert_eq!(run.last_reasoning(), Some(Reasoning::Budget(32_768)));

    let mut run = start("none", ReasoningLevel::High, 1);
    run.turn("one").await;
    assert_eq!(run.last_reasoning(), None);
}
