//! Reasoning enablement (spec 4.7): the request fields each adapter sends per level, the
//! caps on thinking budgets, and reasoning items with signatures round-tripping beside
//! tool calls while reasoning is on.

use serde_json::{Value, json};
use tautly_provider::*;

const LEVELS: [(Effort, &str, u32); 3] = [
    (Effort::Low, "low", 4_096),
    (Effort::Medium, "medium", 12_288),
    (Effort::High, "high", 32_768),
];

fn req(model: &str, max_output: u32, reasoning: Option<Reasoning>) -> CanonicalRequest {
    CanonicalRequest {
        model: model.into(),
        system: vec![Block::Text("sys".into())],
        tools: vec![ToolSpec {
            name: "read_file".into(),
            description: "read".into(),
            schema: json!({"type": "object"}),
        }],
        messages: vec![Message {
            role: Role::User,
            content: vec![Content::Text("q".into())],
        }],
        max_output,
        reasoning,
        cache_breakpoints: vec![],
        volatile_tail: false,
    }
}

/// A user question, an assistant step that reasoned (signed) and called a tool, and the
/// tool's result: what the engine sends back on the next step.
fn tool_round(model: &str, reasoning: Option<Reasoning>, redacted: &str) -> CanonicalRequest {
    let mut r = req(model, 64_000, reasoning);
    r.messages.push(Message {
        role: Role::Assistant,
        content: vec![
            Content::Reasoning {
                text: "Need the file.".into(),
                signature: Some("SIG-1".into()),
            },
            Content::RedactedReasoning {
                data: redacted.into(),
            },
            Content::ToolUse {
                id: "call_1".into(),
                name: "read_file".into(),
                input: json!({"path": "a.rs"}),
            },
        ],
    });
    r.messages.push(Message {
        role: Role::User,
        content: vec![Content::ToolResult {
            id: "call_1".into(),
            output: "body".into(),
            is_error: false,
        }],
    });
    r
}

#[test]
fn effort_levels_mean_4k_12k_32k_tokens() {
    for (e, _, n) in LEVELS {
        assert_eq!(e.budget_tokens(), n);
    }
}

// ---- anthropic ----------------------------------------------------------------------

#[test]
fn anthropic_effort_uses_adaptive_thinking_and_output_effort() {
    for (e, name, _) in LEVELS {
        let w = anthropic::to_wire(&req(
            "anthropic/claude-opus-5-5",
            64_000,
            Some(Reasoning::Effort(e)),
        ));
        assert_eq!(
            w["thinking"],
            json!({"type": "adaptive", "display": "summarized"})
        );
        assert_eq!(w["output_config"], json!({"effort": name}));
    }
    // Off: nothing is sent (current models then use their own default).
    let w = anthropic::to_wire(&req("anthropic/claude-opus-5-5", 64_000, None));
    assert!(w.get("thinking").is_none() && w.get("output_config").is_none());
}

#[test]
fn anthropic_budget_is_capped_below_max_tokens() {
    for (e, _, n) in LEVELS {
        let w = anthropic::to_wire(&req(
            "anthropic/claude-haiku-4-5",
            64_000,
            Some(Reasoning::Budget(e.budget_tokens())),
        ));
        assert_eq!(
            w["thinking"],
            json!({"type": "enabled", "budget_tokens": n})
        );
        assert!(w.get("output_config").is_none());
    }
    // 32k with a 16k output limit: just below the limit.
    let w = anthropic::to_wire(&req(
        "anthropic/claude-haiku-4-5",
        16_000,
        Some(Reasoning::Budget(32_768)),
    ));
    assert_eq!(w["thinking"]["budget_tokens"], 15_999);
    assert_eq!(w["max_tokens"], 16_000);
    // Below the API's 1024 minimum once capped: no thinking at all.
    let w = anthropic::to_wire(&req(
        "anthropic/claude-haiku-4-5",
        1_024,
        Some(Reasoning::Budget(4_096)),
    ));
    assert!(w.get("thinking").is_none());
}

#[test]
fn anthropic_reasoning_sends_no_sampling_or_forced_tool_choice() {
    for r in [Reasoning::Effort(Effort::High), Reasoning::Budget(12_288)] {
        let w = anthropic::to_wire(&req("anthropic/claude-opus-5-5", 64_000, Some(r)));
        for key in ["temperature", "top_p", "top_k", "tool_choice"] {
            assert!(w.get(key).is_none(), "{key} sent with thinking");
        }
    }
}

