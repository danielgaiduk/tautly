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
        // Drop thinking blocks a history edit invalidated instead of failing (C1).
        assert_eq!(
            w["thinking"],
            json!({"type": "adaptive", "display": "summarized",
                   "block_binding": {"prefix_mismatch_behavior": "drop_block"}})
        );
        assert_eq!(w["output_config"], json!({"effort": name}));
    }
    // Off: nothing is sent (current models then use their own default).
    let w = anthropic::to_wire(&req("anthropic/claude-opus-5-5", 64_000, None));
    assert!(w.get("thinking").is_none() && w.get("output_config").is_none());
}

#[test]
fn anthropic_budget_is_capped_at_half_the_output_limit() {
    for (e, _, n) in LEVELS {
        let n = n.min(32_000);
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
    // 32k with a 16k output limit: half of it, so the answer keeps room.
    let w = anthropic::to_wire(&req(
        "anthropic/claude-haiku-4-5",
        16_000,
        Some(Reasoning::Budget(32_768)),
    ));
    assert_eq!(w["thinking"]["budget_tokens"], 8_000);
    assert_eq!(w["max_tokens"], 16_000);
    // Budget thinking runs no history check: no block_binding.
    assert!(w["thinking"].get("block_binding").is_none());
    // Below the API's 1024 minimum once capped: no thinking at all.
    let w = anthropic::to_wire(&req(
        "anthropic/claude-haiku-4-5",
        2_000,
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

#[test]
fn anthropic_beta_headers_follow_the_thinking_mode() {
    const BINDING: &str = "thinking-binding-controls-2026-08-01";
    const INTERLEAVED: &str = "interleaved-thinking-2025-05-14";
    // Adaptive thinking: the binding controls that make `block_binding` valid.
    let adaptive = req(
        "anthropic/claude-opus-5-5",
        64_000,
        Some(Reasoning::Effort(Effort::Low)),
    );
    assert_eq!(anthropic::beta_headers(&adaptive), [BINDING]);
    // Budget thinking with tools: interleaved thinking between tool calls (M5).
    let budget = req(
        "anthropic/claude-haiku-4-5",
        64_000,
        Some(Reasoning::Budget(4_096)),
    );
    assert_eq!(anthropic::beta_headers(&budget), [INTERLEAVED]);
    let mut no_tools = budget.clone();
    no_tools.tools.clear();
    assert!(anthropic::beta_headers(&no_tools).is_empty());
    // A budget capped away (no thinking sent) needs no header either.
    let mut tiny = budget.clone();
    tiny.max_output = 1_500;
    assert!(anthropic::beta_headers(&tiny).is_empty());
    assert!(anthropic::beta_headers(&req("anthropic/claude-opus-5-5", 64_000, None)).is_empty());
}

#[tokio::test]
async fn anthropic_adaptive_request_carries_the_binding_beta_header_and_field() {
    use futures::StreamExt;
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;
    use wiremock::matchers::{body_partial_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header(
            "anthropic-beta",
            "thinking-binding-controls-2026-08-01",
        ))
        .and(body_partial_json(json!({"thinking": {"type": "adaptive",
            "block_binding": {"prefix_mismatch_behavior": "drop_block"}}})))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(""),
        )
        .expect(1)
        .mount(&server)
        .await;
    let p = anthropic::AnthropicProvider::new(
        "k".into(),
        server.uri().parse().unwrap(),
        Arc::new(Catalog::builtin()),
    );
    let r = req(
        "anthropic/claude-opus-5-5",
        64_000,
        Some(Reasoning::Effort(Effort::High)),
    );
    let _: Vec<_> = p.stream(r, CancellationToken::new()).collect().await;
    server.verify().await;
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
            assert_eq!(thinking_budget(&w), Some(u64::from(n.min(32_000))));
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
    assert_eq!(thinking_budget(&w), Some(4_000));
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

#[tokio::test]
async fn google_clamps_the_budget_to_the_models_documented_range() {
    use futures::StreamExt;
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let catalog = Catalog::builtin();
    assert_eq!(
        catalog.thinking_budget_max(&"google/gemini-3-flash".into()),
        Some(24_576)
    );
    for (model, want) in [
        ("google/gemini-3-flash", 24_576),
        ("google/gemini-3-pro", 32_000),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(body_partial_json(
                json!({"generationConfig": {"thinkingConfig": {"thinkingBudget": want}}}),
            ))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(""),
            )
            .expect(1)
            .mount(&server)
            .await;
        let p = google::GoogleProvider::new(
            "k".into(),
            server.uri().parse().unwrap(),
            Arc::new(Catalog::builtin()),
        );
        let r = req(model, 64_000, Some(Reasoning::Effort(Effort::High)));
        let _: Vec<_> = p.stream(r, CancellationToken::new()).collect().await;
        server.verify().await;
    }
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
