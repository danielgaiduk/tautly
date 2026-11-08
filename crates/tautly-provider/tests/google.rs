use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use serde_json::json;
use tautly_provider::google::{GoogleProvider, to_wire};
use tautly_provider::*;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!(
        "{}/tests/fixtures/google/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

fn msg(role: Role, content: Vec<Content>) -> Message {
    Message { role, content }
}

fn text(t: &str) -> Content {
    Content::Text(t.into())
}

fn req_of(messages: Vec<Message>) -> CanonicalRequest {
    CanonicalRequest {
        model: "google/gemini-2.5-pro".into(),
        system: vec![
            Block::Text("You are Tautly.".into()),
            Block::Text("Be brief.".into()),
        ],
        tools: vec![ToolSpec {
            name: "read_file".into(),
            description: "read a file".into(),
            schema: json!({"type":"object","properties":{"path":{"type":"string"}}}),
        }],
        messages,
        max_output: 8_000,
        reasoning: None,
        cache_breakpoints: vec![],
        volatile_tail: false,
    }
}

fn base_req() -> CanonicalRequest {
    req_of(vec![msg(Role::User, vec![text("hi")])])
}

fn req_reasoning(r: Reasoning) -> CanonicalRequest {
    CanonicalRequest {
        reasoning: Some(r),
        ..base_req()
    }
}

fn provider(server: &MockServer) -> GoogleProvider {
    GoogleProvider::new(
        "gk".into(),
        server.uri().parse().unwrap(),
        Arc::new(Catalog::builtin()),
    )
}

async fn run_req(
    server: &MockServer,
    req: CanonicalRequest,
) -> Vec<Result<ProviderEvent, ProviderError>> {
    provider(server)
        .stream(req, CancellationToken::new())
        .collect()
        .await
}

async fn run(server: &MockServer) -> Vec<Result<ProviderEvent, ProviderError>> {
    run_req(server, base_req()).await
}

fn sse(body: String) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(body)
}

async fn serve(resp: ResponseTemplate) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1beta/models/gemini-2.5-pro:streamGenerateContent"))
        .respond_with(resp)
        .mount(&server)
        .await;
    server
}

fn ok(r: Vec<Result<ProviderEvent, ProviderError>>) -> Vec<ProviderEvent> {
    r.into_iter().map(Result::unwrap).collect()
}

#[test]
fn thinking_budget_maps() {
    let w = to_wire(&req_reasoning(Reasoning::Budget(2_000)));
    assert_eq!(
        w["generationConfig"]["thinkingConfig"]["thinkingBudget"],
        2_000
    );
    assert_eq!(
        w["generationConfig"]["thinkingConfig"]["includeThoughts"],
        true
    );
    for (e, n) in [
        (Effort::Low, 4_000), // 4_096, capped at half of maxOutputTokens 8_000
        (Effort::Medium, 4_000),
        (Effort::High, 4_000),
    ] {
        let w = to_wire(&req_reasoning(Reasoning::Effort(e)));
        assert_eq!(w["generationConfig"]["thinkingConfig"]["thinkingBudget"], n);
    }
    let w = to_wire(&base_req());
    assert!(w["generationConfig"].get("thinkingConfig").is_none());
    assert_eq!(w["generationConfig"]["maxOutputTokens"], 8_000);
}

