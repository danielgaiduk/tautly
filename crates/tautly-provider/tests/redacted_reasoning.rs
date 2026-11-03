//! Checkpoint fix: adapters other than Anthropic drop redacted reasoning on the wire.

use tautly_provider::*;

fn req(model: &str) -> CanonicalRequest {
    CanonicalRequest {
        model: model.into(),
        system: vec![],
        tools: vec![],
        messages: vec![
            Message {
                role: Role::User,
                content: vec![Content::Text("q".into())],
            },
            Message {
                role: Role::Assistant,
                content: vec![
                    Content::RedactedReasoning {
                        data: "SECRET-OPAQUE".into(),
                    },
                    Content::Text("a".into()),
                ],
            },
        ],
        max_output: 100,
        reasoning: None,
        cache_breakpoints: vec![],
        volatile_tail: false,
    }
}

#[test]
fn non_anthropic_adapters_drop_redacted_reasoning() {
    let bodies = [
        openai::to_wire(&req("openai/gpt-5")),
        google::to_wire(&req("google/gemini-2.5-pro")),
        compat::to_wire(&req("compat:x/m")),
    ];
    for body in bodies {
        let s = body.to_string();
        assert!(!s.contains("SECRET-OPAQUE"), "{s}");
        assert!(s.contains("\"a\""), "assistant text kept: {s}");
    }
}
