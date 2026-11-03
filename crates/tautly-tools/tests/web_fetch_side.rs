//! `web_fetch` with a `question` is answered by the side model (spec 7.5, Task 27).

use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tautly_provider::{EditFormat, ProviderError, SideModel};
use tautly_sandbox::LocalSandbox;
use tautly_tools::{Tool, ToolCx, ToolResult, WebFetchTool};
use tempfile::TempDir;
use wiremock::matchers::path;
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Records what it was asked and answers with a script.
struct FakeSide {
    answer: Result<String, ProviderError>,
    asked: Mutex<Vec<(String, String, u32)>>,
}

impl FakeSide {
    fn new(answer: Result<String, ProviderError>) -> Arc<FakeSide> {
        Arc::new(FakeSide {
            answer,
            asked: Mutex::new(Vec::new()),
        })
    }

    fn asked(&self) -> Vec<(String, String, u32)> {
        self.asked.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl SideModel for FakeSide {
    async fn complete(
        &self,
        system: &str,
        user: &str,
        max_tokens: u32,
    ) -> Result<String, ProviderError> {
        self.asked
            .lock()
            .unwrap()
            .push((system.into(), user.into(), max_tokens));
        self.answer.clone()
    }
}

async fn serve(route: &str, resp: ResponseTemplate) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(path(route))
        .respond_with(resp)
        .mount(&server)
        .await;
    server
}

async fn page(body: &str) -> (MockServer, String) {
    let server = serve(
        "/doc",
        ResponseTemplate::new(200).set_body_raw(body.to_string(), "text/plain"),
    )
    .await;
    let url = format!("{}/doc", server.uri());
    (server, url)
}

async fn fetch(side: Option<&Arc<FakeSide>>, args: Value) -> ToolResult {
    let dir = TempDir::new().unwrap();
    let sb = LocalSandbox::new(dir.path().to_path_buf()).unwrap();
    let mut cx = ToolCx::new(EditFormat::StrReplace, dir.path().join(".spill"));
    if let Some(s) = side {
        cx = cx.with_side(Arc::clone(s) as Arc<dyn SideModel>);
    }
    WebFetchTool.run(args, &sb, &cx).await
}

#[tokio::test]
async fn web_fetch_with_question_returns_answer_and_url_capped_at_1000_tokens() {
    let (_server, url) = page("The limit is 42 requests.").await;
    let side = FakeSide::new(Ok("42 requests per minute.".into()));
    let r = fetch(
        Some(&side),
        json!({"url": url, "question": "What is the limit?"}),
    )
    .await;
    assert!(!r.is_error, "{}", r.output);
    assert!(r.output.contains("42 requests per minute."), "{}", r.output);
    assert!(r.output.contains(&url), "{}", r.output);
    assert!(
        !r.output.contains("The limit is 42 requests."),
        "the page itself is not returned: {}",
        r.output
    );
    let asked = side.asked();
    assert_eq!(asked.len(), 1);
    let (system, user, max_tokens) = &asked[0];
    assert!(user.contains("What is the limit?"), "{user}");
    assert!(user.contains("The limit is 42 requests."), "{user}");
    assert!(system.contains("untrusted"), "{system}");
    assert_eq!(*max_tokens, 1000);

    // A runaway answer is cut to 1,000 tokens (about four bytes each), the URL included.
    let side = FakeSide::new(Ok("word ".repeat(20_000)));
    let r = fetch(Some(&side), json!({"url": url, "question": "q"})).await;
    assert!(!r.is_error, "{}", r.output);
    assert!(r.output.len() <= 4_000, "{} bytes", r.output.len());
    assert!(r.output.contains(&url), "{}", r.output);
}

#[tokio::test]
async fn web_fetch_without_question_unchanged() {
    let (_server, url) = page("page text").await;
    let side = FakeSide::new(Ok("unused".into()));
    let r = fetch(Some(&side), json!({ "url": url })).await;
    assert!(!r.is_error);
    assert_eq!(r.output, "page text");
    assert!(side.asked().is_empty());
    // An empty question is no question.
    let r = fetch(Some(&side), json!({"url": url, "question": "  "})).await;
    assert_eq!(r.output, "page text");
    assert!(side.asked().is_empty());
    // And with no side model at all the question changes nothing either.
    let r = fetch(None, json!({"url": url, "question": "what?"})).await;
    assert_eq!(r.output, "page text");
}

#[tokio::test]
async fn web_fetch_question_falls_back_to_the_page_when_the_side_model_fails() {
    let (_server, url) = page("page text").await;
    let side = FakeSide::new(Err(ProviderError::Auth("bad key".into())));
    let r = fetch(Some(&side), json!({"url": url, "question": "q"})).await;
    assert!(!r.is_error, "{}", r.output);
    assert!(r.output.contains("page text"), "{}", r.output);
    assert!(r.output.contains("bad key"), "{}", r.output);
}

#[tokio::test]
async fn web_fetch_question_is_not_asked_when_the_fetch_fails() {
    let server = serve("/gone", ResponseTemplate::new(404)).await;
    let side = FakeSide::new(Ok("unused".into()));
    let r = fetch(
        Some(&side),
        json!({"url": format!("{}/gone", server.uri()), "question": "q"}),
    )
    .await;
    assert!(r.is_error);
    assert!(side.asked().is_empty());
}

#[tokio::test]
async fn a_huge_page_is_cut_before_it_is_sent_to_the_side_model() {
    let (_server, url) = page(&"y".repeat(2 * 1024 * 1024)).await;
    let side = FakeSide::new(Ok("ok".into()));
    let r = fetch(Some(&side), json!({"url": url, "question": "q"})).await;
    assert!(!r.is_error, "{}", r.output);
    let user = &side.asked()[0].1;
    assert!(user.len() <= 130 * 1024, "{} bytes", user.len());
}
