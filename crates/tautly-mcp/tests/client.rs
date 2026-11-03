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

mod common;
use common::fixture;

const WAIT: Duration = Duration::from_secs(20);

fn stdio(mode: &str, env: &[(&str, &str)]) -> ServerSpec {
    ServerSpec::Stdio {
        command: fixture(),
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

const ECHO_TOOLS: [&str; 11] = [
    "mcp__echo__big",
    "mcp__echo__change_tools",
    "mcp__echo__echo",
    "mcp__echo__env",
    "mcp__echo__exit",
    "mcp__echo__fail",
    "mcp__echo__headers",
    "mcp__echo__mixed",
    "mcp__echo__slow",
    "mcp__echo__storm",
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
        ("echo", McpState::Ready, 11, ServerSource::User)
    );
    assert_eq!(names(&hub), ECHO_TOOLS);
    let echo = find(&hub, "mcp__echo__echo");
    let spec = echo.spec(&Capabilities::conservative());
    assert_eq!(spec.name, "mcp__echo__echo");
    // Marked as coming from the server.
    assert_eq!(spec.description, "[MCP server echo] Echo the text back");
    assert_eq!(spec.schema["properties"]["text"]["type"], "string");
    assert_eq!(
        echo.effect(&json!({"text": "hi"})).unwrap(),
        Effect::Exec {
            argv: vec![fixture(), "stdio".to_string()]
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
    let mut server = tokio::process::Command::new(fixture())
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
        if let Ok(text) = std::fs::read_to_string(path)
            && !text.is_empty()
        {
            return text;
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
            .any(|w| w.contains("71") && w.contains("50")),
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
    assert!(spec.description.starts_with("[MCP server echo] ddd"));
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
    for (mode, after) in [("stdio", 12), ("legacy", 3)] {
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

#[tokio::test]
async fn descriptions_and_results_marked_and_stripped_of_control_characters() {
    let s = setup();
    let hub = hub(
        &s,
        vec![("echo", stdio("stdio", &[("ECHO_CONTROL", "1")]))],
        &McpConfig::default(),
    )
    .await;
    let ctl = find(&hub, "mcp__echo__ctl");
    let spec = ctl.spec(&Capabilities::conservative());
    let desc = spec.description;
    assert_eq!(desc, "[MCP server echo] red[31m textreversed");
    // Strings inside the input schema reach the model too.
    assert_eq!(spec.schema["properties"]["x"]["title"], "t");
    assert_eq!(spec.schema["properties"]["x"]["description"], "d[2K");
    let r = call(&s, &ctl, json!({})).await;
    // Tabs and newlines stay; ESC, BEL and the like do not.
    assert_eq!(r.output, "a[2Kbc\td\ne");
    hub.shutdown().await;
}

#[tokio::test]
async fn dead_server_is_failed_and_its_tools_removed() {
    let s = setup();
    let hub = hub(
        &s,
        vec![("echo", stdio("stdio", &[]))],
        &McpConfig::default(),
    )
    .await;
    let mut changes = hub.changes();
    let r = call(&s, &find(&hub, "mcp__echo__exit"), json!({})).await;
    assert_eq!(r.output, "bye");
    let end = Instant::now() + WAIT;
    while hub.status()[0].state != McpState::Failed {
        assert!(Instant::now() < end, "{:?}", hub.status());
        let _ = tokio::time::timeout(Duration::from_millis(200), changes.changed()).await;
    }
    let st = &hub.status()[0];
    assert!(
        st.message.as_deref().unwrap_or("").contains("exited"),
        "{st:?}"
    );
    assert_eq!(st.tools, 0);
    assert!(hub.ready_tools().is_empty());
    hub.shutdown().await;
}

/// The server exits while a child that ignores SIGTERM keeps running in its group: the
/// child is killed (the group is signalled while the dead leader still holds its id) and
/// the leader is reaped, all without a shutdown.
#[tokio::test]
async fn exited_leader_group_is_killed_then_reaped() {
    let s = setup();
    let pid_file = s.sb.root().join("pids");
    // `$$` is the shell, which becomes the server (the group leader) by `exec`.
    let script = format!(
        "trap '' TERM; sleep 300 & echo $$ $! > '{}'; exec '{}' stdio",
        pid_file.display(),
        fixture()
    );
    let spec = ServerSpec::Stdio {
        command: "/bin/sh".into(),
        args: vec!["-c".into(), script],
        env: BTreeMap::new(),
        cwd: None,
    };
    let hub = hub(&s, vec![("echo", spec)], &McpConfig::default()).await;
    let pids: Vec<i32> = wait_for_file(&pid_file)
        .await
        .split_whitespace()
        .map(|p| p.parse().unwrap())
        .collect();
    let (leader, child) = (pids[0], pids[1]);
    assert!(alive(leader) && alive(child));
    let r = call(&s, &find(&hub, "mcp__echo__exit"), json!({})).await;
    assert_eq!(r.output, "bye");
    let end = Instant::now() + WAIT;
    while hub.status()[0].state != McpState::Failed {
        assert!(Instant::now() < end, "{:?}", hub.status());
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let end = Instant::now() + Duration::from_secs(5);
    while alive(child) && Instant::now() < end {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!alive(child), "the SIGTERM-ignoring child survived");
    // The leader is reaped too (a zombie would still answer signal 0).
    while alive(leader) && Instant::now() < end {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!alive(leader), "the leader was not reaped");
    hub.shutdown().await;
}

#[tokio::test]
async fn connect_timeout_kills_the_process_group() {
    let s = setup();
    let pid_file = s.sb.root().join("pids");
    let pid_env = pid_file.to_string_lossy().into_owned();
    let hub = McpHub::start_with_connect_timeout(
        vec![(
            "stuck".into(),
            stdio("hang", &[("ECHO_PID_FILE", &pid_env)]),
            ServerSource::User,
        )],
        Arc::clone(&s.sb),
        &McpConfig::default(),
        &[],
        Duration::from_secs(1),
    )
    .await;
    let pids: Vec<i32> = wait_for_file(&pid_file)
        .await
        .lines()
        .map(|l| l.parse().unwrap())
        .collect();
    assert!(hub.wait_started(WAIT).await);
    assert_eq!(hub.status()[0].state, McpState::Failed);
    // Without a shutdown: the timed-out server's whole group is gone.
    let end = Instant::now() + Duration::from_secs(5);
    while pids.iter().any(|&p| alive(p)) && Instant::now() < end {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!pids.iter().any(|&p| alive(p)), "{pids:?} still running");
    hub.shutdown().await;
}

#[tokio::test]
async fn list_changed_storm_is_coalesced() {
    let s = setup();
    let calls = s.sb.root().join("calls");
    let calls_env = calls.to_string_lossy().into_owned();
    let hub = hub(
        &s,
        vec![("echo", stdio("stdio", &[("ECHO_CALLS", &calls_env)]))],
        &McpConfig::default(),
    )
    .await;
    let r = call(&s, &find(&hub, "mcp__echo__storm"), json!({})).await;
    assert_eq!(r.output, "stormed");
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let text = std::fs::read_to_string(&calls).unwrap();
    let lists = text.lines().filter(|l| *l == "tools/list").count();
    // The first listing, then a few coalesced ones: never one per notification.
    assert!(lists <= 10, "{lists} listings for 200 notifications");
    assert!(hub.take_warnings().len() <= 20);
    assert_eq!(hub.status()[0].state, McpState::Ready);
    hub.shutdown().await;
}

#[tokio::test]
async fn endless_pagination_is_capped() {
    let s = setup();
    let cfg = McpConfig {
        max_tools: 5,
        ..McpConfig::default()
    };
    let hub = hub(
        &s,
        vec![("echo", stdio("stdio", &[("ECHO_PAGES", "endless")]))],
        &cfg,
    )
    .await;
    assert_eq!(hub.status()[0].state, McpState::Ready, "{:?}", hub.status());
    assert_eq!(hub.ready_tools().len(), 5);
    let warnings = hub.take_warnings();
    assert!(
        warnings.iter().any(|w| w.contains("stopped listing")),
        "{warnings:?}"
    );
    hub.shutdown().await;
}

#[tokio::test]
async fn total_tools_capped_across_servers() {
    let s = setup();
    let servers: Vec<(String, ServerSpec, ServerSource)> = (0..5)
        .map(|i| {
            (
                format!("s{i}"),
                stdio("stdio", &[("ECHO_FILLER", "45")]),
                ServerSource::User,
            )
        })
        .collect();
    let hub = McpHub::start(servers, Arc::clone(&s.sb), &McpConfig::default(), &[]).await;
    assert!(hub.wait_started(WAIT).await);
    assert_eq!(hub.ready_tools().len(), tautly_mcp::MAX_TOTAL_TOOLS);
    let warnings = hub.take_warnings();
    assert!(
        warnings
            .iter()
            .any(|w| w.contains(&tautly_mcp::MAX_TOTAL_TOOLS.to_string())),
        "{warnings:?}"
    );
    hub.shutdown().await;
}

#[tokio::test]
async fn repository_headers_are_sent_literally_user_headers_expanded() {
    let s = setup();
    let url_file = s.sb.root().join("url");
    let mut server = tokio::process::Command::new(fixture())
        .arg("http")
        .arg(&url_file)
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let url: url::Url = wait_for_file(&url_file).await.parse().unwrap();
    let spec = ServerSpec::Http {
        url,
        headers: BTreeMap::from([("X-Home".to_string(), "${HOME}".to_string())]),
    };
    let servers = vec![
        ("mine".to_string(), spec.clone(), ServerSource::User),
        ("repo".to_string(), spec, ServerSource::Project),
    ];
    let hub = McpHub::start(servers, Arc::clone(&s.sb), &McpConfig::default(), &[]).await;
    assert!(hub.wait_started(WAIT).await, "{:?}", hub.status());
    let home = std::env::var("HOME").unwrap();
    let mine = call(&s, &find(&hub, "mcp__mine__headers"), json!({})).await;
    assert!(
        mine.output.contains(&format!("x-home: {home}")),
        "{}",
        mine.output
    );
    let repo = call(&s, &find(&hub, "mcp__repo__headers"), json!({})).await;
    assert!(repo.output.contains("x-home: ${HOME}"), "{}", repo.output);
    hub.shutdown().await;
    let _ = server.kill().await;
}
