//! MCP servers in the engine (spec 7.11.10): repository-server approval, the tool set per
//! user turn, policy, checkpoints and the loop breaker. The servers are `tautly-mcp`'s
//! `echo_server` fixture (crate `tautly-mcp-fixture`).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::json;
use tautly_core::record::ApprovalKind;
use tautly_core::{
    Command, Config, EngineBuilder, Event, McpState, PreExecHook, ServerSpec, SessionRecord,
};
use tautly_mcp::approvals::key;
use tautly_mcp::{APPROVALS_FILE, ApprovalStore};
use tautly_provider::mock::{call, text_step_no_usage, tools_step};
use tautly_provider::{
    CanonicalRequest, Catalog, MockProvider, MockTurn, ModelId, ProviderEvent, StopReason,
};
use tautly_sandbox::Mode;
use tautly_session::SessionReader;
use tautly_testkit::{Fx, MODEL, Run, catalog};

const WAIT: Duration = Duration::from_secs(20);
/// Tools the fixture offers by default.
const ECHO_TOOL_COUNT: usize = 11;

/// The `echo_server` binary of `tautly-mcp-fixture`, next to this test's directory,
/// brought up to date once per test binary (a test run of this package alone does not
/// rebuild another package's binary).
fn fixture() -> String {
    static PATH: OnceLock<String> = OnceLock::new();
    PATH.get_or_init(|| {
        let exe = std::env::current_exe().unwrap();
        let dir = exe.parent().unwrap().parent().unwrap();
        let bin = dir.join("echo_server");
        {
            let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
            let status = std::process::Command::new(cargo)
                .args(["build", "-p", "tautly-mcp-fixture", "--bin", "echo_server"])
                .status()
                .unwrap();
            assert!(status.success(), "could not build the echo_server fixture");
        }
        assert!(bin.exists(), "no fixture at {}", bin.display());
        bin.to_string_lossy().into_owned()
    })
    .clone()
}

fn config(mode: Mode) -> Config {
    let mut c = Config {
        model: ModelId::from(MODEL),
        catalog_overrides: Some(catalog(200_000, 8000, "str_replace")),
        ..Config::default()
    };
    c.permissions.mode = mode;
    // The fixture server may take more than the default 5 s to start on a loaded runner,
    // and the first request goes without its tools then. Tests of the wait set their own.
    c.mcp.startup_timeout_ms = 60_000;
    c
}

fn stdio(env: &[(&str, &str)]) -> ServerSpec {
    ServerSpec::Stdio {
        command: fixture(),
        args: vec!["stdio".into()],
        env: env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        cwd: None,
    }
}

trait FxExt {
    fn path(&self, rel: &str) -> PathBuf;
    fn project_server(&self, name: &str, env: &[(&str, &str)]);
    fn loaded(&self, mode: Mode) -> (Config, Vec<String>);
    fn waiting_builder(&self, cfg: Config, mock: &MockProvider) -> EngineBuilder;
    fn approvals(&self) -> ApprovalStore;
}

impl FxExt for Fx {
    fn path(&self, rel: &str) -> PathBuf {
        self.root().join(rel)
    }

    /// A project config declaring `[mcp.servers.<name>]` = the fixture with `env`.
    fn project_server(&self, name: &str, env: &[(&str, &str)]) {
        let env: Vec<String> = env
            .iter()
            .map(|(k, v)| format!("{k} = {}", toml_str(v)))
            .collect();
        self.write(
            ".tautly/config.toml",
            &format!(
                "[mcp.servers.{name}]\ncommand = {}\nargs = [\"stdio\"]\nenv = {{ {} }}\n",
                toml_str(&fixture()),
                env.join(", ")
            ),
        );
    }

    /// The configuration as `Config::load` would produce it (no user file).
    fn loaded(&self, mode: Mode) -> (Config, Vec<String>) {
        let loaded =
            Config::load_from(None, &self.root(), tautly_core::CliOverrides::default()).unwrap();
        let mut cfg = loaded.config;
        cfg.model = ModelId::from(MODEL);
        cfg.catalog_overrides = Some(catalog(200_000, 8000, "str_replace"));
        cfg.permissions.mode = mode;
        (cfg, loaded.warnings)
    }

    fn waiting_builder(&self, cfg: Config, mock: &MockProvider) -> EngineBuilder {
        EngineBuilder {
            provider: Some(Arc::new(mock.clone())),
            data_home: Some(self.home.path().to_path_buf()),
            // The repo map is in every first request, however loaded the runner: the
            // context numbers some tests compare depend on it.
            startup_wait: WAIT,
            ..EngineBuilder::new(cfg, self.dir.path())
        }
    }

    fn approvals(&self) -> ApprovalStore {
        ApprovalStore::open(&self.home.path().join(APPROVALS_FILE))
    }
}

fn toml_str(s: &str) -> String {
    serde_json::to_string(s).unwrap()
}

fn start(b: EngineBuilder) -> Run {
    let mut run = tautly_testkit::start(b);
    run.wait = WAIT;
    run
}

trait RunExt {
    async fn until_state(&mut self, server: &str, state: McpState) -> Vec<Event>;
    async fn shutdown(self) -> Vec<Event>;
}

impl RunExt for Run {
    /// Events until `server` reports `state`.
    async fn until_state(&mut self, server: &str, state: McpState) -> Vec<Event> {
        self.until(|e| matches!(e, Event::McpServerStatus { server: s, state: st, .. } if s == server && *st == state))
            .await
    }

    /// Shut down and wait for the event stream to close.
    async fn shutdown(mut self) -> Vec<Event> {
        self.send(Command::Shutdown).await;
        let mut rest = Vec::new();
        while let Ok(Some(ev)) = tokio::time::timeout(WAIT, self.events.recv()).await {
            rest.push(ev);
        }
        rest
    }
}

fn tool_names(req: &CanonicalRequest) -> Vec<String> {
    req.tools.iter().map(|t| t.name.clone()).collect()
}

fn mcp_names(req: &CanonicalRequest) -> Vec<String> {
    tool_names(req)
        .into_iter()
        .filter(|n| n.starts_with("mcp__"))
        .collect()
}

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

fn pids(path: &Path) -> Vec<i32> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(|l| l.parse().unwrap())
        .collect()
}