#[test]
fn wire_basics_and_content_mapping() {
    let req = req_of(vec![
        msg(
            Role::User,
            vec![
                text("hello"),
                Content::Image {
                    media_type: "image/png".into(),
                    data: vec![1, 2, 3],
                },
            ],
        ),
        msg(
            Role::Assistant,
            vec![
                Content::Reasoning {
                    text: "hmm".into(),
                    signature: Some("s".into()),
                },
                text("calling"),
                Content::ToolUse {
                    id: "call_0".into(),
                    name: "read_file".into(),
                    input: json!({"path":"a"}),
                },
            ],
        ),
        msg(
            Role::User,
            vec![
                text("here"),
                Content::ToolResult {
                    id: "call_0".into(),
                    output: "file body".into(),
                    is_error: false,
                },
            ],
        ),
    ]);
    let w = to_wire(&req);
    assert_eq!(
        w["systemInstruction"],
        json!({"parts":[{"text":"You are Tautly."},{"text":"Be brief."}]})
    );
    assert_eq!(
        w["tools"],
        json!([{"functionDeclarations":[{"name":"read_file","description":"read a file","parameters":{"type":"object","properties":{"path":{"type":"string"}}}}]}])
    );
    assert_eq!(
        w["contents"],
        json!([
            {"role":"user","parts":[{"text":"hello"},{"inlineData":{"mimeType":"image/png","data":"AQID"}}]},
            {"role":"model","parts":[{"text":"calling","thoughtSignature":"s"},{"functionCall":{"name":"read_file","args":{"path":"a"}}}]},
            {"role":"user","parts":[
                {"functionResponse":{"name":"read_file","response":{"content":"file body"}}},
                {"text":"here"},
            ]},
        ])
    );
}

#[test]
fn no_system_or_tools_omits_those_fields() {
    let mut req = base_req();
    req.system.clear();
    req.tools.clear();
    let w = to_wire(&req);
    assert!(w.get("systemInstruction").is_none());
    assert!(w.get("tools").is_none());
}

#[tokio::test]
async fn sends_expected_request_and_parses_text() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1beta/models/gemini-2.5-pro:streamGenerateContent"))
        .and(query_param("alt", "sse"))
        .and(header("x-goog-api-key", "gk"))
        .respond_with(sse(fixture("text.sse")))
        .expect(1)
        .mount(&server)
        .await;
    let evs = ok(run(&server).await);
    assert_eq!(
        evs,
        vec![
            ProviderEvent::TextDelta("Hel".into()),
            ProviderEvent::TextDelta("lo".into()),
            ProviderEvent::Usage(RawUsage {
                input: 40,
                output: 20,
                cache_read: 80,
                cache_write: 0
            }),
            ProviderEvent::Done {
                stop: StopReason::EndTurn
            },
        ]
    );
}

#[tokio::test]
async fn usage_maps_cached_content_token_count() {
    let server = serve(sse(fixture("text.sse"))).await;
    let evs = ok(run(&server).await);
    // promptTokenCount (120) includes 80 cached; thoughts (5) count as output with 15 candidates.
    assert!(evs.contains(&ProviderEvent::Usage(RawUsage {
        input: 40,
        output: 20,
        cache_read: 80,
        cache_write: 0
    })));
}

#[tokio::test]
async fn function_call_parts_parse() {
    let server = serve(sse(fixture("function_call.sse"))).await;
    let mut evs = ok(run(&server).await);
    // Ids are generated; check their shape and that they differ, then compare the rest.
    let ids: Vec<String> = evs
        .iter_mut()
        .filter_map(|e| match e {
            ProviderEvent::ToolCall { id, .. } => Some(std::mem::replace(id, "ID".into())),
            _ => None,
        })
        .collect();
    assert_eq!(ids.len(), 2);
    assert_ne!(ids[0], ids[1]);
    for id in &ids {
        assert_call_id_shape(id);
    }
    assert_eq!(
        evs,
        vec![
            ProviderEvent::TextDelta("Let me look.".into()),
            ProviderEvent::ToolCall {
                id: "ID".into(),
                name: "read_file".into(),
                input_json: "{\"path\":\"a.rs\"}".into()
            },
            ProviderEvent::ToolCall {
                id: "ID".into(),
                name: "read_file".into(),
                input_json: "{\"path\":\"b.rs\"}".into()
            },
            ProviderEvent::Usage(RawUsage {
                input: 50,
                output: 12,
                cache_read: 0,
                cache_write: 0
            }),
            ProviderEvent::Done {
                stop: StopReason::ToolUse
            },
        ]
    );
}

