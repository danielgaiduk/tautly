//! `McpHub` against the `echo_server` fixture: stdio, streamable HTTP and a legacy
//! `initialize`-only server.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tautly_mcp::{McpConfig, McpHub, McpState, ServerSource, ServerSpec};
use tautly_provider::{Capabilities, EditFormat};
use tautly_sandbox::{Effect, LocalSandbox, Sandbox};
use tautly_tools::{AppliedEffect, Tool, ToolCx};

const FIXTURE: &str = env!("CARGO_BIN_EXE_echo_server");
const WAIT: Duration = Duration::from_secs(20);

fn stdio(mode: &str, env: &[(&str, &str)]) -> ServerSpec {
    ServerSpec::Stdio {
        command: FIXTURE.into(),
        args: vec![mode.into()],
        env: env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        cwd: None,
    }
}

struct Setup {
    _dir: tempfile::TempDir,
    sb: Arc<dyn Sandbox>,
}

fn setup() -> Setup {
    let dir = tempfile::tempdir().unwrap();
    let sb: Arc<dyn Sandbox> = Arc::new(LocalSandbox::new(dir.path().to_path_buf()).unwrap());
    Setup { _dir: dir, sb }
}

async fn hub(s: &Setup, servers: Vec<(&str, ServerSpec)>, cfg: &McpConfig) -> McpHub {
    let servers = servers
        .into_iter()
        .map(|(n, spec)| (n.to_string(), spec, ServerSource::User))
        .collect();
    let hub = McpHub::start(servers, Arc::clone(&s.sb), cfg, &[]).await;
    assert!(hub.wait_started(WAIT).await, "{:?}", hub.status());
    hub
}

fn find(hub: &McpHub, name: &str) -> Arc<dyn Tool> {
    hub.ready_tools()
        .into_iter()
        .find(|t| t.name() == name)
        .unwrap_or_else(|| panic!("no tool {name}: {:?}", names(hub)))
}

fn names(hub: &McpHub) -> Vec<String> {
    hub.ready_tools()
        .iter()
        .map(|t| t.name().to_string())
        .collect()
}

async fn call(s: &Setup, tool: &Arc<dyn Tool>, args: Value) -> tautly_tools::ToolResult {
    let cx = ToolCx::new(
        EditFormat::StrReplace,
        s.sb.root().join(".tautly/tmp/spill"),
    );
    tool.run(args, s.sb.as_ref(), &cx).await
}

const ECHO_TOOLS: [&str; 8] = [
    "mcp__echo__big",
    "mcp__echo__change_tools",
    "mcp__echo__echo",
    "mcp__echo__env",
    "mcp__echo__fail",
    "mcp__echo__mixed",
    "mcp__echo__slow",
    "mcp__echo__structured",
];

#[tokio::test]
async fn stdio_server_lists_and_calls_tools() {
    let s = setup();
    let cfg = McpConfig::default();
    let hub = hub(&s, vec![("echo", stdio("stdio", &[]))], &cfg).await;
    let st = hub.status();
    assert_eq!(st.len(), 1);
    assert_eq!(
        (st[0].name.as_str(), st[0].state, st[0].tools, st[0].source),
        ("echo", McpState::Ready, 8, ServerSource::User)
    );
    assert_eq!(names(&hub), ECHO_TOOLS);
    let echo = find(&hub, "mcp__echo__echo");
    let spec = echo.spec(&Capabilities::conservative());
    assert_eq!(spec.name, "mcp__echo__echo");
    assert_eq!(spec.description, "Echo the text back");
    assert_eq!(spec.schema["properties"]["text"]["type"], "string");
    assert_eq!(
        echo.effect(&json!({"text": "hi"})).unwrap(),
        Effect::Exec {
            argv: vec![FIXTURE.to_string(), "stdio".to_string()]
        }
    );
    let r = call(&s, &echo, json!({"text": "hi there"})).await;
    assert!(!r.is_error, "{r:?}");
    assert_eq!(r.output, "hi there");
    // A stdio server may change files, like a command.
    assert_eq!(r.effects, vec![AppliedEffect::Exec { may_write: true }]);
    hub.shutdown().await;
}

