use std::sync::Arc;
use std::time::{Duration, SystemTime};

use futures::StreamExt;
use serde_json::json;
use tautly_provider::anthropic::{AnthropicProvider, to_wire};
use tautly_provider::*;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!(
        "{}/tests/fixtures/anthropic/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

fn base_req() -> CanonicalRequest {
    CanonicalRequest {
        model: "anthropic/claude-sonnet-4-5".into(),
        system: vec![Block::Text("You are Tautly.".into())],
        tools: vec![
            ToolSpec {
                name: "a".into(),
                description: "tool a".into(),
                schema: json!({"type":"object"}),
            },
            ToolSpec {
                name: "b".into(),
                description: "tool b".into(),
                schema: json!({"type":"object"}),
            },
        ],
        messages: (0..4)
            .map(|i| Message {
                role: if i % 2 == 0 {
                    Role::User
                } else {
                    Role::Assistant
                },
                content: vec![Content::Text(format!("m{i}"))],
            })
            .collect(),
        max_output: 16_000,
        reasoning: None,
        cache_breakpoints: vec![],
        volatile_tail: false,
    }
}

fn req_with(points: Vec<CachePoint>) -> CanonicalRequest {
    CanonicalRequest {
        cache_breakpoints: points,
        ..base_req()
    }
}

fn req_reasoning(r: Reasoning) -> CanonicalRequest {
    CanonicalRequest {
        reasoning: Some(r),
        ..base_req()
    }
}

async fn run(server: &MockServer) -> Vec<Result<ProviderEvent, ProviderError>> {
    let p = AnthropicProvider::new(
        "k".into(),
        server.uri().parse().unwrap(),
        Arc::new(Catalog::builtin()),
    );
    p.stream(base_req(), CancellationToken::new())
        .collect()
        .await
}

fn sse(body: String) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(body)
}

async fn serve(resp: ResponseTemplate) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(resp)
        .mount(&server)
        .await;
    server
}

#[test]
fn cache_breakpoints_become_cache_control() {
    let w = to_wire(&req_with(vec![
        CachePoint::AfterSystem,
        CachePoint::AfterTools,
        CachePoint::AfterMessage(3),
    ]));
    assert_eq!(
        w["system"].as_array().unwrap().last().unwrap()["cache_control"]["type"],
        "ephemeral"
    );
    assert_eq!(
        w["tools"].as_array().unwrap().last().unwrap()["cache_control"]["type"],
        "ephemeral"
    );
    assert!(
        w["messages"][3]["content"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()["cache_control"]
            .is_object()
    );
    assert!(
        w["messages"][2]["content"][0]
            .get("cache_control")
            .is_none()
    );
    assert!(w["tools"][0].get("cache_control").is_none());
}

#[test]
fn reasoning_budget_maps_to_thinking() {
    let w = to_wire(&req_reasoning(Reasoning::Budget(8000)));
    assert_eq!(w["thinking"]["type"], "enabled");
    assert_eq!(w["thinking"]["budget_tokens"], 8000);
    assert!(to_wire(&base_req()).get("thinking").is_none());
}

#[test]
fn wire_basics_and_message_mapping() {
    let mut req = base_req();
    req.messages = vec![
        Message {
            role: Role::Assistant,
            content: vec![
                Content::Reasoning {
                    text: "hm".into(),
                    signature: Some("sig".into()),
                },
                Content::Reasoning {
                    text: "unsigned".into(),
                    signature: None,
                },
                Content::ToolUse {
                    id: "t1".into(),
                    name: "a".into(),
                    input: json!({"x":1}),
                },
            ],
        },
        Message {
            role: Role::User,
            content: vec![
                Content::Text("after".into()),
                Content::ToolResult {
                    id: "t1".into(),
                    output: "out".into(),
                    is_error: true,
                },
            ],
        },
    ];
    let w = to_wire(&req);
    assert_eq!(w["model"], "claude-sonnet-4-5");
    assert_eq!(w["stream"], true);
    assert_eq!(w["max_tokens"], 16_000);
    assert_eq!(
        w["system"][0],
        json!({"type":"text","text":"You are Tautly."})
    );
    assert_eq!(w["tools"][0]["input_schema"], json!({"type":"object"}));
    let a = &w["messages"][0];
    assert_eq!(a["role"], "assistant");
    assert_eq!(a["content"].as_array().unwrap().len(), 2);
    assert_eq!(
        a["content"][0],
        json!({"type":"thinking","thinking":"hm","signature":"sig"})
    );
    assert_eq!(
        a["content"][1],
        json!({"type":"tool_use","id":"t1","name":"a","input":{"x":1}})
    );
    let u = &w["messages"][1]["content"];
    assert_eq!(
        u[0],
        json!({"type":"tool_result","tool_use_id":"t1","content":"out","is_error":true})
    );
    assert_eq!(u[1], json!({"type":"text","text":"after"}));
}

#[tokio::test]
async fn sends_expected_request_and_parses_text() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", "k"))
        .and(header("anthropic-version", "2023-06-01"))
        .and(header("content-type", "application/json"))
        .and(body_partial_json(
            json!({"stream": true, "model": "claude-sonnet-4-5"}),
        ))
        .respond_with(sse(fixture("text.sse")))
        .expect(1)
        .mount(&server)
        .await;
    let evs: Vec<_> = run(&server).await.into_iter().map(Result::unwrap).collect();
    assert_eq!(evs[0], ProviderEvent::TextDelta("Hello".into()));
    assert_eq!(evs[1], ProviderEvent::TextDelta(", world".into()));
    assert_eq!(
        evs.last().unwrap(),
        &ProviderEvent::Done {
            stop: StopReason::EndTurn
        }
    );
}

