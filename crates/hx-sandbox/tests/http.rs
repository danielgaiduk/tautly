use hx_sandbox::{LocalSandbox, Sandbox, SandboxError, Url};
use wiremock::matchers::path;
use wiremock::{Mock, MockServer, ResponseTemplate};

fn sandbox() -> (tempfile::TempDir, LocalSandbox) {
    let t = tempfile::tempdir().unwrap();
    let sb = LocalSandbox::new(t.path().to_path_buf()).unwrap();
    (t, sb)
}

#[tokio::test]
async fn http_get_happy_path() {
    let server = MockServer::start().await;
    Mock::given(path("/ok"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-test", "yes")
                .set_body_string("hello"),
        )
        .mount(&server)
        .await;
    let (_t, sb) = sandbox();
    let url = Url::parse(&format!("{}/ok", server.uri())).unwrap();
    let r = sb.http_get(&url).await.unwrap();
    assert_eq!(r.status, 200);
    assert_eq!(r.body, b"hello");
    assert!(r.headers.iter().any(|(k, v)| k == "x-test" && v == "yes"));
}

#[tokio::test]
async fn http_get_enforces_body_cap() {
    let server = MockServer::start().await;
    Mock::given(path("/big"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![b'a'; 10 * 1024 * 1024 + 1]))
        .mount(&server)
        .await;
    let (_t, sb) = sandbox();
    let url = Url::parse(&format!("{}/big", server.uri())).unwrap();
    assert!(matches!(
        sb.http_get(&url).await,
        Err(SandboxError::Network(_))
    ));
}

#[tokio::test]
async fn http_get_does_not_follow_redirects() {
    let server = MockServer::start().await;
    Mock::given(path("/loop"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", "/loop"))
        .mount(&server)
        .await;
    let (_t, sb) = sandbox();
    let url = Url::parse(&format!("{}/loop", server.uri())).unwrap();
    let r = sb.http_get(&url).await.unwrap();
    assert_eq!(r.status, 302);
    assert!(
        r.headers
            .iter()
            .any(|(k, v)| k == "location" && v == "/loop")
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}