#[tokio::test]
async fn http_server_lists_and_calls_tools() {
    let s = setup();
    let url_file = s.sb.root().join("url");
    let mut server = tokio::process::Command::new(FIXTURE)
        .arg("http")
        .arg(&url_file)
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let url = wait_for_file(&url_file).await;
    let cfg = McpConfig::default();
    let spec = ServerSpec::Http {
        url: url.parse().unwrap(),
        headers: BTreeMap::from([("X-Test".to_string(), "1".to_string())]),
    };
    let hub = hub(&s, vec![("web", spec)], &cfg).await;
    assert_eq!(hub.status()[0].state, McpState::Ready, "{:?}", hub.status());
    let echo = find(&hub, "mcp__web__echo");
    assert_eq!(
        echo.effect(&json!({})).unwrap(),
        Effect::Network {
            host: "127.0.0.1".into()
        }
    );
    let r = call(&s, &echo, json!({"text": "over http"})).await;
    assert!(!r.is_error, "{r:?}");
    assert_eq!(r.output, "over http");
    assert!(r.effects.is_empty());
    hub.shutdown().await;
    let _ = server.kill().await;
}

async fn wait_for_file(path: &Path) -> String {
    let end = Instant::now() + WAIT;
    loop {
        if let Ok(text) = std::fs::read_to_string(path) {
            if !text.is_empty() {
                return text;
            }
        }
        assert!(Instant::now() < end, "no {}", path.display());
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn legacy_initialize_handshake_server_supported() {
    let s = setup();
    let hub = hub(
        &s,
        vec![("old", stdio("legacy", &[]))],
        &McpConfig::default(),
    )
    .await;
    assert_eq!(hub.status()[0].state, McpState::Ready, "{:?}", hub.status());
    assert_eq!(names(&hub), ["mcp__old__change_tools", "mcp__old__echo"]);
    let r = call(&s, &find(&hub, "mcp__old__echo"), json!({"text": "legacy"})).await;
    assert!(!r.is_error, "{r:?}");
    assert_eq!(r.output, "legacy");
    hub.shutdown().await;
}

#[tokio::test]
async fn call_timeout_respected() {
    let s = setup();
    let cfg = McpConfig {
        timeout_s: 1,
        ..McpConfig::default()
    };
    let hub = hub(&s, vec![("echo", stdio("stdio", &[]))], &cfg).await;
    let slow = find(&hub, "mcp__echo__slow");
    let t = Instant::now();
    let r = call(&s, &slow, json!({"ms": 10_000})).await;
    assert!(r.is_error, "{r:?}");
    assert!(r.output.contains("timed out after 1 s"), "{}", r.output);
    assert!(t.elapsed() < Duration::from_secs(4), "{:?}", t.elapsed());
    // The server is still usable.
    let r = call(&s, &find(&hub, "mcp__echo__echo"), json!({"text": "ok"})).await;
    assert_eq!(r.output, "ok");
    hub.shutdown().await;
}

#[tokio::test]
async fn oversized_schema_tool_skipped_with_warning() {
    let s = setup();
    let hub = hub(
        &s,
        vec![("echo", stdio("stdio", &[("ECHO_BIG_SCHEMA", "1")]))],
        &McpConfig::default(),
    )
    .await;
    assert_eq!(names(&hub), ECHO_TOOLS);
    let warnings = hub.take_warnings();
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("big_schema") && w.contains("8192")),
        "{warnings:?}"
    );
    assert!(hub.take_warnings().is_empty());
    hub.shutdown().await;
}

#[tokio::test]
async fn at_most_50_tools_per_server() {
    let s = setup();
    let hub = hub(
        &s,
        vec![("echo", stdio("stdio", &[("ECHO_FILLER", "60")]))],
        &McpConfig::default(),
    )
    .await;
    let tools = names(&hub);
    assert_eq!(tools.len(), 50);
    assert_eq!(hub.status()[0].tools, 50);
    let warnings = hub.take_warnings();
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("68") && w.contains("50")),
        "{warnings:?}"
    );
    // A smaller cap from the configuration.
    let s2 = setup();
    let cfg = McpConfig {
        max_tools: 3,
        ..McpConfig::default()
    };
    let small = self::hub(&s2, vec![("echo", stdio("stdio", &[]))], &cfg).await;
    assert_eq!(small.ready_tools().len(), 3);
    hub.shutdown().await;
    small.shutdown().await;
}

#[tokio::test]
async fn description_truncated_to_1kb() {
    let s = setup();
    let hub = hub(
        &s,
        vec![("echo", stdio("stdio", &[("ECHO_LONG_DESC", "1")]))],
        &McpConfig::default(),
    )
    .await;
    let spec = find(&hub, "mcp__echo__long_desc").spec(&Capabilities::conservative());
    assert!(spec.description.len() <= 1024, "{}", spec.description.len());
    assert!(spec.description.starts_with("ddd"));
    assert!(spec.description.len() > 900);
    hub.shutdown().await;
}

