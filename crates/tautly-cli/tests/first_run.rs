//! Zero-config first run (spec 7.11.2) through the `tautly` binary: detection, the startup
//! line and the setup hint. Feature `mock`: `cargo test -p tautly-cli --features mock`.

mod common;

use common::{Scratch, stderr, stdout};
use serde_json::{Value, json};

fn text(t: &str) -> Value {
    json!([{"text_delta": t}, {"done": {"stop": "end_turn"}}])
}

#[test]
fn mock_script_skips_detection() {
    let s = Scratch::new();
    // Reading this "keychain" fails: detection would end the run with an error.
    std::fs::write(s.store_path(), "not json").unwrap();
    let script = s.script(json!([text("Hello from the mock.")]));
    let out = s.run(&["-p", "hi", "--mock-script", &script]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(stdout(&out), "Hello from the mock.\n");
    assert!(!stderr(&out).contains("using "), "{}", stderr(&out));
}

#[test]
fn no_provider_found_exits_1_with_hint() {
    let s = Scratch::new();
    let out = s.run(&["-p", "hi"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    let err = stderr(&out);
    for needle in [
        "no model provider found",
        "ANTHROPIC_API_KEY",
        "OPENAI_API_KEY",
        "GEMINI_API_KEY",
        "OPENROUTER_API_KEY",
        "tautly auth add",
        "Ollama",
    ] {
        assert!(err.contains(needle), "{needle} missing from:\n{err}");
    }
    assert!(stdout(&out).is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn startup_line_on_stderr_not_stdout() {
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    let sse = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hello\"}}]}\n\n\
               data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
               data: [DONE]\n\n";
    Mock::given(method("POST"))
        .and(path("/api/v1/chat/completions"))
        .and(header("authorization", "Bearer sk-or-test"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse),
        )
        .expect(1..)
        .mount(&server)
        .await;

    let s = Scratch::new();
    // The built-in `openrouter` entry, pointed at the mock server.
    s.user_config(&format!(
        "[[providers.compat]]\nname = \"openrouter\"\nbase_url = \"{}/api/v1\"\n\
         api_key_env = \"OPENROUTER_API_KEY\"\n",
        server.uri()
    ));
    let mut cmd = s.command(&["-p", "hi", "--output", "json"]);
    cmd.env("OPENROUTER_API_KEY", "sk-or-test");
    let out = tokio::task::spawn_blocking(move || cmd.output().unwrap())
        .await
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let err = stderr(&out);
    assert!(
        err.starts_with(
            "using compat:openrouter/openai/gpt-5 (from OPENROUTER_API_KEY) · /model to switch \
             · tautly doctor to check\n"
        ),
        "{err}"
    );
    let text = stdout(&out);
    assert!(!text.contains("using "), "{text}");
    let events: Vec<Value> = text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("not JSON ({e}): {l}")))
        .collect();
    assert!(
        events.iter().any(|e| e["type"] == "turn_finished"),
        "{text}"
    );
    assert!(!text.contains("sk-or-test") && !err.contains("sk-or-test"));
}

#[test]
fn configured_model_without_key_fails_with_same_hint() {
    let s = Scratch::new();
    s.user_config("model = \"openai/gpt-5\"\n");
    let out = s.run(&["-p", "hi"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    let err = stderr(&out);
    assert!(err.contains("no API key for openai"), "{err}");
    for needle in [
        "OPENAI_API_KEY",
        "OPENROUTER_API_KEY",
        "tautly auth add",
        "Ollama",
    ] {
        assert!(err.contains(needle), "{needle} missing from:\n{err}");
    }
    assert!(
        !err.contains("using "),
        "a configured model is not announced: {err}"
    );
    assert!(stdout(&out).is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_keychain_key_is_detected_and_used_for_the_session() {
    use wiremock::matchers::{header, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // The session's provider gets its key from the same (file) keychain detection read:
    // the request carries it. The server refuses the request, which ends the turn.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(header("x-goog-api-key", "key-from-keychain"))
        .respond_with(ResponseTemplate::new(400).set_body_string("{\"error\":{}}"))
        .expect(1..)
        .mount(&server)
        .await;
    let s = Scratch::new();
    s.store_secret("google", "key-from-keychain");
    s.user_config(&format!(
        "[providers.google]\nbase_url = \"{}\"\n",
        server.uri()
    ));
    let mut cmd = s.command(&["-p", "hi", "--output", "json"]);
    let out = tokio::task::spawn_blocking(move || cmd.output().unwrap())
        .await
        .unwrap();
    let err = stderr(&out);
    assert!(
        err.starts_with("using google/gemini-3-pro (from the keychain)"),
        "{err}"
    );
    assert!(!err.contains("key-from-keychain") && !stdout(&out).contains("key-from-keychain"));
}
