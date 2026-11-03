use std::sync::Arc;
use std::time::{Duration, SystemTime};

use futures::StreamExt;
use serde_json::{Value, json};
use tautly_provider::compat::{CompatProvider, to_wire};
use tautly_provider::*;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!(
        "{}/tests/fixtures/compat/{name}",
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
        model: "compat:ollama/qwen3".into(),
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
        reasoning: Some(Reasoning::Effort(Effort::High)),
        cache_breakpoints: vec![],
        volatile_tail: false,
    }
}

fn base_req() -> CanonicalRequest {
    req_of(vec![msg(Role::User, vec![text("hi")])])
}

fn provider_with(server: &MockServer, key: Option<&str>) -> CompatProvider {
    CompatProvider::new(
        key.map(String::from),
        server.uri().parse().unwrap(),
        Arc::new(Catalog::builtin()),
    )
}

async fn run_req(
    server: &MockServer,
    req: CanonicalRequest,
) -> Vec<Result<ProviderEvent, ProviderError>> {
    provider_with(server, Some("k"))
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
        .and(path("/chat/completions"))
        .respond_with(resp)
        .mount(&server)
        .await;
    server
}

/// The fixture up to (not including) the SSE event that contains `marker`.
fn before_event(fixture: &str, marker: &str) -> String {
    let at = fixture.find(marker).expect("marker in fixture");
    fixture[..fixture[..at].rfind("data:").unwrap()].to_string()
}

fn ok(r: Vec<Result<ProviderEvent, ProviderError>>) -> Vec<ProviderEvent> {
    r.into_iter().map(Result::unwrap).collect()
}

#[test]
fn model_id_yields_bare_model_name() {
    let w = to_wire(&base_req());
    assert_eq!(w["model"], "qwen3");
}

#[test]
fn wire_basics_and_message_mapping() {
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
                    signature: None,
                },
                text("calling"),
                Content::ToolUse {
                    id: "call_1".into(),
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
                    id: "call_1".into(),
                    output: "file body".into(),
                    is_error: false,
                },
            ],
        ),
    ]);
    let w = to_wire(&req);
    assert_eq!(w["stream"], true);
    assert_eq!(w["stream_options"], json!({"include_usage": true}));
    assert_eq!(w["max_tokens"], 8_000);
    // No portable reasoning field exists.
    assert!(w.get("reasoning").is_none() && w.get("reasoning_effort").is_none());
    assert_eq!(
        w["tools"][0],
        json!({"type":"function","function":{"name":"read_file","description":"read a file","parameters":{"type":"object","properties":{"path":{"type":"string"}}}}})
    );
    assert_eq!(
        w["messages"],
        json!([
            {"role":"system","content":"You are Tautly.\n\nBe brief."},
            {"role":"user","content":[
                {"type":"text","text":"hello"},
                {"type":"image_url","image_url":{"url":"data:image/png;base64,AQID"}},
            ]},
            {"role":"assistant","content":"calling","tool_calls":[
                {"id":"call_1","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"a\"}"}}
            ]},
            {"role":"tool","tool_call_id":"call_1","content":"file body"},
            {"role":"user","content":"here"},
        ])
    );
}

#[test]
fn assistant_tool_call_without_text_has_null_content() {
    let req = req_of(vec![msg(
        Role::Assistant,
        vec![Content::ToolUse {
            id: "c".into(),
            name: "t".into(),
            input: json!({}),
        }],
    )]);
    let w = to_wire(&req);
    assert_eq!(w["messages"][1]["content"], Value::Null);
}