async fn gone(pids: &[i32]) -> bool {
    let end = Instant::now() + Duration::from_secs(5);
    while pids.iter().any(|&p| alive(p)) && Instant::now() < end {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    !pids.iter().any(|&p| alive(p))
}

fn approval_request(events: &[Event]) -> Option<(String, String, String, String)> {
    events.iter().find_map(|e| match e {
        Event::McpServerApprovalRequested {
            request_id,
            server,
            source,
            launch,
        } => Some((
            request_id.clone(),
            server.clone(),
            source.clone(),
            launch.clone(),
        )),
        _ => None,
    })
}

// ---------------------------------------------------------------------------------------
// Approvals

#[tokio::test]
async fn repo_server_not_spawned_without_approval() {
    let fx = Fx::new();
    let pid_file = fx.path("pids");
    fx.project_server("echo", &[("ECHO_PID_FILE", &pid_file.to_string_lossy())]);
    let (cfg, _) = fx.loaded(Mode::Ask);
    assert!(cfg.mcp.servers.is_empty());
    assert!(cfg.project_mcp_servers.contains_key("echo"));
    // The key as planned: the pid file, which a variable's value names inside the root,
    // does not exist yet (once the server writes it, the key changes: review S3-M3).
    let k = key(&fx.root(), "echo", &cfg.project_mcp_servers["echo"]);
    let mock = MockProvider::new(vec![]);
    let mut run = start(EngineBuilder {
        interactive: true,
        ..fx.waiting_builder(cfg.clone(), &mock)
    });
    let events = run
        .until(|e| matches!(e, Event::McpServerApprovalRequested { .. }))
        .await;
    let (request_id, server, source, launch) = approval_request(&events).unwrap();
    assert_eq!((server.as_str(), source.as_str()), ("echo", "project"));
    assert!(
        launch.contains(&fixture()) && launch.contains("ECHO_PID_FILE"),
        "{launch}"
    );
    // A repository server's values are shown: they decide what runs.
    assert!(
        launch.contains(&format!("ECHO_PID_FILE={}", pid_file.to_string_lossy())),
        "{launch}"
    );
    // The status may come before or after the request.
    let awaiting = |e: &Event| matches!(e, Event::McpServerStatus { server, state: McpState::AwaitingApproval, .. } if server == "echo");
    if !events.iter().any(awaiting) {
        run.until(awaiting).await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!pid_file.exists(), "the server ran before the approval");

    run.send(Command::Approve {
        request_id,
        session_rule: None,
    })
    .await;
    run.until_state("echo", McpState::Ready).await;
    let started = pids(&pid_file);
    assert_eq!(started.len(), 2);
    assert!(fx.approvals().is_approved(&k));

    // Shutdown kills the server and its process group.
    run.shutdown().await;
    assert!(
        gone(&started).await,
        "{started:?} still running after shutdown"
    );
}

fn records(path: &Path) -> Vec<SessionRecord> {
    let reader = SessionReader::<SessionRecord>::open(path).unwrap();
    reader.records().map(|r| r.unwrap().1).collect()
}

#[tokio::test]
async fn repo_server_approval_request_and_answer_are_logged() {
    let fx = Fx::new();
    fx.project_server("echo", &[]);
    let (cfg, _) = fx.loaded(Mode::Ask);
    let mock = MockProvider::new(vec![]);
    let mut run = start(EngineBuilder {
        interactive: true,
        ..fx.waiting_builder(cfg, &mock)
    });
    let events = run
        .until(|e| matches!(e, Event::McpServerApprovalRequested { .. }))
        .await;
    let (request_id, ..) = approval_request(&events).unwrap();
    // Logged durably before the event was shown (H8).
    let path = run.engine.session_path().to_path_buf();
    let logged = records(&path);
    let asked = logged.iter().position(|r| {
        matches!(
            r,
            SessionRecord::ApprovalRequested {
                request_id: id,
                kind: ApprovalKind::McpServer,
                call_id: None,
                reason,
            } if *id == request_id && reason.contains("`echo`") && reason.contains("project")
        )
    });
    assert!(asked.is_some(), "{logged:?}");

    run.send(Command::Approve {
        request_id: request_id.clone(),
        session_rule: None,
    })
    .await;
    run.until_state("echo", McpState::Ready).await;
    run.shutdown().await;
    let logged = records(&path);
    let answered = logged.iter().position(|r| {
        *r == SessionRecord::Command(Command::Approve {
            request_id: request_id.clone(),
            session_rule: None,
        })
    });
    assert!(answered > asked, "{logged:?}");
}

#[tokio::test]
async fn approval_remembered_by_root_and_launch_hash() {
    let fx = Fx::new();
    let calls = fx.path("calls");
    fx.project_server("echo", &[("ECHO_CALLS", &calls.to_string_lossy())]);
    let (cfg, _) = fx.loaded(Mode::Ask);
    let k = key(&fx.root(), "echo", &cfg.project_mcp_servers["echo"]);
    fx.approvals().approve(&k, &fx.root(), "echo").unwrap();

    // Headless, no flag: an approved server starts without asking.
    let mock = MockProvider::new(vec![]);
    let mut run = start(fx.waiting_builder(cfg, &mock));
    let events = run.until_state("echo", McpState::Ready).await;
    assert!(approval_request(&events).is_none());
    run.shutdown().await;
}

#[tokio::test]
async fn changed_command_asks_again() {
    let fx = Fx::new();
    fx.project_server("echo", &[("A", "1")]);
    let (cfg, _) = fx.loaded(Mode::Ask);
    let k = key(&fx.root(), "echo", &cfg.project_mcp_servers["echo"]);
    fx.approvals().approve(&k, &fx.root(), "echo").unwrap();

    // The repository now adds a variable: the launch changed, so it asks again.
    fx.project_server(
        "echo",
        &[("A", "1"), ("NODE_OPTIONS", "--require ./evil.js")],
    );
    let (cfg, _) = fx.loaded(Mode::Ask);
    let mock = MockProvider::new(vec![]);
    let mut run = start(EngineBuilder {
        interactive: true,
        ..fx.waiting_builder(cfg, &mock)
    });
    let events = run
        .until(|e| matches!(e, Event::McpServerApprovalRequested { .. }))
        .await;
    assert!(approval_request(&events).is_some());
    run.shutdown().await;
}

#[tokio::test]
async fn headless_repo_server_skipped_without_flag_started_with_approve_mcp_and_not_persisted() {
    let fx = Fx::new();
    let pid_file = fx.path("pids");
    fx.project_server("echo", &[("ECHO_PID_FILE", &pid_file.to_string_lossy())]);
    let (cfg, _) = fx.loaded(Mode::Ask);

    let mock = MockProvider::new(vec![text_step_no_usage("one")]);
    let mut run = start(fx.waiting_builder(cfg.clone(), &mock));
    let events = run.until_state("echo", McpState::Skipped).await;
    assert!(approval_request(&events).is_none());
    let skipped = events.iter().find_map(|e| match e {
        Event::McpServerStatus { message, .. } => message.clone(),
        _ => None,
    });
    assert!(skipped.unwrap_or_default().contains("--approve-mcp"));
    run.turn_with("hi").await;
    assert!(mcp_names(&mock.requests()[0]).is_empty());
    assert!(!pid_file.exists());
    run.shutdown().await;

    let mock = MockProvider::new(vec![text_step_no_usage("two")]);
    let mut run = start(EngineBuilder {
        approve_mcp: true,
        ..fx.waiting_builder(cfg.clone(), &mock)
    });
    run.until_state("echo", McpState::Ready).await;
    run.turn_with("hi").await;
    assert!(mcp_names(&mock.requests()[0]).contains(&"mcp__echo__echo".to_string()));
    run.shutdown().await;
    let k = key(&fx.root(), "echo", &cfg.project_mcp_servers["echo"]);
    assert!(
        !fx.approvals().is_approved(&k),
        "--approve-mcp must not persist"
    );
    assert!(!fx.home.path().join(APPROVALS_FILE).exists());
}

#[tokio::test]
async fn user_server_shadows_repo_server_with_warning() {
    let fx = Fx::new();
    let repo_pids = fx.path("repo-pids");
    fx.project_server("echo", &[("ECHO_PID_FILE", &repo_pids.to_string_lossy())]);
    let (mut cfg, _) = fx.loaded(Mode::Ask);
    cfg.mcp.servers.insert("echo".into(), stdio(&[]));
    let mock = MockProvider::new(vec![]);
    let mut run = start(EngineBuilder {
        interactive: true,
        ..fx.waiting_builder(cfg, &mock)
    });
    let events = run.until_state("echo", McpState::Ready).await;
    assert!(approval_request(&events).is_none());
    assert!(
        events.iter().any(|e| matches!(e, Event::Warning { message }
            if message.contains("echo") && message.contains("project") && message.contains("user"))),
        "{events:?}"
    );
    assert!(!repo_pids.exists());
    run.shutdown().await;
}

#[test]
fn project_layer_mcp_keys_other_than_servers_dropped() {
    let fx = Fx::new();
    fx.write(
        ".tautly/config.toml",
        r#"
[mcp]
enabled = false
timeout_s = 99999
import_mcp_json = false
[mcp.servers.good]
command = "srv"
args = ["--x"]
[mcp.servers.bad]
args = ["no command"]
"#,
    );
    let user = fx.path("user.toml");
    std::fs::write(
        &user,
        "[mcp]\ntimeout_s = 30\n[mcp.servers.mine]\ncommand = \"mine\"\n",
    )
    .unwrap();
    let loaded = Config::load_from(Some(&user), &fx.root(), Default::default()).unwrap();
    let cfg = loaded.config;
    assert!(cfg.mcp.enabled && cfg.mcp.import_mcp_json);
    assert_eq!(cfg.mcp.timeout_s, 30);
    assert_eq!(cfg.mcp.servers.keys().collect::<Vec<_>>(), vec!["mine"]);
    assert_eq!(
        cfg.project_mcp_servers.keys().collect::<Vec<_>>(),
        vec!["good"]
    );
    for key in [
        "mcp.enabled",
        "mcp.timeout_s",
        "mcp.import_mcp_json",
        "mcp.servers.bad",
    ] {
        assert!(
            loaded.warnings.iter().any(|w| w.contains(key)),
            "{key}: {:?}",
            loaded.warnings
        );
    }
    // A project server never lands among the user's, even with the same name.
    fx.write(
        ".tautly/config.toml",
        "[mcp.servers.mine]\ncommand = \"theirs\"\n",
    );
    let cfg = Config::load_from(Some(&user), &fx.root(), Default::default())
        .unwrap()
        .config;
    assert!(
        matches!(&cfg.mcp.servers["mine"], ServerSpec::Stdio { command, .. } if command == "mine")
    );
    assert!(
        matches!(&cfg.project_mcp_servers["mine"], ServerSpec::Stdio { command, .. } if command == "theirs")
    );
}

#[tokio::test]
async fn deny_lasts_for_the_session() {
    let fx = Fx::new();
    let pid_file = fx.path("pids");
    fx.project_server("echo", &[("ECHO_PID_FILE", &pid_file.to_string_lossy())]);
    let (cfg, _) = fx.loaded(Mode::Ask);
    let mock = MockProvider::new(vec![text_step_no_usage("a"), text_step_no_usage("b")]);
    let mut run = start(EngineBuilder {
        interactive: true,
        ..fx.waiting_builder(cfg.clone(), &mock)
    });
    let events = run
        .until(|e| matches!(e, Event::McpServerApprovalRequested { .. }))
        .await;
    let (request_id, ..) = approval_request(&events).unwrap();
    run.send(Command::Deny {
        request_id,
        reason: None,
    })
    .await;
    run.until_state("echo", McpState::Skipped).await;
    for text in ["one", "two"] {
        let turn = run.turn_with(text).await;
        assert!(approval_request(&turn).is_none());
    }
    assert!(mock.requests().iter().all(|r| mcp_names(r).is_empty()));
    assert!(!pid_file.exists());
    run.shutdown().await;
    let k = key(&fx.root(), "echo", &cfg.project_mcp_servers["echo"]);
    assert!(!fx.approvals().is_approved(&k));

    // A denial is not remembered: the next session asks again.
    let mock = MockProvider::new(vec![]);
    let mut run = start(EngineBuilder {
        interactive: true,
        ..fx.waiting_builder(cfg, &mock)
    });
    let events = run
        .until(|e| matches!(e, Event::McpServerApprovalRequested { .. }))
        .await;
    assert!(approval_request(&events).is_some());
    run.shutdown().await;
}

#[tokio::test]
async fn mcp_json_ignored_when_import_mcp_json_false() {
    let fx = Fx::new();
    fx.write(
        ".mcp.json",
        &json!({"mcpServers": {"fromjson": {"command": fixture(), "args": ["stdio"]}}}).to_string(),
    );
    let (mut cfg, _) = fx.loaded(Mode::Ask);
    cfg.mcp.import_mcp_json = false;
    cfg.mcp.servers.insert("echo".into(), stdio(&[]));
    let mock = MockProvider::new(vec![]);
    let mut run = start(EngineBuilder {
        interactive: true,
        ..fx.waiting_builder(cfg.clone(), &mock)
    });
    let events = run.until_state("echo", McpState::Ready).await;
    assert!(approval_request(&events).is_none());
    tokio::time::sleep(Duration::from_millis(300)).await;
    let rest = run.shutdown().await;
    assert!(
        !events
            .iter()
            .chain(&rest)
            .any(|e| matches!(e, Event::McpServerStatus { server, .. } if server == "fromjson")),
        "{events:?} {rest:?}"
    );

    // On (the default), it is a repository server that needs approval.
    cfg.mcp.import_mcp_json = true;
    let mock = MockProvider::new(vec![]);
    let mut run = start(EngineBuilder {
        interactive: true,
        ..fx.waiting_builder(cfg, &mock)
    });
    let events = run
        .until(|e| matches!(e, Event::McpServerApprovalRequested { .. }))
        .await;
    let (_, server, source, _) = approval_request(&events).unwrap();
    assert_eq!((server.as_str(), source.as_str()), ("fromjson", "mcp_json"));
    run.shutdown().await;
}

// ---------------------------------------------------------------------------------------
// The tool set and calls

fn with_user_server(mode: Mode, env: &[(&str, &str)]) -> Config {
    let mut cfg = config(mode);
    cfg.mcp.servers.insert("echo".into(), stdio(env));
    cfg
}

#[tokio::test]
async fn mcp_tools_sorted_by_name_in_request() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![text_step_no_usage("done")]);
    let mut run = start(fx.waiting_builder(with_user_server(Mode::Ask, &[]), &mock));
    run.turn_with("hi").await;
    let req = &mock.requests()[0];
    let names = tool_names(req);
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted);
    assert!(names.contains(&"mcp__echo__echo".to_string()));
    assert!(names.contains(&"read".to_string()));
    assert_eq!(mcp_names(req).len(), ECHO_TOOL_COUNT);
    run.shutdown().await;
}