#[tokio::test]
async fn parses_parallel_tool_use_fixture() {
    let server = serve(sse(fixture("tool_use_parallel.sse"))).await;
    let evs: Vec<_> = run(&server).await.into_iter().map(Result::unwrap).collect();
    assert_eq!(
        evs,
        vec![
            ProviderEvent::TextDelta("Checking.".into()),
            ProviderEvent::ToolCall {
                id: "toolu_01".into(),
                name: "get_weather".into(),
                input_json: r#"{"location": "Paris"}"#.into()
            },
            ProviderEvent::ToolCall {
                id: "toolu_02".into(),
                name: "read_file".into(),
                input_json: r#"{"path": "a.txt"}"#.into()
            },
            ProviderEvent::ToolCall {
                id: "toolu_03".into(),
                name: "list_tools".into(),
                input_json: "{}".into()
            },
            ProviderEvent::Usage(RawUsage {
                input: 50,
                output: 60,
                cache_read: 0,
                cache_write: 0
            }),
            ProviderEvent::Done {
                stop: StopReason::ToolUse
            },
        ]
    );
}

#[tokio::test]
async fn parses_thinking_fixture() {
    let server = serve(sse(fixture("thinking.sse"))).await;
    let evs: Vec<_> = run(&server).await.into_iter().map(Result::unwrap).collect();
    assert_eq!(
        evs,
        vec![
            ProviderEvent::ReasoningDelta("Let me think".into()),
            ProviderEvent::ReasoningDelta(" about this.".into()),
            ProviderEvent::ReasoningSignature("EqQBCgIYAhIM".into()),
            ProviderEvent::TextDelta("42".into()),
            ProviderEvent::Usage(RawUsage {
                input: 10,
                output: 30,
                cache_read: 0,
                cache_write: 0
            }),
            ProviderEvent::Done {
                stop: StopReason::EndTurn
            },
        ]
    );
}

#[tokio::test]
async fn parses_usage_with_cache_tokens() {
    let server = serve(sse(fixture("text.sse"))).await;
    let evs: Vec<_> = run(&server).await.into_iter().map(Result::unwrap).collect();
    assert!(evs.contains(&ProviderEvent::Usage(RawUsage {
        input: 25,
        output: 15,
        cache_read: 200,
        cache_write: 100
    })));
}