/// `call_` followed by 12 lowercase hex digits.
fn assert_call_id_shape(id: &str) {
    let hex = id.strip_prefix("call_").unwrap_or_else(|| panic!("{id}"));
    assert_eq!(hex.len(), 12, "{id}");
    assert!(
        hex.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "{id}"
    );
}

fn first_call_id(evs: Vec<ProviderEvent>) -> String {
    evs.into_iter()
        .find_map(|e| match e {
            ProviderEvent::ToolCall { id, .. } => Some(id),
            _ => None,
        })
        .expect("a tool call")
}

#[tokio::test]
async fn call_ids_do_not_collide_across_providers_or_resumes() {
    // A resumed session builds a new provider; its first call must not reuse the id of an
    // earlier session's first call, which may still be in the history.
    let server = serve(sse(fixture("function_call.sse"))).await;
    let mut ids = std::collections::HashSet::new();
    for _ in 0..20 {
        let id = first_call_id(ok(run(&server).await));
        assert_call_id_shape(&id);
        assert!(ids.insert(id.clone()), "duplicate {id}");
    }
}

#[tokio::test]
async fn thought_parts_become_reasoning_deltas() {
    let server = serve(sse(fixture("thinking.sse"))).await;
    let evs = ok(run(&server).await);
    assert_eq!(
        &evs[..2],
        &[
            ProviderEvent::ReasoningDelta("Considering the question.".into()),
            ProviderEvent::TextDelta("42".into()),
        ]
    );
    assert!(evs.contains(&ProviderEvent::Usage(RawUsage {
        input: 10,
        output: 20,
        cache_read: 0,
        cache_write: 0
    })));
}

#[tokio::test]
async fn finish_reasons_map_to_stop_reasons() {
    let server = serve(sse(fixture("max_tokens.sse"))).await;
    let evs = ok(run(&server).await);
    assert_eq!(
        evs.last(),
        Some(&ProviderEvent::Done {
            stop: StopReason::MaxTokens
        })
    );
    let server = serve(sse(fixture("safety.sse"))).await;
    let evs = ok(run(&server).await);
    assert_eq!(
        evs.last(),
        Some(&ProviderEvent::Done {
            stop: StopReason::Refusal
        })
    );
}

#[tokio::test]
async fn no_quota_event() {
    let server = serve(sse(fixture("text.sse"))).await;
    let evs = ok(run(&server).await);
    assert!(!evs.iter().any(|e| matches!(e, ProviderEvent::Quota(_))));
}

#[tokio::test]
async fn http_errors_map_to_expected_variants() {
    let server = serve(
        ResponseTemplate::new(429)
            .insert_header("retry-after", "7")
            .set_body_string(
                r#"{"error":{"code":429,"message":"quota","status":"RESOURCE_EXHAUSTED"}}"#,
            ),
    )
    .await;
    assert_eq!(
        run(&server).await[0],
        Err(ProviderError::Transient {
            status: Some(429),
            retry_after: Some(Duration::from_secs(7)),
            msg: "quota".into()
        })
    );

    let server = serve(ResponseTemplate::new(503).set_body_string("down")).await;
    assert!(matches!(
        run(&server).await[0],
        Err(ProviderError::Transient {
            status: Some(503),
            ..
        })
    ));

    for status in [401, 403] {
        let server = serve(ResponseTemplate::new(status).set_body_string(
            r#"{"error":{"code":403,"message":"API key not valid","status":"PERMISSION_DENIED"}}"#,
        ))
        .await;
        assert!(matches!(run(&server).await[0], Err(ProviderError::Auth(_))));
    }

    let server =
        serve(ResponseTemplate::new(400).set_body_string(fixture("context_overflow.json"))).await;
    assert!(matches!(
        run(&server).await[0],
        Err(ProviderError::ContextOverflow(_))
    ));

    let server = serve(ResponseTemplate::new(400).set_body_string(
        r#"{"error":{"code":400,"message":"Unknown name \"foo\"","status":"INVALID_ARGUMENT"}}"#,
    ))
    .await;
    assert!(matches!(
        run(&server).await[0],
        Err(ProviderError::InvalidRequest(_))
    ));
}