#[tokio::test]
async fn mcp_call_asks_in_ask_and_auto_edit_allowed_in_yolo() {
    for mode in [Mode::Ask, Mode::AutoEdit] {
        let fx = Fx::new();
        let mock = MockProvider::new(vec![
            tools_step(vec![call("c1", "mcp__echo__echo", json!({"text": "hi"}))]),
            text_step_no_usage("done"),
        ]);
        let mut run = start(EngineBuilder {
            interactive: true,
            ..fx.waiting_builder(with_user_server(mode, &[]), &mock)
        });
        run.send(Command::SendMessage {
            id: None,
            text: "go".into(),
        })
        .await;
        let events = run
            .until(|e| matches!(e, Event::ToolApprovalRequested { .. }))
            .await;
        let Some(Event::ToolApprovalRequested {
            request_id, tool, ..
        }) = events.last()
        else {
            unreachable!()
        };
        assert_eq!(tool, "mcp__echo__echo");
        run.send(Command::Approve {
            request_id: request_id.clone(),
            session_rule: None,
        })
        .await;
        let events = run.until(|e| matches!(e, Event::TurnFinished { .. })).await;
        assert!(events.iter().any(|e| matches!(e, Event::ToolFinished { output, is_error: false, .. } if output == "hi")), "{mode:?}: {events:?}");
        run.shutdown().await;
    }

    let fx = Fx::new();
    let mock = MockProvider::new(vec![
        tools_step(vec![call("c1", "mcp__echo__echo", json!({"text": "yolo"}))]),
        text_step_no_usage("done"),
    ]);
    let mut run = start(EngineBuilder {
        interactive: true,
        ..fx.waiting_builder(with_user_server(Mode::Yolo, &[]), &mock)
    });
    let events = run.turn_with("go").await;
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, Event::ToolApprovalRequested { .. }))
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::ToolFinished { output, .. } if output == "yolo"))
    );
    run.shutdown().await;
}

