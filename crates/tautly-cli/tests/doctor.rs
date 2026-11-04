//! `tautly doctor` (spec 7.11.2): provider endpoints are a local mock server, the keychain
//! is a file. Feature `mock`: `cargo test -p tautly-cli --features mock`.

mod common;

use std::process::{Command, Output};

use common::{Scratch, stderr, stdout};
use serde_json::Value;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const ANTHROPIC_KEY: &str = "sk-ant-doctor-SECRET-1";

async fn run(mut cmd: Command) -> Output {
    tokio::task::spawn_blocking(move || cmd.output().unwrap())
        .await
        .unwrap()
}

fn git_init(s: &Scratch) {
    let ok = Command::new("git")
        .args(["init", "-q"])
        .current_dir(s.work.path())
        .status()
        .unwrap()
        .success();
    assert!(ok);
}

/// Lines starting with `level` (`ok`, `warn`, `fail`, `info`) for `check`.
fn lines<'a>(text: &'a str, level: &str, check: &str) -> Vec<&'a str> {
    let prefix = format!("{level:<4}  {check}: ");
    text.lines().filter(|l| l.starts_with(&prefix)).collect()
}

/// A scratch setup whose Anthropic endpoint is `server` and key is in the environment.
fn anthropic_on(s: &Scratch, server: &MockServer) {
    s.user_config(&format!(
        "[providers.anthropic]\nbase_url = \"{}\"\n",
        server.uri()
    ));
}

async fn models_endpoint(status: u16) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(status).set_body_string("{\"data\":[]}"))
        .mount(&server)
        .await;
    server
}

