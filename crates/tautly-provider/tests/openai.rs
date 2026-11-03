use std::sync::Arc;
use std::time::{Duration, SystemTime};

use futures::StreamExt;
use serde_json::{Value, json};
use tautly_provider::openai::{OpenAiProvider, to_wire};
use tautly_provider::*;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!(
        "{}/tests/fixtures/openai/{name}",
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
        model: "openai/gpt-5".into(),
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

fn provider(server: &MockServer) -> OpenAiProvider {
    OpenAiProvider::new(
        "k".into(),
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
        .and(path("/v1/responses"))
        .respond_with(resp)
        .mount(&server)
        .await;
    server
}

fn ok(r: Vec<Result<ProviderEvent, ProviderError>>) -> Vec<ProviderEvent> {
    r.into_iter().map(Result::unwrap).collect()
}

#[test]
fn reasoning_effort_maps() {
    let w = to_wire(&req_reasoning(Reasoning::Effort(Effort::High)));
    assert_eq!(w["reasoning"]["effort"], "high");
    let w = to_wire(&req_reasoning(Reasoning::Effort(Effort::Low)));
    assert_eq!(w["reasoning"]["effort"], "low");
    assert!(to_wire(&base_req()).get("reasoning").is_none());
}

#[test]
fn reasoning_budget_maps_to_effort_buckets() {
    for (budget, want) in [
        (1, "low"),
        (2_048, "low"),
        (2_049, "medium"),
        (8_192, "medium"),
        (8_193, "high"),
    ] {
        let w = to_wire(&req_reasoning(Reasoning::Budget(budget)));
        assert_eq!(w["reasoning"]["effort"], want, "budget {budget}");
    }
}

#[test]
fn wire_basics_and_item_mapping() {
    let req = req_of(vec![
        msg(Role::User, vec![text("hello")]),
        msg(
            Role::Assistant,
            vec![
                Content::Reasoning {
                    text: "hmm".into(),
                    signature: Some("s".into()),
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
    assert_eq!(w["model"], "gpt-5");
    assert_eq!(w["stream"], true);
    assert_eq!(w["store"], false);
    assert_eq!(w["include"], json!(["reasoning.encrypted_content"]));
    assert_eq!(w["max_output_tokens"], 8_000);
    assert_eq!(w["instructions"], "You are Tautly.\n\nBe brief.");
    assert!(w.get("previous_response_id").is_none());
    assert_eq!(
        w["tools"][0],
        json!({"type":"function","name":"read_file","description":"read a file","parameters":{"type":"object","properties":{"path":{"type":"string"}}}})
    );
    assert_eq!(
        w["input"],
        json!([
            {"role":"user","content":[{"type":"input_text","text":"hello"}]},
            {"role":"assistant","content":[{"type":"output_text","text":"calling"}]},
            {"type":"function_call","call_id":"call_1","name":"read_file","arguments":"{\"path\":\"a\"}"},
            {"type":"function_call_output","call_id":"call_1","output":"file body"},
            {"role":"user","content":[{"type":"input_text","text":"here"}]},
        ])
    );
}

async fn bodies(server: &MockServer) -> Vec<Value> {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect()
}

#[tokio::test]
async fn sends_expected_request_and_parses_text() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(header("authorization", "Bearer k"))
        .and(body_partial_json(json!({"stream": true, "model": "gpt-5"})))
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
async fn usage_maps_cached_tokens() {
    let server = serve(sse(fixture("text.sse"))).await;
    let evs = ok(run(&server).await);
    // input_tokens (120) includes the 80 cached ones; cached tokens are reported separately.
    assert!(evs.contains(&ProviderEvent::Usage(RawUsage {
        input: 40,
        output: 15,
        cache_read: 80,
        cache_write: 0
    })));
}

#[tokio::test]
async fn function_call_fixture_parses() {
    let server = serve(sse(fixture("function_call.sse"))).await;
    let evs = ok(run(&server).await);
    let calls: Vec<_> = evs
        .iter()
        .filter(|e| matches!(e, ProviderEvent::ToolCall { .. }))
        .collect();
    assert_eq!(
        calls,
        vec![&ProviderEvent::ToolCall {
            id: "call_abc".into(),
            name: "read_file".into(),
            input_json: "{\"path\":\"a.rs\"}".into()
        }]
    );
    assert_eq!(
        evs.last(),
        Some(&ProviderEvent::Done {
            stop: StopReason::ToolUse
        })
    );
}

#[tokio::test]
async fn parses_reasoning_summary_fixture() {
    let server = serve(sse(fixture("reasoning.sse"))).await;
    let evs = ok(run(&server).await);
    assert_eq!(
        &evs[..3],
        &[
            ProviderEvent::ReasoningDelta("Thinking about ".into()),
            ProviderEvent::ReasoningDelta("the answer.".into()),
            ProviderEvent::TextDelta("42".into()),
        ]
    );
}

#[tokio::test]
async fn incomplete_max_output_tokens_is_max_tokens() {
    let server = serve(sse(fixture("incomplete.sse"))).await;
    let evs = ok(run(&server).await);
    assert_eq!(
        evs.last(),
        Some(&ProviderEvent::Done {
            stop: StopReason::MaxTokens
        })
    );
}

#[tokio::test]
async fn x_ratelimit_headers_become_quota() {
    let resp = sse(fixture("text.sse"))
        .insert_header("x-ratelimit-remaining-requests", "49")
        .insert_header("x-ratelimit-remaining-tokens", "39000")
        .insert_header("x-ratelimit-reset-requests", "1s")
        .insert_header("x-ratelimit-reset-tokens", "6m0s");
    let server = serve(resp).await;
    let before = SystemTime::now();
    let evs = ok(run(&server).await);
    let after = SystemTime::now();
    let ProviderEvent::Quota(q) = &evs[0] else {
        panic!("first event should be Quota, got {:?}", evs[0]);
    };
    assert_eq!(q.remaining_requests, Some(49));
    assert_eq!(q.remaining_tokens, Some(39_000));
    // The later reset (6 minutes) wins.
    let reset = q.reset_at.unwrap();
    assert!(reset >= before + Duration::from_secs(360));
    assert!(reset <= after + Duration::from_secs(360));
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

    let server = serve(ResponseTemplate::new(503).set_body_string("upstream down")).await;
    assert!(matches!(
        run(&server).await[0],
        Err(ProviderError::Transient {
            status: Some(503),
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

    let server = serve(ResponseTemplate::new(400).set_body_string(
        r#"{"error":{"message":"This model's maximum context length is 8192 tokens.","code":null}}"#,
    ))
    .await;
    assert!(matches!(
        run(&server).await[0],
        Err(ProviderError::ContextOverflow(_))
    ));

    let server = serve(ResponseTemplate::new(400).set_body_string(
        r#"{"error":{"message":"Unsupported parameter: foo","code":"unsupported_parameter"}}"#,
    ))
    .await;
    assert!(matches!(
        run(&server).await[0],
        Err(ProviderError::InvalidRequest(_))
    ));
}

#[tokio::test]
async fn mid_stream_failures_are_classified() {
    let server = serve(sse(fixture("failed.sse"))).await;
    assert!(matches!(
        run(&server).await[0],
        Err(ProviderError::Transient { status: None, .. })
    ));

    let server = serve(sse(fixture("error.sse"))).await;
    assert!(matches!(
        run(&server).await[0],
        Err(ProviderError::Other(_))
    ));
}

#[tokio::test]
async fn stream_without_completed_is_transient() {
    let truncated: String = fixture("text.sse")
        .split("event: response.completed")
        .next()
        .unwrap()
        .to_string();
    let server = serve(sse(truncated)).await;
    let r = run(&server).await;
    assert!(matches!(
        r.last().unwrap(),
        Err(ProviderError::Transient { status: None, .. })
    ));
}

#[tokio::test]
async fn tool_call_is_not_emitted_for_unfinished_arguments() {
    let partial: String = fixture("function_call.sse")
        .split("event: response.function_call_arguments.done")
        .next()
        .unwrap()
        .to_string();
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
    let p = OpenAiProvider::new(
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
fn user_image_content_becomes_input_image_data_url() {
    let req = req_of(vec![msg(
        Role::User,
        vec![
            text("look"),
            Content::Image {
                media_type: "image/png".into(),
                data: vec![1, 2, 3],
            },
        ],
    )]);
    let w = to_wire(&req);
    assert_eq!(
        w["input"],
        json!([{"role":"user","content":[
            {"type":"input_text","text":"look"},
            {"type":"input_image","image_url":"data:image/png;base64,AQID"},
        ]}])
    );
}

#[tokio::test]
async fn encrypted_reasoning_item_round_trips_into_the_next_request() {
    let server = serve(sse(fixture("reasoning_encrypted.sse"))).await;
    let evs = ok(run(&server).await);
    // The summary still streams for display; the item itself arrives as opaque reasoning.
    assert_eq!(
        evs[0],
        ProviderEvent::ReasoningDelta("Need the file.".into())
    );
    let data = evs
        .iter()
        .find_map(|e| match e {
            ProviderEvent::RedactedReasoning(d) => Some(d.clone()),
            _ => None,
        })
        .expect("a reasoning item event");
    let pos = |pred: &dyn Fn(&ProviderEvent) -> bool| evs.iter().position(pred).unwrap();
    assert!(
        pos(&|e| matches!(e, ProviderEvent::RedactedReasoning(_)))
            < pos(&|e| matches!(e, ProviderEvent::ToolCall { .. }))
    );

    // The engine stores what it received, in order, and sends it back.
    let next = req_of(vec![
        msg(Role::User, vec![text("q")]),
        msg(
            Role::Assistant,
            vec![
                Content::RedactedReasoning { data },
                Content::ToolUse {
                    id: "call_9".into(),
                    name: "read_file".into(),
                    input: json!({"path": "a.rs"}),
                },
            ],
        ),
        msg(
            Role::User,
            vec![Content::ToolResult {
                id: "call_9".into(),
                output: "body".into(),
                is_error: false,
            }],
        ),
    ]);
    let w = to_wire(&next);
    assert_eq!(
        w["input"],
        json!([
            {"role":"user","content":[{"type":"input_text","text":"q"}]},
            {"type":"reasoning","id":"rs_9","encrypted_content":"ENC==",
             "summary":[{"type":"summary_text","text":"Need the file."}]},
            {"type":"function_call","call_id":"call_9","name":"read_file","arguments":"{\"path\":\"a.rs\"}"},
            {"type":"function_call_output","call_id":"call_9","output":"body"},
        ])
    );
}

#[test]
fn foreign_redacted_reasoning_is_not_sent_to_openai() {
    let req = req_of(vec![
        msg(Role::User, vec![text("q")]),
        msg(
            Role::Assistant,
            vec![
                Content::RedactedReasoning {
                    data: "anthropic-opaque".into(),
                },
                text("a"),
            ],
        ),
    ]);
    let w = to_wire(&req);
    assert_eq!(w["input"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn requests_are_stateless_and_never_reference_a_previous_response() {
    let server = serve(sse(fixture("text.sse"))).await;
    let p = provider(&server);
    for turn in 0..3 {
        let mut messages = vec![msg(Role::User, vec![text("q")])];
        for i in 0..turn {
            messages.push(msg(Role::Assistant, vec![text("Hello")]));
            messages.push(msg(Role::User, vec![text(&format!("n{i}"))]));
        }
        p.stream(req_of(messages), CancellationToken::new())
            .collect::<Vec<_>>()
            .await;
    }
    let b = bodies(&server).await;
    assert_eq!(b.len(), 3);
    for (i, body) in b.iter().enumerate() {
        assert!(body.get("previous_response_id").is_none(), "{body}");
        assert_eq!(body["store"], false);
        assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
        assert_eq!(body["input"].as_array().unwrap().len(), 1 + 2 * i);
    }
}