#[tokio::test]
async fn mcp_rule_by_name_glob_allows() {
    let fx = Fx::new();
    let mut cfg = with_user_server(Mode::Ask, &[]);
    cfg.permissions.allow = vec!["mcp__echo__*".into()];
    cfg.permissions.deny = vec!["mcp__echo__fail".into()];
    let mock = MockProvider::new(vec![
        tools_step(vec![
            call("c1", "mcp__echo__echo", json!({"text": "allowed"})),
            call("c2", "mcp__echo__fail", json!({})),
        ]),
        text_step_no_usage("done"),
    ]);
    // Headless: anything that asked would be denied.
    let mut run = start(fx.waiting_builder(cfg, &mock));
    let events = run.turn_with("go").await;
    let finished: Vec<(String, bool)> = events
        .iter()
        .filter_map(|e| match e {
            Event::ToolFinished {
                output, is_error, ..
            } => Some((output.clone(), *is_error)),
            _ => None,
        })
        .collect();
    assert_eq!(finished[0], ("allowed".to_string(), false));
    assert!(
        finished[1].1 && finished[1].0.contains("mcp__echo__fail"),
        "{finished:?}"
    );
    run.shutdown().await;
}

#[tokio::test]
async fn first_request_waits_up_to_startup_timeout() {
    // Ready within the wait: the first request has the tools.
    let fx = Fx::new();
    let mut cfg = with_user_server(Mode::Ask, &[("ECHO_START_DELAY_MS", "700")]);
    cfg.mcp.startup_timeout_ms = 60_000;
    let mock = MockProvider::new(vec![text_step_no_usage("done")]);
    let mut run = start(fx.waiting_builder(cfg, &mock));
    run.turn_with("hi").await;
    assert_eq!(mcp_names(&mock.requests()[0]).len(), ECHO_TOOL_COUNT);
    run.shutdown().await;

    // Not ready: the first request goes without them, after about the wait. The server
    // starts a minute late, so a turn on a loaded runner still ends long before it would.
    let fx = Fx::new();
    let mut cfg = with_user_server(Mode::Ask, &[("ECHO_START_DELAY_MS", "60000")]);
    cfg.mcp.startup_timeout_ms = 300;
    let mock = MockProvider::new(vec![text_step_no_usage("done")]);
    let mut run = start(fx.waiting_builder(cfg, &mock));
    let t = Instant::now();
    run.turn_with("hi").await;
    assert!(t.elapsed() < Duration::from_secs(20), "{:?}", t.elapsed());
    assert!(
        t.elapsed() >= Duration::from_millis(250),
        "{:?}",
        t.elapsed()
    );
    assert!(mcp_names(&mock.requests()[0]).is_empty());
    run.shutdown().await;
}

#[tokio::test]
async fn late_server_joins_at_next_user_turn_once() {
    let fx = Fx::new();
    // The server answers once the test opens the gate, after the first turn: a fixed delay
    // raced the first turn's start-up on a loaded runner, so the server could be ready
    // before the first request (and was then rightly offered in it).
    let gate = fx.home.path().join("echo-gate");
    let mut cfg = with_user_server(Mode::Yolo, &[("ECHO_START_GATE", gate.to_str().unwrap())]);
    cfg.mcp.startup_timeout_ms = 100;
    let state = |n: &str| call(n, "state", json!({"decisions_add": [n]}));
    let mock = MockProvider::new(vec![
        tools_step(vec![state("a")]),
        text_step_no_usage("turn 1"),
        tools_step(vec![state("b")]),
        tools_step(vec![state("c")]),
        text_step_no_usage("turn 2"),
    ]);
    // No wait for the repo map: on a loaded runner the index can take longer than an
    // event wait, and nothing here depends on it.
    let mut run = start(EngineBuilder {
        startup_wait: Duration::ZERO,
        ..fx.waiting_builder(cfg, &mock)
    });
    run.turn_with("one").await;
    let reqs = mock.requests();
    assert!(mcp_names(&reqs[0]).is_empty() && mcp_names(&reqs[1]).is_empty());
    std::fs::write(&gate, "").unwrap();
    run.until_state("echo", McpState::Ready).await;
    let events = run.turn_with("two").await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::Warning { message } if message.contains("MCP"))),
        "the cache break is announced: {events:?}"
    );
    let reqs = mock.requests();
    assert_eq!(reqs.len(), 5);
    assert_eq!(mcp_names(&reqs[2]).len(), ECHO_TOOL_COUNT);
    for later in &reqs[3..] {
        assert_eq!(later.tools, reqs[2].tools);
        assert_eq!(later.system, reqs[2].system);
    }
    run.shutdown().await;
}

#[tokio::test]
async fn list_changed_applies_at_next_user_turn() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![
        tools_step(vec![call("c1", "mcp__echo__change_tools", json!({}))]),
        tools_step(vec![call("c2", "state", json!({"decisions_add": ["x"]}))]),
        text_step_no_usage("turn 1"),
        text_step_no_usage("turn 2"),
    ]);
    let mut run = start(fx.waiting_builder(with_user_server(Mode::Yolo, &[]), &mock));
    run.turn_with("one").await;
    // Give the notification time to arrive and the list to be fetched again.
    tokio::time::sleep(Duration::from_millis(800)).await;
    run.turn_with("two").await;
    let reqs = mock.requests();
    let extra = "mcp__echo__extra".to_string();
    for r in &reqs[..3] {
        assert!(!mcp_names(r).contains(&extra));
        assert_eq!(r.tools, reqs[0].tools);
    }
    assert!(
        mcp_names(&reqs[3]).contains(&extra),
        "{:?}",
        mcp_names(&reqs[3])
    );
    run.shutdown().await;
}

/// The fixture's call log without its `tools/list` lines.
fn tool_calls(path: &Path) -> String {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter(|l| *l != "tools/list")
        .map(|l| format!("{l}\n"))
        .collect()
}

struct Recorder {
    calls_file: PathBuf,
    seen: Mutex<Vec<(String, String)>>,
}

#[async_trait]
impl PreExecHook for Recorder {
    async fn before_exec(&self, tool: &str, _turn: u32) {
        let calls = tool_calls(&self.calls_file);
        self.seen.lock().unwrap().push((tool.to_string(), calls));
    }
}