#[test]
fn anthropic_thinking_and_signature_round_trip_with_tools() {
    let w = anthropic::to_wire(&tool_round(
        "anthropic/claude-opus-5-5",
        Some(Reasoning::Effort(Effort::Medium)),
        "REDACTED==",
    ));
    assert_eq!(w["thinking"]["type"], "adaptive");
    // The thinking blocks lead the assistant turn, signature and opaque data verbatim,
    // then the tool call; the result answers it.
    assert_eq!(
        w["messages"][1]["content"],
        json!([
            {"type": "thinking", "thinking": "Need the file.", "signature": "SIG-1"},
            {"type": "redacted_thinking", "data": "REDACTED=="},
            {"type": "tool_use", "id": "call_1", "name": "read_file", "input": {"path": "a.rs"}},
        ])
    );
    assert_eq!(w["messages"][2]["content"][0]["type"], "tool_result");
    assert_eq!(w["messages"][2]["content"][0]["tool_use_id"], "call_1");
}

// ---- openai -------------------------------------------------------------------------

#[test]
fn openai_effort_levels() {
    for (e, name, n) in LEVELS {
        let w = openai::to_wire(&req("openai/gpt-5", 128_000, Some(Reasoning::Effort(e))));
        assert_eq!(w["reasoning"], json!({"effort": name, "summary": "auto"}));
        // A budget of the same level picks the same effort.
        let w = openai::to_wire(&req("openai/gpt-5", 128_000, Some(Reasoning::Budget(n))));
        assert_eq!(w["reasoning"]["effort"], name, "budget {n}");
    }
    assert!(
        openai::to_wire(&req("openai/gpt-5", 128_000, None))
            .get("reasoning")
            .is_none()
    );
}

#[test]
fn openai_encrypted_reasoning_round_trips_with_tools() {
    let item =
        json!({"type": "reasoning", "id": "rs_1", "encrypted_content": "ENC==", "summary": []});
    let w = openai::to_wire(&tool_round(
        "openai/gpt-5",
        Some(Reasoning::Effort(Effort::High)),
        &item.to_string(),
    ));
    assert_eq!(w["reasoning"]["effort"], "high");
    assert_eq!(w["include"], json!(["reasoning.encrypted_content"]));
    let input = w["input"].as_array().unwrap();
    assert_eq!(input[1], item);
    assert_eq!(input[2]["type"], "function_call");
    assert_eq!(input[2]["call_id"], "call_1");
    assert_eq!(input[3]["type"], "function_call_output");
}

// ---- google -------------------------------------------------------------------------

fn thinking_budget(w: &Value) -> Option<u64> {
    w["generationConfig"]["thinkingConfig"]["thinkingBudget"].as_u64()
}

#[test]
fn google_budget_levels_are_capped_below_max_output() {
    for (e, _, n) in LEVELS {
        for r in [Reasoning::Budget(n), Reasoning::Effort(e)] {
            let w = google::to_wire(&req("google/gemini-3-pro", 64_000, Some(r)));
            assert_eq!(thinking_budget(&w), Some(u64::from(n)));
            assert_eq!(
                w["generationConfig"]["thinkingConfig"]["includeThoughts"],
                true
            );
        }
    }
    let w = google::to_wire(&req(
        "google/gemini-3-pro",
        8_000,
        Some(Reasoning::Budget(32_768)),
    ));
    assert_eq!(thinking_budget(&w), Some(7_999));
    let w = google::to_wire(&req("google/gemini-3-pro", 64_000, None));
    assert!(w["generationConfig"].get("thinkingConfig").is_none());
}

#[test]
fn google_thought_signature_round_trips_with_tools() {
    let w = google::to_wire(&tool_round(
        "google/gemini-3-pro",
        Some(Reasoning::Budget(12_288)),
        "anthropic-only",
    ));
    assert_eq!(thinking_budget(&w), Some(12_288));
    assert_eq!(
        w["contents"][1]["parts"],
        json!([{"functionCall": {"name": "read_file", "args": {"path": "a.rs"}}, "thoughtSignature": "SIG-1"}])
    );
    assert_eq!(
        w["contents"][2]["parts"][0]["functionResponse"]["name"],
        "read_file"
    );
}

// ---- compat -------------------------------------------------------------------------

#[test]
fn compat_sends_reasoning_effort_only_when_asked_for_an_effort() {
    for (e, name, n) in LEVELS {
        let w = compat::to_wire(&req("compat:vllm/q", 8_000, Some(Reasoning::Effort(e))));
        assert_eq!(w["reasoning_effort"], name);
        // No portable budget field exists.
        let w = compat::to_wire(&req("compat:vllm/q", 8_000, Some(Reasoning::Budget(n))));
        assert!(w.get("reasoning_effort").is_none() && w.get("reasoning").is_none());
    }
    let w = compat::to_wire(&req("compat:vllm/q", 8_000, None));
    assert!(w.get("reasoning_effort").is_none());
}
