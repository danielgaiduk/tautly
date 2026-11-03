use hx_provider::EditFormat;
use hx_sandbox::{Effect, LocalSandbox};
use hx_tools::{Tool, ToolCx, ToolResult, WebFetchTool};
use serde_json::{Value, json};
use tempfile::TempDir;
use wiremock::matchers::path;
use wiremock::{Mock, MockServer, ResponseTemplate};

fn env() -> (TempDir, LocalSandbox, ToolCx) {
    let dir = TempDir::new().unwrap();
    let sb = LocalSandbox::new(dir.path().to_path_buf()).unwrap();
    let cx = ToolCx::new(EditFormat::StrReplace, dir.path().join(".spill"));
    (dir, sb, cx)
}

async fn fetch(args: Value) -> ToolResult {
    let (_d, sb, cx) = env();
    WebFetchTool.run(args, &sb, &cx).await
}

async fn serve(route: &str, resp: ResponseTemplate) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(path(route))
        .respond_with(resp)
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn web_fetch_converts_html_to_markdown() {
    let html = "<html><head><title>T</title><style>p{color:red}</style></head><body>\
                <h1>Title</h1><p>Hello <a href=\"https://x.test/\">link</a></p>\
                <script>evil()</script></body></html>";
    let server = serve(
        "/page",
        ResponseTemplate::new(200).set_body_raw(html, "text/html; charset=utf-8"),
    )
    .await;
    let r = fetch(json!({"url": format!("{}/page", server.uri())})).await;
    assert!(!r.is_error, "{}", r.output);
    assert!(r.output.contains("# Title"), "{}", r.output);
    assert!(r.output.contains("[link](https://x.test/)"), "{}", r.output);
    assert!(!r.output.contains("<h1>"), "{}", r.output);
    assert!(!r.output.contains("evil()"), "{}", r.output);
    assert!(!r.output.contains("color:red"), "{}", r.output);
    assert!(r.effects.is_empty());
}

#[tokio::test]
async fn web_fetch_returns_plain_text_as_is() {
    let body = "# not <b>html</b>\nline two\n";
    let server = serve(
        "/t",
        ResponseTemplate::new(200).set_body_raw(body, "text/plain"),
    )
    .await;
    let r = fetch(json!({"url": format!("{}/t", server.uri())})).await;
    assert!(!r.is_error);
    assert_eq!(r.output, body);

    let server = serve(
        "/j",
        ResponseTemplate::new(200).set_body_raw("{\"a\":1}", "application/json"),
    )
    .await;
    let r = fetch(json!({"url": format!("{}/j", server.uri())})).await;
    assert_eq!(r.output, "{\"a\":1}");
}

#[tokio::test]
async fn web_fetch_caps_output() {
    let server = serve(
        "/big",
        ResponseTemplate::new(200).set_body_raw("x".repeat(200 * 1024), "text/plain"),
    )
    .await;
    let r = fetch(json!({"url": format!("{}/big", server.uri())})).await;
    assert!(!r.is_error);
    assert!(r.output.len() <= 31 * 1024, "{}", r.output.len());
    assert!(r.output.contains("bytes omitted"));
}

#[tokio::test]
async fn web_fetch_binary_content_is_error() {
    let server = serve(
        "/img",
        ResponseTemplate::new(200).set_body_raw(vec![0x89, b'P', b'N', b'G', 0, 1, 2], "image/png"),
    )
    .await;
    let r = fetch(json!({"url": format!("{}/img", server.uri())})).await;
    assert!(r.is_error);
    assert!(r.output.contains("binary"), "{}", r.output);

    // Mislabelled as text but full of NULs.
    let server = serve(
        "/lie",
        ResponseTemplate::new(200).set_body_raw(vec![0u8; 64], "text/plain"),
    )
    .await;
    let r = fetch(json!({"url": format!("{}/lie", server.uri())})).await;
    assert!(r.is_error);
}

#[tokio::test]
async fn web_fetch_http_error_status_is_error() {
    let server = serve("/gone", ResponseTemplate::new(404).set_body_string("nope")).await;
    let r = fetch(json!({"url": format!("{}/gone", server.uri())})).await;
    assert!(r.is_error);
    assert!(r.output.contains("404"), "{}", r.output);
}

#[tokio::test]
async fn web_fetch_only_allows_http_and_https() {
    for url in [
        "file:///etc/passwd",
        "ftp://example.com/x",
        "data:text/plain,hi",
        "not a url",
        "",
    ] {
        let r = fetch(json!({ "url": url })).await;
        assert!(r.is_error, "{url}");
    }
}

#[tokio::test]
async fn web_fetch_accepts_question_and_rejects_unknown_args() {
    let server = serve(
        "/q",
        ResponseTemplate::new(200).set_body_raw("body", "text/plain"),
    )
    .await;
    let url = format!("{}/q", server.uri());
    let r = fetch(json!({"url": url, "question": "what?"})).await;
    assert!(!r.is_error, "{}", r.output);
    assert_eq!(r.output, "body");
    let r = fetch(json!({"url": url, "bogus": 1})).await;
    assert!(r.is_error);
    let r = fetch(json!({})).await;
    assert!(r.is_error);
}

#[tokio::test]
async fn web_fetch_connection_failure_is_error_not_panic() {
    let r = fetch(json!({"url": "http://127.0.0.1:1/"})).await;
    assert!(r.is_error);
}

#[test]
fn web_fetch_effect_is_read_only_and_spec_is_strict() {
    let (_d, _sb, _cx) = env();
    assert_eq!(
        WebFetchTool
            .effect(&json!({"url": "https://a.test"}))
            .unwrap(),
        Effect::ReadOnly
    );
    assert!(WebFetchTool.effect(&json!({"nope": 1})).is_err());
    let spec = WebFetchTool.spec(&hx_provider::Capabilities::conservative());
    assert_eq!(spec.name, "web_fetch");
    assert_eq!(spec.schema["additionalProperties"], json!(false));
    assert_eq!(spec.schema["required"], json!(["url"]));
}