#[tokio::test]
async fn checkpoint_taken_before_stdio_mcp_call() {
    let fx = Fx::new();
    let calls = fx.path("calls");
    fx.write("a.txt", "hello\n");
    let hook = Arc::new(Recorder {
        calls_file: calls.clone(),
        seen: Mutex::new(Vec::new()),
    });
    let mock = MockProvider::new(vec![
        tools_step(vec![
            call("c1", "read", json!({"path": "a.txt"})),
            call("c2", "mcp__echo__echo", json!({"text": "x"})),
        ]),
        text_step_no_usage("done"),
    ]);
    let mut run = start(EngineBuilder {
        pre_exec: Some(hook.clone()),
        ..fx.waiting_builder(
            with_user_server(Mode::Yolo, &[("ECHO_CALLS", &calls.to_string_lossy())]),
            &mock,
        )
    });
    run.turn_with("go").await;
    let seen = hook.seen.lock().unwrap().clone();
    // Once, for the MCP call only, before the server saw it.
    assert_eq!(seen, vec![("mcp__echo__echo".to_string(), String::new())]);
    assert_eq!(tool_calls(&calls), "echo\n");
    run.shutdown().await;
}

#[tokio::test]
async fn identical_failed_mcp_call_blocked() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![
        tools_step(vec![call("c1", "mcp__echo__fail", json!({"why": 1}))]),
        tools_step(vec![call("c2", "mcp__echo__fail", json!({"why": 1}))]),
        text_step_no_usage("done"),
    ]);
    let mut run = start(fx.waiting_builder(with_user_server(Mode::Yolo, &[]), &mock));
    let events = run.turn_with("go").await;
    let blocked: Vec<bool> = events
        .iter()
        .filter_map(|e| match e {
            Event::ToolFinished { blocked, .. } => Some(*blocked),
            _ => None,
        })
        .collect();
    assert_eq!(blocked, vec![false, true]);
    run.shutdown().await;
}

#[tokio::test]
async fn client_servers_are_trusted() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![text_step_no_usage("done")]);
    let mut run = start(EngineBuilder {
        interactive: true,
        client_mcp: vec![("editor".into(), stdio(&[]))],
        ..fx.waiting_builder(config(Mode::Ask), &mock)
    });
    let events = run.until_state("editor", McpState::Ready).await;
    assert!(approval_request(&events).is_none());
    run.turn_with("hi").await;
    assert!(mcp_names(&mock.requests()[0]).contains(&"mcp__editor__echo".to_string()));
    run.shutdown().await;
}

/// What `tautly mcp list` and `doctor` show (the CLI parts follow with Tasks 29/40).
#[tokio::test]
async fn plan_lists_servers_with_source_and_approval() {
    use tautly_core::{Approval, ServerSource, plan_servers};
    let fx = Fx::new();
    fx.write(
        ".tautly/config.toml",
        &format!(
            "[mcp.servers.repo]\ncommand = {0}\n[mcp.servers.pending]\ncommand = {0}\nargs = [\"x\"]\n",
            toml_str(&fixture())
        ),
    );
    fx.write(
        ".mcp.json",
        &json!({"mcpServers": {"json": {"command": "srv"}, "repo": {"command": "other"}}})
            .to_string(),
    );
    let (mut cfg, _) = fx.loaded(Mode::Ask);
    cfg.mcp.servers.insert("mine".into(), stdio(&[]));
    let k = key(&fx.root(), "repo", &cfg.project_mcp_servers["repo"]);
    fx.approvals().approve(&k, &fx.root(), "repo").unwrap();
    let sb = tautly_sandbox::LocalSandbox::new(fx.root()).unwrap();
    let client = vec![("editor".to_string(), stdio(&[]))];
    let home = fx.home.path();
    let plan = plan_servers(
        &cfg.mcp,
        &cfg.project_mcp_servers,
        &client,
        false,
        home,
        &sb,
    )
    .await;
    let got: Vec<(&str, ServerSource, Approval)> = plan
        .servers
        .iter()
        .map(|s| (s.name.as_str(), s.source, s.approval))
        .collect();
    assert_eq!(
        got,
        vec![
            ("editor", ServerSource::Client, Approval::Trusted),
            ("json", ServerSource::McpJson, Approval::Pending),
            ("mine", ServerSource::User, Approval::Trusted),
            ("pending", ServerSource::Project, Approval::Pending),
            ("repo", ServerSource::Project, Approval::Remembered),
        ]
    );
    assert!(
        plan.warnings
            .iter()
            .any(|w| w.contains("`repo` of .mcp.json")),
        "{:?}",
        plan.warnings
    );

    let plan = plan_servers(&cfg.mcp, &cfg.project_mcp_servers, &[], true, home, &sb).await;
    let pending = plan.servers.iter().find(|s| s.name == "pending").unwrap();
    assert_eq!(pending.approval, Approval::ThisRun);

    // A repository cannot flood the user with approval requests.
    let many: std::collections::BTreeMap<String, ServerSpec> =
        (0..40).map(|i| (format!("s{i:02}"), stdio(&[]))).collect();
    let plan = plan_servers(&cfg.mcp, &many, &[], false, home, &sb).await;
    let repo = plan
        .servers
        .iter()
        .filter(|s| s.source.is_repository())
        .count();
    assert_eq!(repo, 32);
    // 40 in the project config, 2 in `.mcp.json`.
    assert!(
        plan.warnings.iter().any(|w| w.contains("42 MCP servers")),
        "{:?}",
        plan.warnings
    );

    cfg.mcp.enabled = false;
    let plan = plan_servers(&cfg.mcp, &cfg.project_mcp_servers, &[], true, home, &sb).await;
    assert!(plan.servers.is_empty());
}

#[tokio::test]
async fn changed_env_value_asks_again() {
    let fx = Fx::new();
    fx.project_server("echo", &[("NODE_OPTIONS", "")]);
    let (cfg, _) = fx.loaded(Mode::Ask);
    let k = key(&fx.root(), "echo", &cfg.project_mcp_servers["echo"]);
    fx.approvals().approve(&k, &fx.root(), "echo").unwrap();

    // Same variable, new value: asks again, and the prompt shows the value.
    fx.project_server("echo", &[("NODE_OPTIONS", "--require ./evil.js")]);
    let (cfg, _) = fx.loaded(Mode::Ask);
    let mock = MockProvider::new(vec![]);
    let mut run = start(EngineBuilder {
        interactive: true,
        ..fx.waiting_builder(cfg, &mock)
    });
    let events = run
        .until(|e| matches!(e, Event::McpServerApprovalRequested { .. }))
        .await;
    let (_, _, _, launch) = approval_request(&events).unwrap();
    assert!(
        launch.contains("NODE_OPTIONS=--require ./evil.js"),
        "{launch}"
    );
    run.shutdown().await;
}

/// The `PYTHONPYCACHEPREFIX` a server saw, from the environment it dumped to `seen`
/// (the command line must not name the variable: such a server is never remembered).
fn pycache_prefix_seen(seen: &Path) -> String {
    let dump = std::fs::read_to_string(seen).unwrap();
    dump.lines()
        .find_map(|l| l.strip_prefix("PYTHONPYCACHEPREFIX="))
        .unwrap_or_else(|| panic!("no PYTHONPYCACHEPREFIX in {dump}"))
        .to_string()
}