#[tokio::test]
async fn rate_limit_headers_become_quota() {
    let resp = sse(fixture("text.sse"))
        .insert_header("anthropic-ratelimit-requests-remaining", "49")
        .insert_header("anthropic-ratelimit-tokens-remaining", "39000")
        .insert_header("anthropic-ratelimit-tokens-reset", "2026-01-01T00:00:30Z");
    let server = serve(resp).await;
    let evs: Vec<_> = run(&server).await.into_iter().map(Result::unwrap).collect();
    let reset = SystemTime::UNIX_EPOCH + Duration::from_secs(1_767_225_630);
    assert_eq!(
        evs[0],
        ProviderEvent::Quota(Quota {
            remaining_requests: Some(49),
            remaining_tokens: Some(39_000),
            reset_at: Some(reset)
        })
    );
    assert_eq!(
        evs.iter()
            .filter(|e| matches!(e, ProviderEvent::Quota(_)))
            .count(),
        1
    );
}

#[tokio::test]
async fn no_quota_event_without_headers() {
    let server = serve(sse(fixture("text.sse"))).await;
    let evs: Vec<_> = run(&server).await.into_iter().map(Result::unwrap).collect();
    assert!(!evs.iter().any(|e| matches!(e, ProviderEvent::Quota(_))));
}

#[tokio::test]
async fn status_529_is_transient_and_413_prompt_too_long_is_context_overflow() {
    let server = serve(
        ResponseTemplate::new(529)
            .insert_header("retry-after", "7")
            .set_body_string(fixture("overloaded_529.sse")),
    )
    .await;
    let r = run(&server).await;
    assert_eq!(r.len(), 1);
    assert!(matches!(
        r[0].as_ref().unwrap_err(),
        ProviderError::Transient { status: Some(529), retry_after: Some(d), .. } if *d == Duration::from_secs(7)
    ));

    let server =
        serve(ResponseTemplate::new(413).set_body_string(fixture("context_overflow.json"))).await;
    assert!(matches!(
        run(&server).await[0],
        Err(ProviderError::ContextOverflow(_))
    ));

    let server =
        serve(ResponseTemplate::new(400).set_body_string(fixture("context_overflow.json"))).await;
    assert!(matches!(
        run(&server).await[0],
        Err(ProviderError::ContextOverflow(_))
    ));
}

#[tokio::test]
async fn other_statuses_map_to_expected_errors() {
    let bad = r#"{"type":"error","error":{"type":"invalid_request_error","message":"max_tokens: required"}}"#;
    let server = serve(ResponseTemplate::new(400).set_body_string(bad)).await;
    assert!(matches!(
        run(&server).await[0],
        Err(ProviderError::InvalidRequest(_))
    ));
    for code in [401, 403] {
        let server = serve(ResponseTemplate::new(code).set_body_string("{}")).await;
        assert!(matches!(run(&server).await[0], Err(ProviderError::Auth(_))));
    }
    for code in [429, 500, 502, 503, 504] {
        let server = serve(ResponseTemplate::new(code).set_body_string("{}")).await;
        assert!(
            matches!(run(&server).await[0], Err(ProviderError::Transient { status: Some(s), .. }) if s == code)
        );
    }
}

#[tokio::test]
async fn mid_stream_error_events_are_classified() {
    let server = serve(sse(fixture("overloaded_529.sse"))).await;
    let r = run(&server).await;
    assert!(matches!(
        r[0],
        Err(ProviderError::Transient { status: None, .. })
    ));

    let other = "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"invalid_request_error\",\"message\":\"bad\"}}\n\n";
    let server = serve(sse(other.into())).await;
    assert!(matches!(
        run(&server).await[0],
        Err(ProviderError::Other(_))
    ));
}

#[tokio::test]
async fn connection_failure_is_transient() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let p = AnthropicProvider::new(
        "k".into(),
        url.parse().unwrap(),
        Arc::new(Catalog::builtin()),
    );
    let r: Vec<_> = p
        .stream(base_req(), CancellationToken::new())
        .collect()
        .await;
    assert!(matches!(
        r[0],
        Err(ProviderError::Transient { status: None, .. })
    ));
}

#[tokio::test]
async fn system_images_are_rejected_without_sending_a_request() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(sse(fixture("text.sse")))
        .expect(0)
        .mount(&server)
        .await;
    let mut req = base_req();
    req.system.push(Block::Image {
        media_type: "image/png".into(),
        data: vec![1],
    });
    let p = AnthropicProvider::new(
        "k".into(),
        server.uri().parse().unwrap(),
        Arc::new(Catalog::builtin()),
    );
    let r: Vec<_> = p.stream(req, CancellationToken::new()).collect().await;
    assert_eq!(
        r,
        vec![Err(ProviderError::InvalidRequest(
            "images are not supported in the system prompt".into()
        ))]
    );
}