#[tokio::test(flavor = "multi_thread")]
async fn doctor_healthy_exits_0() {
    let server = models_endpoint(200).await;
    let s = Scratch::new();
    anthropic_on(&s, &server);
    git_init(&s);
    let mut cmd = s.command(&["doctor"]);
    cmd.env("ANTHROPIC_API_KEY", ANTHROPIC_KEY);
    let out = run(cmd).await;
    let text = stdout(&out);
    assert_eq!(out.status.code(), Some(0), "{text}\n{}", stderr(&out));
    assert!(!text.lines().any(|l| l.starts_with("fail")), "{text}");
    for (level, check, detail) in [
        ("ok", "config", "loaded"),
        (
            "ok",
            "model",
            "anthropic/claude-sonnet-5-5 (detected from ANTHROPIC_API_KEY)",
        ),
        ("ok", "keys", "anthropic: environment (ANTHROPIC_API_KEY)"),
        ("info", "keys", "openai: none"),
        ("ok", "keychain", "available (test secret store)"),
        ("ok", "network", "anthropic (127.0.0.1:"),
        ("ok", "network", "): key accepted (HTTP 200)"),
        ("ok", "data home", ""),
        ("ok", "sandbox", "root "),
        ("info", "sandbox", "no OS isolation in v1"),
        ("ok", "git", "git version"),
        ("info", "index", "not built yet"),
        ("info", "mcp", "no servers"),
    ] {
        assert!(
            lines(&text, level, check)
                .iter()
                .any(|l| l.contains(detail)),
            "no `{level} {check}: …{detail}…` in:\n{text}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn doctor_rejected_key_fails_exit_1() {
    let server = models_endpoint(401).await;
    let s = Scratch::new();
    anthropic_on(&s, &server);
    let mut cmd = s.command(&["doctor"]);
    cmd.env("ANTHROPIC_API_KEY", ANTHROPIC_KEY);
    let out = run(cmd).await;
    let text = stdout(&out);
    assert_eq!(out.status.code(), Some(1), "{text}");
    let host = server.address().to_string();
    assert_eq!(
        lines(&text, "fail", "network"),
        [format!(
            "fail  network: anthropic ({host}): key rejected (HTTP 401)"
        )],
        "{text}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn doctor_offline_skips_network() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    let s = Scratch::new();
    anthropic_on(&s, &server);
    let mut cmd = s.command(&["doctor", "--offline"]);
    cmd.env("ANTHROPIC_API_KEY", ANTHROPIC_KEY);
    let out = run(cmd).await;
    let text = stdout(&out);
    assert_eq!(out.status.code(), Some(0), "{text}");
    assert_eq!(
        lines(&text, "info", "network"),
        ["info  network: skipped (--offline)"]
    );
    server.verify().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn doctor_sends_key_in_header_not_url() {
    let server = MockServer::start().await;
    let no_query = |req: &Request| req.url.query().is_none();
    for (route, name, value) in [
        ("/v1/models", "x-api-key", "anthropic-key-1"),
        ("/v1/models", "authorization", "Bearer openai-key-2"),
        ("/v1beta/models", "x-goog-api-key", "google-key-3"),
        ("/api/v1/models", "authorization", "Bearer openrouter-key-4"),
    ] {
        Mock::given(method("GET"))
            .and(path(route))
            .and(header(name, value))
            .and(no_query)
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .expect(1)
            .mount(&server)
            .await;
    }
    let s = Scratch::new();
    let uri = server.uri();
    s.user_config(&format!(
        "[providers.anthropic]\nbase_url = \"{uri}\"\n[providers.openai]\nbase_url = \"{uri}\"\n\
         [providers.google]\nbase_url = \"{uri}\"\n\n[[providers.compat]]\nname = \"openrouter\"\n\
         base_url = \"{uri}/api/v1\"\napi_key_env = \"OPENROUTER_API_KEY\"\n"
    ));
    // One from the keychain, the rest from the environment.
    s.store_secret("google", "google-key-3");
    let mut cmd = s.command(&["doctor"]);
    cmd.env("ANTHROPIC_API_KEY", "anthropic-key-1")
        .env("OPENAI_API_KEY", "openai-key-2")
        .env("OPENROUTER_API_KEY", "openrouter-key-4");
    let out = run(cmd).await;
    let text = stdout(&out);
    assert_eq!(out.status.code(), Some(0), "{text}");
    assert_eq!(lines(&text, "ok", "network").len(), 4, "{text}");
    server.verify().await;
    for req in server.received_requests().await.unwrap() {
        let url = req.url.as_str();
        assert!(!url.contains("key-"), "a key in the URL: {url}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn doctor_never_prints_key_values() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
        .mount(&server)
        .await;
    let s = Scratch::new();
    // Every provider with a key points at the mock server.
    let uri = server.uri();
    s.user_config(&format!(
        "[providers.anthropic]\nbase_url = \"{uri}\"\n[providers.openai]\nbase_url = \"{uri}\"\n\n\
         [[providers.compat]]\nname = \"openrouter\"\nbase_url = \"{uri}/api/v1\"\n\
         api_key_env = \"OPENROUTER_API_KEY\"\n\n[[providers.compat]]\nname = \"ollama\"\n\
         base_url = \"{uri}/v1\"\n"
    ));
    s.store_secret("openai", "sk-openai-keychain-SECRET-2");
    s.store_secret("compat:ollama", "ollama-SECRET-3");
    for args in [
        &["doctor"][..],
        &["doctor", "--json"],
        &["doctor", "--offline"],
    ] {
        let mut cmd = s.command(args);
        cmd.env("ANTHROPIC_API_KEY", ANTHROPIC_KEY)
            .env("OPENROUTER_API_KEY", "sk-or-env-SECRET-4");
        let out = run(cmd).await;
        let all = format!("{}{}", stdout(&out), stderr(&out));
        assert!(!all.contains("SECRET"), "{args:?}: {all}");
        assert!(stdout(&out).contains("keychain"), "{args:?}: {all}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn doctor_missing_git_is_warn_not_fail() {
    let s = Scratch::new();
    let empty = tempfile::tempdir().unwrap();
    let mut cmd = s.command(&["doctor", "--offline"]);
    cmd.env("ANTHROPIC_API_KEY", ANTHROPIC_KEY)
        .env("PATH", empty.path());
    let out = run(cmd).await;
    let text = stdout(&out);
    assert_eq!(out.status.code(), Some(0), "{text}");
    let git = lines(&text, "warn", "git");
    assert_eq!(git.len(), 1, "{text}");
    assert!(git[0].contains("checkpoints"), "{text}");
}

#[tokio::test(flavor = "multi_thread")]
async fn doctor_reports_pending_repo_mcp_approval_as_warn() {
    let s = Scratch::new();
    s.project_config("[mcp.servers.echo]\ncommand = \"echo\"\nargs = [\"hi\"]\n");
    s.user_config("[mcp.servers.mine]\ncommand = \"true\"\n");
    let mut cmd = s.command(&["doctor", "--offline"]);
    cmd.env("ANTHROPIC_API_KEY", ANTHROPIC_KEY);
    let out = run(cmd).await;
    let text = stdout(&out);
    assert_eq!(out.status.code(), Some(0), "{text}");
    let pending = lines(&text, "warn", "mcp");
    assert_eq!(pending.len(), 1, "{text}");
    assert!(
        pending[0].contains("echo") && pending[0].contains("approval"),
        "{text}"
    );
    assert!(
        lines(&text, "ok", "mcp").iter().any(|l| l.contains("mine")),
        "{text}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn doctor_json_shape() {
    let s = Scratch::new();
    git_init(&s);
    let mut cmd = s.command(&["doctor", "--json", "--offline"]);
    cmd.env("ANTHROPIC_API_KEY", ANTHROPIC_KEY);
    let out = run(cmd).await;
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let home = s.home.path().canonicalize().unwrap();
    let work = s.work.path().canonicalize().unwrap();
    let text = stdout(&out)
        .replace(&home.display().to_string(), "[HOME]")
        .replace(&s.home.path().display().to_string(), "[HOME]")
        .replace(&work.display().to_string(), "[WORK]")
        .replace(&s.work.path().display().to_string(), "[WORK]");
    let mut v: Value = serde_json::from_str(&text).unwrap();
    for check in v["checks"].as_array_mut().unwrap() {
        if check["name"] == "git" {
            check["detail"] = Value::String("[git version]".into());
        }
    }
    insta::assert_json_snapshot!(v);
}

// ---- review I2: the index check only reads ----

#[tokio::test(flavor = "multi_thread")]
async fn doctor_reads_a_present_index_without_changing_it() {
    use std::sync::Arc;

    let s = Scratch::new();
    std::fs::write(s.work.path().join("a.rs"), "fn a() {}\n").unwrap();
    std::fs::write(s.work.path().join("b.rs"), "fn b() {}\n").unwrap();
    let root = s.work.path().canonicalize().unwrap();
    let db = s
        .home
        .path()
        .join("index")
        .join(format!("{}.db", tautly_session::paths::project_hash(&root)));
    let sandbox = Arc::new(tautly_sandbox::LocalSandbox::new(root.clone()).unwrap());
    let index = tautly_index::Index::open(&db, sandbox).unwrap();
    index.refresh().await.unwrap();
    drop(index);
    let before = std::fs::read(&db).unwrap();
    let modified = std::fs::metadata(&db).unwrap().modified().unwrap();

    // A session in the middle of a refresh holds the write lock: a reader is not held up
    // (opening the index for writing would wait for it, then fail).
    let mut writer = rusqlite::Connection::open(&db).unwrap();
    let tx = writer
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .unwrap();
    let mut cmd = s.command(&["doctor", "--offline"]);
    cmd.env("ANTHROPIC_API_KEY", ANTHROPIC_KEY);
    let start = std::time::Instant::now();
    let out = run(cmd).await;
    let took = start.elapsed();
    tx.rollback().unwrap();
    drop(writer);
    assert!(took < std::time::Duration::from_secs(3), "took {took:?}");
    let text = stdout(&out);
    assert_eq!(out.status.code(), Some(0), "{text}");
    let index = lines(&text, "ok", "index");
    assert_eq!(index.len(), 1, "{text}");
    assert!(index[0].contains("present, 2 files"), "{text}");
    assert_eq!(
        std::fs::read(&db).unwrap(),
        before,
        "the index is unchanged"
    );
    assert_eq!(
        std::fs::metadata(&db).unwrap().modified().unwrap(),
        modified
    );
}

// ---- review M6: a project cannot redirect the keyed request ----

#[tokio::test(flavor = "multi_thread")]
async fn doctor_ignores_a_project_base_url() {
    let user = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("x-api-key", ANTHROPIC_KEY))
        .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
        .expect(1)
        .mount(&user)
        .await;
    let repo = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
        .expect(0)
        .mount(&repo)
        .await;
    let s = Scratch::new();
    anthropic_on(&s, &user);
    s.project_config(&format!(
        "[providers.anthropic]\nbase_url = \"{}\"\napi_key_env = \"ANTHROPIC_API_KEY\"\n",
        repo.uri()
    ));
    let mut cmd = s.command(&["doctor"]);
    cmd.env("ANTHROPIC_API_KEY", ANTHROPIC_KEY);
    let out = run(cmd).await;
    let text = stdout(&out);
    assert_eq!(out.status.code(), Some(0), "{text}");
    assert!(
        lines(&text, "warn", "config")
            .iter()
            .any(|l| l.contains("ignored `providers`")),
        "{text}"
    );
    let host = user.address().to_string();
    assert!(
        lines(&text, "ok", "network")
            .iter()
            .any(|l| l.contains(&format!("anthropic ({host})"))),
        "{text}"
    );
    user.verify().await;
    repo.verify().await;
}