/// Review S2-I1: every stdio server runs with `PYTHONPYCACHEPREFIX` set to a private
/// directory in the data home (0700), so a Python server never loads a bytecode file the
/// repository put in a `__pycache__` the approval key does not cover. A server that sets
/// it itself is overridden, with a warning.
#[tokio::test]
async fn stdio_servers_get_a_private_pycache_prefix() {
    use std::os::unix::fs::PermissionsExt;
    let fx = Fx::new();
    let seen = fx.path("seen.txt");
    fx.write(
        ".tautly/config.toml",
        &format!(
            "[mcp.servers.echo]\ncommand = \"/bin/sh\"\nargs = [\"-c\", {}]\n\
             env = {{ PYTHONPYCACHEPREFIX = \"repo-cache\" }}\n",
            toml_str(&format!(
                "env > '{}'; exec '{}' stdio",
                seen.display(),
                fixture()
            ))
        ),
    );
    let (cfg, _) = fx.loaded(Mode::Ask);
    let k = key(&fx.root(), "echo", &cfg.project_mcp_servers["echo"]);
    fx.approvals().approve(&k, &fx.root(), "echo").unwrap();
    let mock = MockProvider::new(vec![]);
    let mut run = start(fx.waiting_builder(cfg, &mock));
    let events = run.until_state("echo", McpState::Ready).await;
    run.shutdown().await;
    let warned: Vec<&String> = events
        .iter()
        .filter_map(|e| match e {
            Event::Warning { message } if message.contains("PYTHONPYCACHEPREFIX") => Some(message),
            _ => None,
        })
        .collect();
    assert_eq!(warned.len(), 1, "{events:?}");
    let private = fx.home.path().join("mcp-pycache");
    assert_eq!(pycache_prefix_seen(&seen), private.to_str().unwrap());
    let mode = std::fs::metadata(&private).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o700, "{mode:o}");
}

/// Review S3-M2: the private bytecode directory must be a real directory of the current
/// user, mode 0700, outside the root. Here a link stands at its path: it is not followed
/// (its target keeps its mode), a warning says so, and the server gets a fresh 0700
/// directory for this run instead.
#[tokio::test]
async fn a_bad_private_pycache_dir_is_replaced_for_the_run() {
    use std::os::unix::fs::PermissionsExt;
    let fx = Fx::new();
    let seen = fx.path("seen.txt");
    let target = fx.path("elsewhere");
    std::fs::create_dir_all(&target).unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
    let private = fx.home.path().join("mcp-pycache");
    std::os::unix::fs::symlink(&target, &private).unwrap();
    fx.write(
        ".tautly/config.toml",
        &format!(
            "[mcp.servers.echo]\ncommand = \"/bin/sh\"\nargs = [\"-c\", {}]\n",
            toml_str(&format!(
                "env > '{}'; exec '{}' stdio",
                seen.display(),
                fixture()
            ))
        ),
    );
    let (cfg, _) = fx.loaded(Mode::Ask);
    let k = key(&fx.root(), "echo", &cfg.project_mcp_servers["echo"]);
    fx.approvals().approve(&k, &fx.root(), "echo").unwrap();
    let mock = MockProvider::new(vec![]);
    let mut run = start(fx.waiting_builder(cfg, &mock));
    let events = run.until_state("echo", McpState::Ready).await;
    let got = PathBuf::from(pycache_prefix_seen(&seen));
    let warned: Vec<&String> = events
        .iter()
        .filter_map(|e| match e {
            Event::Warning { message } if message.contains("bytecode cache") => Some(message),
            _ => None,
        })
        .collect();
    assert_eq!(warned.len(), 1, "{events:?}");
    assert_ne!(got, private, "{got:?}");
    assert_ne!(got, target.canonicalize().unwrap(), "{got:?}");
    let meta = got.symlink_metadata().unwrap();
    assert!(meta.is_dir(), "{got:?}");
    assert_eq!(meta.permissions().mode() & 0o777, 0o700);
    assert!(!got.starts_with(fx.root()), "{got:?}");
    // The link's target was not touched.
    let mode = std::fs::metadata(&target).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o755, "{mode:o}");
    assert!(private.symlink_metadata().unwrap().file_type().is_symlink());
    run.shutdown().await;
    // The replacement lasts for the run only.
    assert!(!got.exists(), "{got:?}");
}

/// Review N9: a server whose command line names a file that cannot be hashed (here over
/// the 1 MiB limit) can never match a remembered approval, so approving it is for this run
/// only: the prompt says so and nothing is written to the approvals file.
#[tokio::test]
async fn unhashable_server_is_approved_for_this_run_only() {
    let fx = Fx::new();
    fx.write(
        "scripts/mcp.sh",
        &format!("#!/bin/sh\nexec '{}' stdio\n", fixture()),
    );
    fx.write("big.dat", &"x".repeat(1024 * 1024 + 1));
    fx.write(
        ".tautly/config.toml",
        "[mcp.servers.echo]\ncommand = \"/bin/sh\"\nargs = [\"scripts/mcp.sh\", \"big.dat\"]\n",
    );
    let (cfg, _) = fx.loaded(Mode::Ask);
    let mock = MockProvider::new(vec![]);
    let mut run = start(EngineBuilder {
        interactive: true,
        ..fx.waiting_builder(cfg, &mock)
    });
    let events = run
        .until(|e| matches!(e, Event::McpServerApprovalRequested { .. }))
        .await;
    let (request_id, _, _, launch) = approval_request(&events).unwrap();
    assert!(launch.contains("this run only"), "{launch}");
    run.send(Command::Approve {
        request_id,
        session_rule: None,
    })
    .await;
    run.until_state("echo", McpState::Ready).await;
    run.shutdown().await;
    let file = fx.home.path().join(APPROVALS_FILE);
    let saved = std::fs::read_to_string(&file).unwrap_or_default();
    assert!(!saved.contains("\"echo\""), "{saved}");
}

/// Review B-I1: the approval covers the script the command line names. The same command
/// line with the script rewritten (a PR branch) asks again instead of starting.
#[tokio::test]
async fn rewritten_server_script_asks_again() {
    let fx = Fx::new();
    let script = |extra: &str| format!("#!/bin/sh\n{extra}exec '{}' stdio\n", fixture());
    fx.write("scripts/mcp.sh", &script(""));
    fx.write(
        ".tautly/config.toml",
        "[mcp.servers.echo]\ncommand = \"/bin/sh\"\nargs = [\"scripts/mcp.sh\"]\n",
    );
    let (cfg, _) = fx.loaded(Mode::Ask);
    let k = key(&fx.root(), "echo", &cfg.project_mcp_servers["echo"]);
    fx.approvals().approve(&k, &fx.root(), "echo").unwrap();

    // Unchanged: remembered, started without asking.
    let mock = MockProvider::new(vec![]);
    let mut run = start(fx.waiting_builder(cfg.clone(), &mock));
    let events = run.until_state("echo", McpState::Ready).await;
    assert!(approval_request(&events).is_none());
    run.shutdown().await;

    // Rewritten: asks again, and nothing starts meanwhile.
    let marker = fx.path("ran");
    fx.write(
        "scripts/mcp.sh",
        &script(&format!("touch '{}'\n", marker.display())),
    );
    let mock = MockProvider::new(vec![]);
    let mut run = start(EngineBuilder {
        interactive: true,
        ..fx.waiting_builder(cfg, &mock)
    });
    let events = run
        .until(|e| matches!(e, Event::McpServerApprovalRequested { .. }))
        .await;
    assert!(approval_request(&events).is_some());
    run.shutdown().await;
    assert!(
        !marker.exists(),
        "the rewritten script ran without approval"
    );
}