#[tokio::test]
async fn mid_stream_error_object_is_classified() {
    let server = serve(sse(fixture("stream_error.sse"))).await;
    let r = run(&server).await;
    assert_eq!(r[0], Ok(ProviderEvent::TextDelta("par".into())));
    assert!(matches!(
        r[1],
        Err(ProviderError::Transient { status: None, .. })
    ));
}

#[tokio::test]
async fn stream_without_finish_reason_is_transient() {
    // Drop the final chunk, which carries the finishReason.
    let truncated = fixture("text.sse")
        .split("\n\ndata:")
        .next()
        .unwrap()
        .to_string()
        + "\n\n";
    let server = serve(sse(truncated)).await;
    let r = run(&server).await;
    assert!(matches!(
        r.last().unwrap(),
        Err(ProviderError::Transient { status: None, .. })
    ));
    assert!(
        !r.iter()
            .any(|e| matches!(e, Ok(ProviderEvent::Done { .. })))
    );
}

#[tokio::test]
async fn connection_failure_is_transient() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let p = GoogleProvider::new(
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
async fn cancellation_ends_the_stream_silently() {
    let server = serve(sse(fixture("text.sse")).set_delay(Duration::from_secs(30))).await;
    let cancel = CancellationToken::new();
    let stream = provider(&server).stream(base_req(), cancel.clone());
    let handle = tokio::spawn(async move { stream.collect::<Vec<_>>().await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    cancel.cancel();
    let r = tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .unwrap()
        .unwrap();
    assert!(r.is_empty());
}

#[tokio::test]
async fn system_images_are_rejected_without_sending_a_request() {
    let server = MockServer::start().await;
    let mut req = base_req();
    req.system.push(Block::Image {
        media_type: "image/png".into(),
        data: vec![1],
    });
    let r = run_req(&server, req).await;
    assert!(matches!(r[0], Err(ProviderError::InvalidRequest(_))));
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[test]
fn tool_schemas_are_sanitized_recursively() {
    let mut req = base_req();
    req.tools = vec![ToolSpec {
        name: "t".into(),
        description: "d".into(),
        schema: json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "type": "object",
            "additionalProperties": false,
            "$defs": {"x": {"type": "string"}},
            "properties": {
                "opts": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {"n": {"type": "integer", "default": 3, "examples": [1]}},
                },
                "list": {"type": "array", "items": {"type": "object", "additionalProperties": false, "$comment": "c"}},
                // A property that happens to share a stripped keyword's name is kept.
                "examples": {"type": "string"},
            },
        }),
    }];
    let w = to_wire(&req);
    assert_eq!(
        w["tools"][0]["functionDeclarations"][0]["parameters"],
        json!({
            "type": "object",
            "properties": {
                "opts": {
                    "type": "object",
                    "properties": {"n": {"type": "integer", "default": 3}},
                },
                "list": {"type": "array", "items": {"type": "object"}},
                "examples": {"type": "string"},
            },
        })
    );
}

#[test]
fn tool_result_error_uses_error_key() {
    let req = req_of(vec![
        msg(
            Role::Assistant,
            vec![Content::ToolUse {
                id: "c0".into(),
                name: "read_file".into(),
                input: json!({}),
            }],
        ),
        msg(
            Role::User,
            vec![Content::ToolResult {
                id: "c0".into(),
                output: "boom".into(),
                is_error: true,
            }],
        ),
    ]);
    let w = to_wire(&req);
    assert_eq!(
        w["contents"][1]["parts"][0],
        json!({"functionResponse":{"name":"read_file","response":{"error":"boom"}}})
    );
}

#[test]
fn reasoning_signature_attaches_to_following_part() {
    let req = req_of(vec![msg(
        Role::Assistant,
        vec![
            Content::Reasoning {
                text: "t".into(),
                signature: Some("sigA".into()),
            },
            Content::ToolUse {
                id: "c0".into(),
                name: "read_file".into(),
                input: json!({"path":"a"}),
            },
            Content::Reasoning {
                text: "t".into(),
                signature: Some("sigB".into()),
            },
            text("done"),
            Content::Reasoning {
                text: "unsigned".into(),
                signature: None,
            },
            text("tail"),
        ],
    )]);
    let w = to_wire(&req);
    assert_eq!(
        w["contents"][0]["parts"],
        json!([
            {"functionCall":{"name":"read_file","args":{"path":"a"}},"thoughtSignature":"sigA"},
            {"text":"done","thoughtSignature":"sigB"},
            {"text":"tail"},
        ])
    );
}

#[tokio::test]
async fn thought_signature_emits_event_before_its_part() {
    let server = serve(sse(fixture("thought_signature.sse"))).await;
    let mut evs = ok(run(&server).await);
    if let ProviderEvent::ToolCall { id, .. } = &mut evs[1] {
        assert_call_id_shape(id);
        *id = "ID".into();
    }
    assert_eq!(
        &evs[..4],
        &[
            ProviderEvent::ReasoningSignature("sigA".into()),
            ProviderEvent::ToolCall {
                id: "ID".into(),
                name: "read_file".into(),
                input_json: "{\"path\":\"a.rs\"}".into()
            },
            ProviderEvent::ReasoningSignature("sigB".into()),
            ProviderEvent::TextDelta("done".into()),
        ]
    );
}

fn tool_result(id: &str, out: &str) -> Content {
    Content::ToolResult {
        id: id.into(),
        output: out.into(),
        is_error: false,
    }
}

#[test]
fn consecutive_same_role_messages_merge_into_one_turn() {
    // History ending in tool results followed by the working-state tail.
    let req = req_of(vec![
        msg(Role::User, vec![text("q")]),
        msg(
            Role::Assistant,
            vec![Content::ToolUse {
                id: "c1".into(),
                name: "read_file".into(),
                input: json!({"path": "a"}),
            }],
        ),
        msg(Role::User, vec![tool_result("c1", "body")]),
        msg(Role::User, vec![text("<working_state>s</working_state>")]),
    ]);
    let w = to_wire(&req);
    let contents = w["contents"].as_array().unwrap();
    assert_eq!(contents.len(), 3, "{contents:?}");
    assert_eq!(contents[2]["role"], "user");
    let parts = contents[2]["parts"].as_array().unwrap();
    assert_eq!(parts.len(), 2);
    assert!(parts[0].get("functionResponse").is_some());
    assert_eq!(parts[1]["text"], "<working_state>s</working_state>");

    // First request of a turn: prompt followed by the state tail.
    let req = req_of(vec![
        msg(Role::User, vec![text("prompt")]),
        msg(Role::User, vec![text("state")]),
    ]);
    let w = to_wire(&req);
    let contents = w["contents"].as_array().unwrap();
    assert_eq!(contents.len(), 1);
    assert_eq!(
        contents[0]["parts"],
        json!([{"text": "prompt"}, {"text": "state"}])
    );
}

#[test]
fn merge_keeps_function_responses_first_when_text_precedes_them() {
    let req = req_of(vec![
        msg(
            Role::Assistant,
            vec![Content::ToolUse {
                id: "c1".into(),
                name: "read_file".into(),
                input: json!({}),
            }],
        ),
        msg(Role::User, vec![text("early")]),
        msg(Role::User, vec![tool_result("c1", "body")]),
    ]);
    let w = to_wire(&req);
    let parts = &w["contents"][1]["parts"];
    assert!(parts[0].get("functionResponse").is_some(), "{parts}");
    assert_eq!(parts[1]["text"], "early");
}
