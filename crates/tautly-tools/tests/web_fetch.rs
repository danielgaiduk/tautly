use serde_json::{Value, json};
use tautly_provider::EditFormat;
use tautly_sandbox::{Effect, LocalSandbox};
use tautly_tools::{Tool, ToolCx, ToolResult, WebFetchTool};
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
fn web_fetch_spec_is_strict() {
    assert!(WebFetchTool.effect(&json!({"nope": 1})).is_err());
    let spec = WebFetchTool.spec(&tautly_provider::Capabilities::conservative());
    assert_eq!(spec.name, "web_fetch");
    assert_eq!(spec.schema["additionalProperties"], json!(false));
    assert_eq!(spec.schema["required"], json!(["url"]));
}

#[test]
fn web_fetch_effect_names_the_host() {
    let e = |url: &str| WebFetchTool.effect(&json!({ "url": url }));
    assert_eq!(
        e("https://Docs.Rust-Lang.org/std?q=1").unwrap(),
        Effect::Network {
            host: "docs.rust-lang.org".into()
        }
    );
    assert_eq!(
        e("http://127.0.0.1:8080/x").unwrap(),
        Effect::Network {
            host: "127.0.0.1".into()
        }
    );
    // Canonical form: lowercase, no port, no trailing dot, no IPv6 brackets.
    for (url, host) in [
        ("http://evil.test./", "evil.test"),
        ("http://x.evil.test../p", "x.evil.test"),
        ("HTTP://EVIL.TEST.:8080/", "evil.test"),
        ("http://localhost./", "localhost"),
        ("http://[::1]/", "::1"),
        ("http://[::1]:3000/x", "::1"),
        ("http://[FE80::1]/", "fe80::1"),
    ] {
        assert_eq!(
            e(url).unwrap(),
            Effect::Network { host: host.into() },
            "{url}"
        );
    }
    for bad in ["file:///etc/passwd", "not a url", "", "ftp://a.test/"] {
        assert!(e(bad).is_err(), "{bad}");
    }
}

#[tokio::test]
async fn web_fetch_surfaces_redirects_without_following() {
    let server = MockServer::start().await;
    Mock::given(path("/old"))
        .respond_with(ResponseTemplate::new(301).insert_header("location", "/new"))
        .mount(&server)
        .await;
    Mock::given(path("/abs"))
        .respond_with(
            ResponseTemplate::new(302).insert_header("location", "http://169.254.169.254/meta"),
        )
        .mount(&server)
        .await;
    Mock::given(path("/new"))
        .respond_with(ResponseTemplate::new(200).set_body_raw("moved", "text/plain"))
        .mount(&server)
        .await;
    Mock::given(path("/nowhere"))
        .respond_with(ResponseTemplate::new(302))
        .mount(&server)
        .await;

    let r = fetch(json!({"url": format!("{}/old", server.uri())})).await;
    assert!(!r.is_error, "{}", r.output);
    assert_eq!(
        r.output,
        format!(
            "redirected (301) to {}/new; call web_fetch with that URL to follow it",
            server.uri()
        )
    );
    let r = fetch(json!({"url": format!("{}/abs", server.uri())})).await;
    assert!(!r.is_error);
    assert_eq!(
        r.output,
        "redirected (302) to http://169.254.169.254/meta; call web_fetch with that URL to follow it"
    );
    // A redirect with no Location is just a failed fetch.
    let r = fetch(json!({"url": format!("{}/nowhere", server.uri())})).await;
    assert!(r.is_error);
    // Nothing behind a redirect was requested.
    let paths: Vec<String> = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|r| r.url.path().to_string())
        .collect();
    assert!(!paths.contains(&"/new".to_string()), "{paths:?}");
}