#[tokio::test]
async fn repository_server_names_cannot_borrow_user_rules() {
    let fx = Fx::new();
    let server = |name: &str| {
        format!(
            "[mcp.servers.\"{name}\"]\ncommand = {}\nargs = [\"stdio\"]\n",
            toml_str(&fixture())
        )
    };
    fx.write(
        ".tautly/config.toml",
        &format!("{}{}{}", server("gh__x"), server("gh."), server("my_srv")),
    );
    let (mut cfg, _) = fx.loaded(Mode::Ask);
    cfg.mcp.servers.insert("gh".into(), stdio(&[]));
    // The user's `my.srv` has the tool prefix `mcp__my_srv__` too.
    cfg.mcp.servers.insert("my.srv".into(), stdio(&[]));
    cfg.permissions.allow = vec!["mcp__gh__*".into(), "mcp__my_srv__*".into()];
    let mock = MockProvider::new(vec![text_step_no_usage("done")]);
    let mut run = start(EngineBuilder {
        approve_mcp: true,
        ..fx.waiting_builder(cfg, &mock)
    });
    let ready = |events: &[Event], name: &str| {
        events.iter().any(|e| matches!(e, Event::McpServerStatus { server, state: McpState::Ready, .. } if server == name))
    };
    let mut events = Vec::new();
    while !(ready(&events, "gh") && ready(&events, "my.srv")) {
        events.push(run.next().await);
    }
    let warnings: Vec<&String> = events
        .iter()
        .filter_map(|e| match e {
            Event::Warning { message } => Some(message),
            _ => None,
        })
        .collect();
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("gh__x") && w.contains("not a valid")),
        "{warnings:?}"
    );
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("`gh.`") && w.contains("not a valid")),
        "{warnings:?}"
    );
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("my_srv") && w.contains("shadowed")),
        "{warnings:?}"
    );
    run.turn_with("hi").await;
    // Only the user's servers' tools carry their prefixes; no repository tool falls under
    // the user's rules.
    let names = mcp_names(&mock.requests()[0]);
    assert_eq!(names.len(), 2 * ECHO_TOOL_COUNT, "{names:?}");
    assert!(
        names
            .iter()
            .all(|n| !n.starts_with("mcp__gh__x__") && !n.starts_with("mcp__gh___")),
        "{names:?}"
    );
    run.shutdown().await;
}

#[tokio::test]
async fn approval_prompt_escapes_control_characters() {
    let fx = Fx::new();
    fx.write(
        ".mcp.json",
        &json!({"mcpServers": {"echo": {
            "command": fixture(),
            "args": ["\r\u{1b}[2Kstdio\u{202e}"],
            "env": {"ECHO_NOTE": "a\u{7}b"}
        }}})
        .to_string(),
    );
    let (cfg, _) = fx.loaded(Mode::Ask);
    let mock = MockProvider::new(vec![]);
    let mut run = start(EngineBuilder {
        interactive: true,
        ..fx.waiting_builder(cfg, &mock)
    });
    let events = run
        .until(|e| matches!(e, Event::McpServerApprovalRequested { .. }))
        .await;
    let (_, _, _, launch) = approval_request(&events).unwrap();
    assert!(
        !launch.chars().any(|c| c.is_control() || c == '\u{202e}'),
        "{launch:?}"
    );
    assert!(launch.contains("\\r\\u{1b}[2Kstdio\\u{202e}"), "{launch}");
    assert!(launch.contains("ECHO_NOTE=a\\u{7}b"), "{launch}");
    run.shutdown().await;
}

#[tokio::test]
async fn pending_approval_is_announced_again_each_turn() {
    let fx = Fx::new();
    fx.project_server("echo", &[]);
    let (cfg, _) = fx.loaded(Mode::Ask);
    let mock = MockProvider::new(vec![text_step_no_usage("a"), text_step_no_usage("b")]);
    let mut run = start(EngineBuilder {
        interactive: true,
        ..fx.waiting_builder(cfg, &mock)
    });
    let events = run
        .until(|e| matches!(e, Event::McpServerApprovalRequested { .. }))
        .await;
    let (first, ..) = approval_request(&events).unwrap();
    for text in ["one", "two"] {
        let turn = run.turn_with(text).await;
        let (again, ..) = approval_request(&turn).expect("re-announced at the turn start");
        assert_eq!(again, first);
    }
    run.send(Command::Approve {
        request_id: first,
        session_rule: None,
    })
    .await;
    run.until_state("echo", McpState::Ready).await;
    run.shutdown().await;
}

#[tokio::test]
async fn dead_server_tools_removed_at_next_user_turn() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![
        tools_step(vec![call("c1", "mcp__echo__exit", json!({}))]),
        text_step_no_usage("turn 1"),
        text_step_no_usage("turn 2"),
    ]);
    let mut run = start(fx.waiting_builder(with_user_server(Mode::Yolo, &[]), &mock));
    let first = run.turn_with("one").await;
    // The server may be reported dead during turn one already.
    let failed = |e: &Event| matches!(e, Event::McpServerStatus { server, state: McpState::Failed, .. } if server == "echo");
    if !first.iter().any(failed) {
        run.until(failed).await;
    }
    let events = run.turn_with("two").await;
    assert!(
        events.iter().any(
            |e| matches!(e, Event::Warning { message } if message.contains("MCP tools changed"))
        ),
        "{events:?}"
    );
    let reqs = mock.requests();
    assert_eq!(mcp_names(&reqs[0]).len(), ECHO_TOOL_COUNT);
    assert_eq!(mcp_names(&reqs[1]).len(), ECHO_TOOL_COUNT);
    assert!(mcp_names(&reqs[2]).is_empty());
    run.shutdown().await;
}

/// Review A M6 / C M1: a changed MCP tool set breaks the cached prefix at a turn start;
/// the step's epoch takes the pending pins with it, so the prefix breaks once.
#[tokio::test]
async fn a_tool_set_change_folds_pending_pins_into_the_same_prefix_break() {
    let fx = Fx::new();
    let mut cfg = with_user_server(Mode::Yolo, &[]);
    // A warm cache: without the forced epoch the pins would wait for a later one.
    cfg.catalog_overrides = Some(
        Catalog::from_toml(&format!(
            r#"[models."{MODEL}"]
context_window = 200000
max_output = 8000
parallel_tools = true
reasoning = "none"
cache = "explicit"
images = false
edit_format = "str_replace"
"#
        ))
        .unwrap(),
    );
    let mock = MockProvider::new(vec![
        tools_step(vec![call("c1", "mcp__echo__exit", json!({}))]),
        text_step_no_usage("turn 1"),
        text_step_no_usage("turn 2"),
    ]);
    let mut run = start(fx.waiting_builder(cfg, &mock));
    let events = run.turn_with("one").await;
    let failed = |e: &Event| matches!(e, Event::McpServerStatus { server, state: McpState::Failed, .. } if server == "echo");
    if !events.iter().any(failed) {
        run.until(failed).await;
    }
    run.send(Command::Pin {
        text: "PIN-AFTER-TURN-ONE".into(),
    })
    .await;
    run.turn_with("two").await;
    let path = run.engine.session_path().to_path_buf();
    run.shutdown().await;
    let recs: Vec<SessionRecord> = SessionReader::<SessionRecord>::open(&path)
        .unwrap()
        .records()
        .map(|r| r.unwrap().1)
        .collect();
    let start = recs
        .iter()
        .rposition(|r| matches!(r, SessionRecord::TurnStart { .. }))
        .unwrap();
    let before_request: Vec<&SessionRecord> = recs[start..]
        .iter()
        .take_while(|r| !matches!(r, SessionRecord::Request { .. }))
        .collect();
    assert!(
        before_request
            .iter()
            .any(|r| matches!(r, SessionRecord::PinsFolded)),
        "{before_request:?}"
    );
}