#[tokio::test]
async fn result_mapping_text_structured_image_link() {
    let s = setup();
    let hub = hub(
        &s,
        vec![("echo", stdio("stdio", &[]))],
        &McpConfig::default(),
    )
    .await;
    let r = call(&s, &find(&hub, "mcp__echo__mixed"), json!({})).await;
    assert!(!r.is_error, "{r:?}");
    let lines: Vec<&str> = r.output.lines().collect();
    assert_eq!(lines[0], "hello");
    assert!(lines[1].starts_with("[image: image/png"), "{}", r.output);
    assert_eq!(lines[2], "[resource link: report <file:///tmp/report.txt>]");
    assert_eq!(lines[3], "world");

    let r = call(&s, &find(&hub, "mcp__echo__structured"), json!({})).await;
    assert!(!r.is_error, "{r:?}");
    let v: Value = serde_json::from_str(&r.output).unwrap();
    assert_eq!(v, json!({"answer": 42}));

    let r = call(&s, &find(&hub, "mcp__echo__fail"), json!({})).await;
    assert!(r.is_error);
    assert_eq!(r.output, "boom");
    hub.shutdown().await;
}

#[tokio::test]
async fn result_capped_at_30kb_with_spill() {
    let s = setup();
    let hub = hub(
        &s,
        vec![("echo", stdio("stdio", &[]))],
        &McpConfig::default(),
    )
    .await;
    let r = call(&s, &find(&hub, "mcp__echo__big"), json!({"bytes": 100_000})).await;
    assert!(!r.is_error);
    assert!(r.output.len() < 31 * 1024, "{}", r.output.len());
    assert!(r.output.contains("bytes omitted"), "{}", &r.output[..200]);
    let saved = r
        .output
        .lines()
        .find_map(|l| l.strip_prefix("[full output saved to "))
        .and_then(|l| l.strip_suffix(']'))
        .unwrap_or_else(|| panic!("no spill note"));
    let full = std::fs::read_to_string(s.sb.root().join(saved)).unwrap();
    assert_eq!(full.len(), 100_000);
    hub.shutdown().await;
}

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

#[tokio::test]
async fn stdio_server_killed_on_shutdown() {
    let s = setup();
    let pid_file: PathBuf = s.sb.root().join("pids");
    let pid_env = pid_file.to_string_lossy().into_owned();
    let hub = hub(
        &s,
        vec![("echo", stdio("stdio", &[("ECHO_PID_FILE", &pid_env)]))],
        &McpConfig::default(),
    )
    .await;
    let pids: Vec<i32> = wait_for_file(&pid_file)
        .await
        .lines()
        .map(|l| l.parse().unwrap())
        .collect();
    assert_eq!(pids.len(), 2);
    assert!(pids.iter().all(|&p| alive(p)));
    hub.shutdown().await;
    // The server and the child it started (same process group) are gone.
    let end = Instant::now() + Duration::from_secs(5);
    while pids.iter().any(|&p| alive(p)) && Instant::now() < end {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!pids.iter().any(|&p| alive(p)), "{pids:?} still running");
    assert!(hub.ready_tools().is_empty());
}

#[tokio::test]
async fn list_changed_updates_the_tool_list() {
    let s = setup();
    // A 2026-07-28 server sends it on a subscription, an older one directly.
    for (mode, after) in [("stdio", 9), ("legacy", 3)] {
        let hub = hub(&s, vec![("echo", stdio(mode, &[]))], &McpConfig::default()).await;
        let mut changes = hub.changes();
        changes.borrow_and_update();
        let r = call(&s, &find(&hub, "mcp__echo__change_tools"), json!({})).await;
        assert_eq!(r.output, "changed");
        let end = Instant::now() + WAIT;
        while !names(&hub).contains(&"mcp__echo__extra".to_string()) {
            assert!(Instant::now() < end, "{:?}", names(&hub));
            let _ = tokio::time::timeout(Duration::from_millis(200), changes.changed()).await;
        }
        assert_eq!(hub.status()[0].tools, after, "{mode}");
        hub.shutdown().await;
    }
}

#[tokio::test]
async fn failed_server_is_reported() {
    let s = setup();
    let cfg = McpConfig::default();
    let servers = vec![(
        "gone".to_string(),
        ServerSpec::Stdio {
            command: "/nonexistent/tautly-mcp-server".into(),
            args: vec![],
            env: BTreeMap::new(),
            cwd: None,
        },
        ServerSource::User,
    )];
    let hub = McpHub::start(servers, Arc::clone(&s.sb), &cfg, &[]).await;
    assert!(hub.wait_started(WAIT).await);
    let st = hub.status();
    assert_eq!(st[0].state, McpState::Failed);
    assert!(st[0].message.is_some());
    assert!(hub.ready_tools().is_empty());
    hub.shutdown().await;
}