#[tokio::test]
async fn unrelated_400_mentioning_context_is_invalid_request() {
    let body = r#"{"type":"error","error":{"type":"invalid_request_error","message":"context_management: unknown field"}}"#;
    let server = serve(ResponseTemplate::new(400).set_body_string(body)).await;
    assert!(matches!(
        run(&server).await[0],
        Err(ProviderError::InvalidRequest(_))
    ));
}

#[test]
fn user_image_content_becomes_base64_image_block() {
    let mut req = base_req();
    req.messages = vec![Message {
        role: Role::User,
        content: vec![
            Content::Text("look".into()),
            Content::Image {
                media_type: "image/png".into(),
                data: vec![1, 2, 3],
            },
        ],
    }];
    let w = to_wire(&req);
    assert_eq!(
        w["messages"][0]["content"],
        json!([
            {"type":"text","text":"look"},
            {"type":"image","source":{"type":"base64","media_type":"image/png","data":"AQID"}},
        ])
    );
}

#[test]
fn image_content_serde_round_trips_as_base64() {
    let c = Content::Image {
        media_type: "image/png".into(),
        data: vec![1, 2, 3],
    };
    let v = serde_json::to_value(&c).unwrap();
    assert_eq!(v, json!({"image":{"media_type":"image/png","data":"AQID"}}));
    assert_eq!(serde_json::from_value::<Content>(v).unwrap(), c);
}

#[test]
fn assistant_image_content_is_skipped() {
    let mut req = base_req();
    req.messages = vec![Message {
        role: Role::Assistant,
        content: vec![
            Content::Text("ok".into()),
            Content::Image {
                media_type: "image/png".into(),
                data: vec![1],
            },
        ],
    }];
    let w = to_wire(&req);
    assert_eq!(
        w["messages"][0]["content"],
        json!([{"type":"text","text":"ok"}])
    );
}

// ---- checkpoint fix: redacted thinking round-trip ----
#[tokio::test]
async fn redacted_thinking_block_is_surfaced() {
    let events = [
        json!({"type":"message_start","message":{"usage":{"input_tokens":5,"output_tokens":0}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"redacted_thinking","data":"EmwKAhgBEgy3va3pzix"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"ok"}}),
        json!({"type":"content_block_stop","index":1}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}),
        json!({"type":"message_stop"}),
    ];
    let body: String = events
        .iter()
        .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
        .collect();
    let server = serve(sse(body)).await;
    let evs: Vec<_> = run(&server).await.into_iter().map(Result::unwrap).collect();
    assert_eq!(
        evs[0],
        ProviderEvent::RedactedReasoning("EmwKAhgBEgy3va3pzix".into())
    );
    assert_eq!(evs[1], ProviderEvent::TextDelta("ok".into()));
}

#[test]
fn redacted_reasoning_is_echoed_verbatim() {
    let mut req = base_req();
    req.messages = vec![
        Message {
            role: Role::User,
            content: vec![Content::Text("q".into())],
        },
        Message {
            role: Role::Assistant,
            content: vec![
                Content::RedactedReasoning {
                    data: "opaque==".into(),
                },
                Content::Text("a".into()),
            ],
        },
    ];
    let w = to_wire(&req);
    assert_eq!(
        w["messages"][1]["content"][0],
        json!({"type": "redacted_thinking", "data": "opaque=="})
    );
    let c = Content::RedactedReasoning { data: "x".into() };
    let s = serde_json::to_string(&c).unwrap();
    assert_eq!(s, r#"{"redacted_reasoning":{"data":"x"}}"#);
    assert_eq!(serde_json::from_str::<Content>(&s).unwrap(), c);
    let e = ProviderEvent::RedactedReasoning("x".into());
    let s = serde_json::to_string(&e).unwrap();
    assert_eq!(serde_json::from_str::<ProviderEvent>(&s).unwrap(), e);
}