#[tokio::test]
async fn mcp_json_fifo_does_not_block_startup() {
    let fx = Fx::new();
    let fifo = std::ffi::CString::new(fx.path(".mcp.json").to_string_lossy().as_bytes()).unwrap();
    // SAFETY: a valid NUL-terminated path.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o644) }, 0);
    let mock = MockProvider::new(vec![text_step_no_usage("done")]);
    let mut run = start(fx.waiting_builder(with_user_server(Mode::Ask, &[]), &mock));
    let events = run.until_state("echo", McpState::Ready).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::Warning { message } if message.contains(".mcp.json"))),
        "{events:?}"
    );
    run.turn_with("hi").await;
    run.shutdown().await;
}

// ---------------------------------------------------------------------------------------
// Cancel during the first turn's start-up wait

#[tokio::test]
async fn cancel_during_the_mcp_startup_wait_ends_the_turn_within_200ms() {
    // Review probe P2: a server that never answers `initialize` holds the first user
    // turn for up to the start-up timeout; `Cancel` must not wait for it.
    let fx = Fx::new();
    let mut cfg = config(Mode::Yolo);
    cfg.mcp.servers.insert(
        "slow".into(),
        ServerSpec::Stdio {
            command: "/bin/sleep".into(),
            args: vec!["30".into()],
            env: Default::default(),
            cwd: None,
        },
    );
    let mock = MockProvider::new(vec![text_step_no_usage("never sent")]);
    let mut run = start(fx.waiting_builder(cfg, &mock));
    run.send(Command::SendMessage {
        id: None,
        text: "go".into(),
    })
    .await;
    run.until(|e| matches!(e, Event::TurnStarted)).await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    let t0 = Instant::now();
    run.send(Command::Cancel).await;
    let events = run.until(|e| matches!(e, Event::TurnFinished { .. })).await;
    let took = t0.elapsed();
    assert!(
        matches!(
            events.last(),
            Some(Event::TurnFinished {
                reason: tautly_core::FinishReason::Cancelled,
                ..
            })
        ),
        "{events:?}"
    );
    assert!(
        took < Duration::from_millis(250),
        "TurnFinished {took:?} after Cancel"
    );
    assert!(mock.requests().is_empty());
    run.shutdown().await;
}

// ---------------------------------------------------------------------------------------
// Resume of a session that had MCP tools

/// A text response reporting `input` tokens (the estimator calibrates on it).
fn text_with_usage(text: &str, input: u64) -> MockTurn {
    MockTurn::Events(vec![
        ProviderEvent::TextDelta(text.into()),
        ProviderEvent::Usage(tautly_provider::RawUsage {
            input,
            output: 5,
            cache_read: 0,
            cache_write: 0,
        }),
        ProviderEvent::Done {
            stop: StopReason::EndTurn,
        },
    ])
}

/// `used` of each `ContextStatus` in `events`.
fn used(events: &[Event]) -> Vec<u32> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::ContextStatus { used, .. } => Some(*used),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_resumed_mcp_session_estimates_as_the_live_one_and_does_not_warn_of_new_tools() {
    // Review C probe: the rebuild calibrated without the MCP schemas (the resumed
    // estimate ran high) and the resume warned that the tools differ, though the first
    // resumed request carries the very same tools.
    let fx = Fx::new();
    let cfg = with_user_server(Mode::Ask, &[]);

    // The live session: two turns.
    let live = MockProvider::new(vec![
        text_with_usage("one", 6000),
        text_with_usage("two", 6000),
    ]);
    let mut run = start(fx.waiting_builder(cfg.clone(), &live));
    run.turn_with("first").await;
    let live_two = run.turn_with("second").await;
    run.shutdown().await;

    // The same first turn in another session, then resumed for the second.
    let first = MockProvider::new(vec![text_with_usage("one", 6000)]);
    let mut run = start(fx.waiting_builder(cfg.clone(), &first));
    let id = run.engine.session_id().to_string();
    run.turn_with("first").await;
    run.shutdown().await;
    let resumed = MockProvider::new(vec![text_with_usage("two", 6000)]);
    let mut run = start(EngineBuilder {
        resume: Some(id),
        ..fx.waiting_builder(cfg, &resumed)
    });
    let resumed_two = run.turn_with("second").await;
    run.shutdown().await;

    assert_eq!(
        mcp_names(&resumed.requests()[0]),
        mcp_names(&live.requests()[1])
    );
    assert_eq!(mcp_names(&resumed.requests()[0]).len(), ECHO_TOOL_COUNT);
    assert_eq!(used(&resumed_two), used(&live_two));
    let differ: Vec<&Event> = resumed_two
        .iter()
        .filter(|e| matches!(e, Event::Warning { message } if message.contains("differ")))
        .collect();
    assert!(differ.is_empty(), "{differ:?}");
}

#[tokio::test]
async fn a_resumed_session_whose_mcp_tools_are_gone_warns_once_at_its_first_request() {
    let fx = Fx::new();
    let first = MockProvider::new(vec![text_with_usage("one", 6000)]);
    let mut run = start(fx.waiting_builder(with_user_server(Mode::Ask, &[]), &first));
    let id = run.engine.session_id().to_string();
    run.turn_with("first").await;
    run.shutdown().await;
    // Resumed without the server.
    let resumed = MockProvider::new(vec![
        text_with_usage("two", 6000),
        text_with_usage("three", 6000),
    ]);
    let mut run = start(EngineBuilder {
        resume: Some(id),
        ..fx.waiting_builder(config(Mode::Ask), &resumed)
    });
    let two = run.turn_with("second").await;
    let three = run.turn_with("third").await;
    run.shutdown().await;
    let differ = |events: &[Event]| {
        events
            .iter()
            .filter(|e| matches!(e, Event::Warning { message } if message.contains("tools differ")))
            .count()
    };
    assert_eq!((differ(&two), differ(&three)), (1, 0), "{two:?}");
}

/// quality Q1: approvals that cannot be read approve nothing (a remembered server asks
/// again), and an approval that cannot be saved holds for this session only, said so.
#[tokio::test]
async fn unreadable_approvals_fail_closed_and_unsaved_ones_last_one_session() {
    use tautly_core::{Approval, plan_servers};
    let fx = Fx::new();
    fx.project_server("echo", &[]);
    let (cfg, _) = fx.loaded(Mode::Ask);
    let k = key(&fx.root(), "echo", &cfg.project_mcp_servers["echo"]);
    fx.approvals().approve(&k, &fx.root(), "echo").unwrap();
    // The approvals file is replaced by a directory: it cannot be read or written.
    let file = fx.home.path().join(APPROVALS_FILE);
    std::fs::remove_file(&file).unwrap();
    std::fs::create_dir(&file).unwrap();
    let sb = tautly_sandbox::LocalSandbox::new(fx.root()).unwrap();
    let plan = plan_servers(
        &cfg.mcp,
        &cfg.project_mcp_servers,
        &[],
        false,
        fx.home.path(),
        &sb,
    )
    .await;
    let echo = plan.servers.iter().find(|s| s.name == "echo").unwrap();
    assert_eq!(echo.approval, Approval::Pending);

    let mock = MockProvider::new(vec![]);
    let mut run = start(EngineBuilder {
        interactive: true,
        ..fx.waiting_builder(cfg, &mock)
    });
    let events = run
        .until(|e| matches!(e, Event::McpServerApprovalRequested { .. }))
        .await;
    let (request_id, _, _, _) = approval_request(&events).expect("asked again");
    run.send(Command::Approve {
        request_id,
        session_rule: None,
    })
    .await;
    let events = run.until_state("echo", McpState::Ready).await;
    run.shutdown().await;
    assert!(
        events.iter().any(|e| matches!(e, Event::Warning { message }
            if message.contains("could not be saved; it is approved for this session only"))),
        "{events:?}"
    );
    assert!(file.is_dir());
}
