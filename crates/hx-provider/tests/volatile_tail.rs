//! `CanonicalRequest::volatile_tail` is a marker for a later adapter option (checkpoint 3,
//! I6 prep). Until an adapter uses it, it must not change a single byte of any wire body.

use hx_provider::{
    Block, CachePoint, CanonicalRequest, Content, Message, Reasoning, Role, ToolSpec, anthropic,
    compat, google, openai,
};
use serde_json::json;

type ToWire = fn(&CanonicalRequest) -> serde_json::Value;

fn request(model: &str, volatile_tail: bool) -> CanonicalRequest {
    let user = |t: &str| Message {
        role: Role::User,
        content: vec![Content::Text(t.into())],
    };
    CanonicalRequest {
        model: model.into(),
        system: vec![Block::Text("You are hx.".into())],
        tools: vec![ToolSpec {
            name: "read".into(),
            description: "read a file".into(),
            schema: json!({"type": "object"}),
        }],
        messages: vec![
            user("fix the bug"),
            Message {
                role: Role::Assistant,
                content: vec![Content::ToolUse {
                    id: "c1".into(),
                    name: "read".into(),
                    input: json!({"path": "a.rs"}),
                }],
            },
            Message {
                role: Role::User,
                content: vec![Content::ToolResult {
                    id: "c1".into(),
                    output: "fn main() {}".into(),
                    is_error: false,
                }],
            },
            // The tail, after the last breakpoint.
            user("<working_state>task: fix the bug</working_state>"),
        ],
        max_output: 8_000,
        reasoning: Some(Reasoning::Budget(2_000)),
        cache_breakpoints: vec![
            CachePoint::AfterTools,
            CachePoint::AfterSystem,
            CachePoint::AfterMessage(2),
        ],
        volatile_tail,
    }
}

#[test]
fn no_adapter_changes_its_output_for_the_marker() {
    let wires: [(&str, ToWire); 4] = [
        ("anthropic/claude-sonnet-4-5", anthropic::to_wire),
        ("openai/gpt-5", openai::to_wire),
        ("google/gemini-2.5-pro", google::to_wire),
        ("compat:local/model", compat::to_wire),
    ];
    for (model, to_wire) in wires {
        assert_eq!(
            to_wire(&request(model, true)),
            to_wire(&request(model, false)),
            "{model}"
        );
    }
}

#[test]
fn the_marker_defaults_to_false_when_absent_from_a_stored_request() {
    let mut v = serde_json::to_value(request("openai/gpt-5", true)).unwrap();
    assert_eq!(v["volatile_tail"], json!(true));
    v.as_object_mut().unwrap().remove("volatile_tail");
    let back: CanonicalRequest = serde_json::from_value(v).unwrap();
    assert!(!back.volatile_tail);
}