#[tokio::test]
async fn sends_expected_request_and_parses_text() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(header("authorization", "Bearer k"))
        .and(body_partial_json(
            json!({"stream": true, "model": "qwen3", "stream_options": {"include_usage": true}}),
        ))
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
                output: 15,
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
async fn chat_completions_tool_calls_accumulate_by_index() {
    let server = serve(sse(fixture("tool_calls.sse"))).await;
    let evs = ok(run(&server).await);
    assert_eq!(
        evs,
        vec![
            ProviderEvent::ToolCall {
                id: "call_a".into(),
                name: "read_file".into(),
                input_json: "{\"path\":\"a.rs\"}".into()
            },
            ProviderEvent::ToolCall {
                id: "call_b".into(),
                name: "grep".into(),
                input_json: "{\"pattern\":\"fn\"}".into()
            },
            ProviderEvent::Usage(RawUsage {
                input: 30,
                output: 20,
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
async fn missing_usage_is_tolerated() {
    let server = serve(sse(fixture("no_usage.sse"))).await;
    let evs = ok(run(&server).await);
    assert_eq!(
        evs,
        vec![
            ProviderEvent::TextDelta("Hi".into()),
            ProviderEvent::Done {
                stop: StopReason::EndTurn
            },
        ]
    );
}

#[tokio::test]
async fn no_api_key_omits_authorization_header() {
    let server = serve(sse(fixture("no_usage.sse"))).await;
    let p = provider_with(&server, None);
    let evs: Vec<_> = p
        .stream(base_req(), CancellationToken::new())
        .collect()
        .await;
    ok(evs);
    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 1);
    assert!(reqs[0].headers.get("authorization").is_none());
}

#[tokio::test]
async fn ollama_fixture_parses() {
    let server = serve(sse(fixture("ollama.sse"))).await;
    let evs = ok(run(&server).await);
    assert_eq!(
        evs,
        vec![
            ProviderEvent::ReasoningDelta("Let me think. ".into()),
            ProviderEvent::ReasoningDelta("Easy.".into()),
            ProviderEvent::TextDelta("It is 4.".into()),
            ProviderEvent::Done {
                stop: StopReason::EndTurn
            },
        ]
    );
}

#[tokio::test]
async fn reasoning_content_and_length_finish_parse() {
    let server = serve(sse(fixture("reasoning_content.sse"))).await;
    let evs = ok(run(&server).await);
    assert_eq!(
        evs,
        vec![
            ProviderEvent::ReasoningDelta("Hmm.".into()),
            ProviderEvent::TextDelta("Ok".into()),
            ProviderEvent::Done {
                stop: StopReason::MaxTokens
            },
        ]
    );
}

#[tokio::test]
async fn x_ratelimit_headers_become_quota() {
    let resp = sse(fixture("no_usage.sse"))
        .insert_header("x-ratelimit-remaining-requests", "49")
        .insert_header("x-ratelimit-remaining-tokens", "39000")
        .insert_header("x-ratelimit-reset-requests", "1s")
        .insert_header("x-ratelimit-reset-tokens", "6m0s");
    let server = serve(resp).await;
    let before = SystemTime::now();
    let evs = ok(run(&server).await);
    let ProviderEvent::Quota(q) = &evs[0] else {
        panic!("first event should be Quota, got {:?}", evs[0]);
    };
    assert_eq!(q.remaining_requests, Some(49));
    assert_eq!(q.remaining_tokens, Some(39_000));
    assert!(q.reset_at.unwrap() >= before + Duration::from_secs(360));

    let server = serve(sse(fixture("no_usage.sse"))).await;
    let evs = ok(run(&server).await);
    assert!(!evs.iter().any(|e| matches!(e, ProviderEvent::Quota(_))));
}

#[tokio::test]
async fn http_errors_map_to_expected_variants() {
    let server = serve(
        ResponseTemplate::new(429)
            .insert_header("retry-after", "7")
            .set_body_string(r#"{"error":{"message":"slow down","code":"rate_limit_exceeded"}}"#),
    )
    .await;
    assert_eq!(
        run(&server).await[0],
        Err(ProviderError::Transient {
            status: Some(429),
            retry_after: Some(Duration::from_secs(7)),
            msg: "slow down".into()
        })
    );

    let server = serve(ResponseTemplate::new(502).set_body_string("bad gateway")).await;
    assert!(matches!(
        run(&server).await[0],
        Err(ProviderError::Transient {
            status: Some(502),
            ..
        })
    ));

    let server =
        serve(ResponseTemplate::new(401).set_body_string(
            r#"{"error":{"message":"Incorrect API key","code":"invalid_api_key"}}"#,
        ))
        .await;
    assert!(matches!(run(&server).await[0], Err(ProviderError::Auth(_))));

    let server =
        serve(ResponseTemplate::new(400).set_body_string(fixture("context_overflow.json"))).await;
    assert!(matches!(
        run(&server).await[0],
        Err(ProviderError::ContextOverflow(_))
    ));

    for msg in [
        "This model's maximum context length is 8192 tokens.",
        "prompt exceeds the context window of the model",
    ] {
        let body = json!({"error": {"message": msg}}).to_string();
        let server = serve(ResponseTemplate::new(400).set_body_string(body)).await;
        assert!(
            matches!(
                run(&server).await[0],
                Err(ProviderError::ContextOverflow(_))
            ),
            "{msg}"
        );
    }

    // Ollama reports errors as a bare string.
    let server =
        serve(ResponseTemplate::new(404).set_body_string(r#"{"error":"model 'x' not found"}"#))
            .await;
    assert_eq!(
        run(&server).await[0],
        Err(ProviderError::InvalidRequest("model 'x' not found".into()))
    );
}

#[tokio::test]
async fn mid_stream_error_is_classified() {
    let server = serve(sse(fixture("error.sse"))).await;
    let r = run(&server).await;
    assert_eq!(r[0], Ok(ProviderEvent::TextDelta("Hi".into())));
    assert!(matches!(
        r[1],
        Err(ProviderError::Transient { status: None, .. })
    ));
}

#[tokio::test]
async fn stream_without_finish_or_done_is_transient() {
    let truncated = before_event(&fixture("text.sse"), "\"finish_reason\":\"stop\"");
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
async fn tool_calls_are_not_emitted_before_the_choice_finishes() {
    let partial = before_event(
        &fixture("tool_calls.sse"),
        "\"finish_reason\":\"tool_calls\"",
    );
    let server = serve(sse(partial)).await;
    let r = run(&server).await;
    assert!(
        !r.iter()
            .any(|e| matches!(e, Ok(ProviderEvent::ToolCall { .. })))
    );
    assert!(matches!(
        r.last().unwrap(),
        Err(ProviderError::Transient { .. })
    ));
}

#[tokio::test]
async fn connection_failure_is_transient() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let p = CompatProvider::new(None, url.parse().unwrap(), Arc::new(Catalog::builtin()));
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
    let stream = provider_with(&server, None).stream(base_req(), cancel.clone());
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

#[tokio::test]
async fn tool_calls_without_index_split_on_new_id() {
    let server = serve(sse(fixture("tool_calls_no_index.sse"))).await;
    let evs = ok(run(&server).await);
    assert_eq!(
        evs,
        vec![
            ProviderEvent::ToolCall {
                id: "call_x".into(),
                name: "read_file".into(),
                input_json: "{\"path\":\"a.rs\"}".into()
            },
            ProviderEvent::ToolCall {
                id: "call_y".into(),
                name: "grep".into(),
                input_json: "{\"pattern\":\"fn\"}".into()
            },
            ProviderEvent::Done {
                stop: StopReason::ToolUse
            },
        ]
    );
}

// ---- checkpoint fix: I2 index-less tool call after a finished choice ----
#[tokio::test]
async fn new_indexless_tool_call_after_finish_does_not_panic() {
    let body = [
        r#"{"id":"n1","choices":[{"index":0,"delta":{"tool_calls":[{"id":"call_x","type":"function","function":{"name":"read_file","arguments":"{}"}}]},"finish_reason":null}]}"#,
        r#"{"id":"n1","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
        r#"{"id":"n1","choices":[{"index":0,"delta":{"tool_calls":[{"id":"call_y","type":"function","function":{"name":"grep","arguments":"{}"}}]},"finish_reason":null}]}"#,
        "[DONE]",
    ]
    .iter()
    .map(|d| format!("data: {d}\n\n"))
    .collect::<String>();
    let server = serve(sse(body)).await;
    // Run on its own task so a panic shows up as a JoinError, not a test abort.
    let handle = tokio::spawn(async move { run(&server).await });
    let evs = ok(handle.await.expect("compat stream task panicked"));
    assert_eq!(
        evs,
        vec![
            ProviderEvent::ToolCall {
                id: "call_x".into(),
                name: "read_file".into(),
                input_json: "{}".into()
            },
            ProviderEvent::ToolCall {
                id: "call_y".into(),
                name: "grep".into(),
                input_json: "{}".into()
            },
            ProviderEvent::Done {
                stop: StopReason::ToolUse
            },
        ]
    );
}

#[test]
fn consecutive_user_messages_merge_into_one() {
    let req = req_of(vec![
        msg(Role::User, vec![text("prompt")]),
        msg(Role::User, vec![text("state")]),
    ]);
    let w = to_wire(&req);
    let users: Vec<&Value> = w["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "user")
        .collect();
    assert_eq!(users.len(), 1);
    assert_eq!(users[0]["content"], "prompt\n\nstate");
}

#[test]
fn tool_result_then_state_tail_is_tool_message_then_one_user_message() {
    let req = req_of(vec![
        msg(Role::User, vec![text("q")]),
        msg(
            Role::Assistant,
            vec![Content::ToolUse {
                id: "c1".into(),
                name: "read_file".into(),
                input: json!({}),
            }],
        ),
        msg(
            Role::User,
            vec![Content::ToolResult {
                id: "c1".into(),
                output: "body".into(),
                is_error: false,
            }],
        ),
        msg(Role::User, vec![text("state")]),
    ]);
    let w = to_wire(&req);
    let roles: Vec<&str> = w["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, ["system", "user", "assistant", "tool", "user"]);
    assert_eq!(w["messages"][4]["content"], "state");
}
