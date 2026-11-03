use serde_json::{Value, json};
use tautly_provider::EditFormat;
use tautly_sandbox::{Effect, LocalSandbox};
use tautly_tools::{
    ContextOp, MAX_PIN_BYTES, PinTool, PlanStep, StatePatch, StateTool, StepStatus, Tool, ToolCx,
    ToolResult, parse_context_op,
};
use tempfile::TempDir;

async fn run(tool: &dyn Tool, args: Value) -> ToolResult {
    let dir = TempDir::new().unwrap();
    let sb = LocalSandbox::new(dir.path().to_path_buf()).unwrap();
    let cx = ToolCx::new(EditFormat::StrReplace, dir.path().join(".spill"));
    tool.run(args, &sb, &cx).await
}

#[test]
fn pin_parses_context_op() {
    assert_eq!(
        parse_context_op("pin", &json!({"text": "never edit generated/"})),
        Some(ContextOp::Pin("never edit generated/".into()))
    );
    assert_eq!(parse_context_op("pin", &json!({})), None);
    assert_eq!(parse_context_op("pin", &json!({"text": ""})), None);
    assert_eq!(parse_context_op("pin", &json!({"text": "a", "x": 1})), None);
    assert_eq!(parse_context_op("read", &json!({"text": "a"})), None);
}

#[test]
fn state_parses_context_op() {
    let args = json!({
        "plan": [
            {"text": "write tests", "status": "done"},
            {"text": "implement", "status": "in_progress"},
            {"text": "ship", "status": "pending"}
        ],
        "decisions_add": ["use tree-sitter"],
        "questions_add": ["which grammar?"],
        "questions_resolve": [0, 2]
    });
    let expected = StatePatch {
        plan: Some(vec![
            PlanStep {
                text: "write tests".into(),
                status: StepStatus::Done,
            },
            PlanStep {
                text: "implement".into(),
                status: StepStatus::InProgress,
            },
            PlanStep {
                text: "ship".into(),
                status: StepStatus::Pending,
            },
        ]),
        decisions_add: vec!["use tree-sitter".into()],
        questions_add: vec!["which grammar?".into()],
        questions_resolve: vec![0, 2],
    };
    assert_eq!(
        parse_context_op("state", &args),
        Some(ContextOp::StateUpdate(expected))
    );
}

#[test]
fn state_fields_are_all_optional() {
    assert_eq!(
        parse_context_op("state", &json!({})),
        Some(ContextOp::StateUpdate(StatePatch::default()))
    );
    assert_eq!(
        parse_context_op("state", &json!({"decisions_add": ["x"]})),
        Some(ContextOp::StateUpdate(StatePatch {
            decisions_add: vec!["x".into()],
            ..StatePatch::default()
        }))
    );
}

#[test]
fn state_rejects_unknown_fields() {
    assert_eq!(parse_context_op("state", &json!({"bogus": 1})), None);
    assert_eq!(
        parse_context_op(
            "state",
            &json!({"plan": [{"text": "a", "status": "done", "extra": 1}]})
        ),
        None
    );
    assert_eq!(
        parse_context_op(
            "state",
            &json!({"plan": [{"text": "a", "status": "blocked"}]})
        ),
        None
    );
}

#[tokio::test]
async fn pin_run_returns_json_body_without_effects() {
    let r = run(&PinTool, json!({"text": "keep API stable"})).await;
    assert!(!r.is_error, "{}", r.output);
    assert!(r.effects.is_empty());
    let body: Value = serde_json::from_str(&r.output).expect("JSON body");
    assert_eq!(body["ok"], json!(true));
}

#[tokio::test]
async fn state_run_returns_json_body_without_effects() {
    let r = run(&StateTool, json!({"decisions_add": ["a"]})).await;
    assert!(!r.is_error, "{}", r.output);
    assert!(r.effects.is_empty());
    let body: Value = serde_json::from_str(&r.output).expect("JSON body");
    assert_eq!(body["ok"], json!(true));
}

#[tokio::test]
async fn bad_args_are_error_results() {
    assert!(run(&PinTool, json!({"text": ""})).await.is_error);
    assert!(run(&PinTool, json!({"nope": 1})).await.is_error);
    assert!(run(&StateTool, json!({"bogus": 1})).await.is_error);
}

#[test]
fn effects_are_read_only_and_specs_are_strict() {
    assert_eq!(
        PinTool.effect(&json!({"text": "a"})).unwrap(),
        Effect::ReadOnly
    );
    assert_eq!(StateTool.effect(&json!({})).unwrap(), Effect::ReadOnly);
    assert!(PinTool.effect(&json!({"nope": 1})).is_err());
    assert!(StateTool.effect(&json!({"nope": 1})).is_err());
    let caps = tautly_provider::Capabilities::conservative();
    for t in [&PinTool as &dyn Tool, &StateTool] {
        let s = t.spec(&caps);
        assert_eq!(s.name, t.name());
        assert_eq!(s.schema["additionalProperties"], json!(false));
    }
}

#[tokio::test]
async fn pin_over_2_kb_is_refused_with_a_clear_error() {
    let big = "x".repeat(MAX_PIN_BYTES + 1);
    assert_eq!(parse_context_op("pin", &json!({"text": big})), None);
    let r = run(&PinTool, json!({"text": big})).await;
    assert!(r.is_error);
    assert!(r.output.contains("2048"), "{}", r.output);
    assert!(r.output.contains("2049"), "{}", r.output);
    // Exactly the limit is fine.
    let ok = "y".repeat(MAX_PIN_BYTES);
    assert_eq!(
        parse_context_op("pin", &json!({"text": ok})),
        Some(ContextOp::Pin(ok.clone()))
    );
}
