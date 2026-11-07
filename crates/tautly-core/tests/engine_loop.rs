//! The engine's turn loop against a scripted provider (`MockProvider`) and a real
//! `LocalSandbox` on a temporary directory.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{Value, json};
use tautly_core::{
    Command, Config, Engine, EngineBuilder, ErrorKind, Event, FinishReason, SessionRecord,
    Workspace,
};
use tautly_provider::{
    Block, CanonicalRequest, Catalog, Content, MockProvider, MockTurn, ModelId, ProviderError,
    ProviderEvent, RawUsage, StopReason,
};
use tautly_sandbox::{
    Child, FileMeta, HttpResponse, LocalSandbox, Mode, Sandbox, SandboxError, SpawnSpec, Url,
};
use tautly_session::SessionReader;
use tempfile::TempDir;
use tokio::sync::mpsc;

const MODEL: &str = "mock/test";
const WAIT: Duration = Duration::from_secs(30);

fn catalog(window: u32, max_output: u32) -> Catalog {
    Catalog::from_toml(&format!(
        r#"[models."{MODEL}"]
context_window = {window}
max_output = {max_output}
parallel_tools = true
reasoning = "none"
cache = "none"
images = false
edit_format = "str_replace"
"#
    ))
    .unwrap()
}

fn config(mode: Mode) -> Config {
    let mut c = Config {
        model: ModelId::from(MODEL),
        catalog_overrides: Some(catalog(200_000, 8_000)),
        ..Config::default()
    };
    c.permissions.mode = mode;
    c
}

fn call(id: &str, name: &str, args: Value) -> ProviderEvent {
    ProviderEvent::ToolCall {
        id: id.into(),
        name: name.into(),
        input_json: args.to_string(),
    }
}

fn usage() -> ProviderEvent {
    ProviderEvent::Usage(RawUsage {
        input: 100,
        output: 10,
        cache_read: 0,
        cache_write: 0,
    })
}

/// A response with tool calls. It reports no usage: the mock's numbers would calibrate
/// the token estimator far off.
fn tools_step(calls: Vec<ProviderEvent>) -> MockTurn {
    let mut evs = calls;
    evs.push(ProviderEvent::Done {
        stop: StopReason::ToolUse,
    });
    MockTurn::Events(evs)
}

fn text_step(text: &str) -> MockTurn {
    MockTurn::Events(vec![
        ProviderEvent::TextDelta(text.into()),
        usage(),
        ProviderEvent::Done {
            stop: StopReason::EndTurn,
        },
    ])
}

struct Fx {
    dir: TempDir,
    home: TempDir,
}

impl Fx {
    fn new() -> Fx {
        Fx {
            dir: tempfile::tempdir().unwrap(),
            home: tempfile::tempdir().unwrap(),
        }
    }

    fn root(&self) -> PathBuf {
        self.dir.path().canonicalize().unwrap()
    }

    fn write(&self, rel: &str, text: &str) {
        let p = self.dir.path().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    fn builder(&self, cfg: Config, mock: &MockProvider) -> EngineBuilder {
        EngineBuilder {
            provider: Some(Arc::new(mock.clone())),
            data_home: Some(self.home.path().to_path_buf()),
            ..EngineBuilder::new(cfg, self.dir.path())
        }
    }

    /// A workspace over this root whose index has finished its first refresh.
    async fn ready_workspace(&self) -> Arc<Workspace> {
        let sb: Arc<dyn Sandbox> = Arc::new(LocalSandbox::new(self.root()).unwrap());
        let ws = Workspace::new(sb, self.home.path());
        ws.start();
        assert!(ws.wait_ready(WAIT).await, "index never became ready");
        ws
    }
}

struct Run {
    engine: Engine,
    events: mpsc::Receiver<Event>,
}

fn start(b: EngineBuilder) -> Run {
    let (engine, events) = b.build().unwrap();
    Run { engine, events }
}

impl Run {
    async fn say(&self, text: &str) {
        self.engine
            .send(Command::SendMessage { text: text.into() })
            .await
            .unwrap();
    }

    async fn next(&mut self) -> Event {
        tokio::time::timeout(WAIT, self.events.recv())
            .await
            .expect("timed out waiting for an event")
            .expect("event channel closed")
    }

    /// Events up to and including the first that matches `pred`.
    async fn until(&mut self, pred: impl Fn(&Event) -> bool) -> Vec<Event> {
        let mut out = Vec::new();
        loop {
            let ev = self.next().await;
            let done = pred(&ev);
            out.push(ev);
            if done {
                return out;
            }
        }
    }

    /// Events through `TurnFinished`, from `TurnStarted` if it is among them (earlier
    /// events, from session start, are dropped).
    async fn turn(&mut self) -> Vec<Event> {
        let all = self
            .until(|e| matches!(e, Event::TurnFinished { .. }))
            .await;
        let from = all
            .iter()
            .position(|e| matches!(e, Event::TurnStarted))
            .unwrap_or(0);
        all[from..].to_vec()
    }

    async fn turn_with(&mut self, text: &str) -> Vec<Event> {
        self.say(text).await;
        self.turn().await
    }
}

fn kind(ev: &Event) -> String {
    serde_json::to_value(ev).unwrap()["type"]
        .as_str()
        .unwrap()
        .to_string()
}

fn finish_reason(events: &[Event]) -> FinishReason {
    match events.last() {
        Some(Event::TurnFinished { reason, .. }) => *reason,
        other => panic!("turn did not finish: {other:?}"),
    }
}

/// `(call_id, output, is_error, blocked)` of every `ToolFinished`, in order.
fn finished(events: &[Event]) -> Vec<(String, String, bool, bool)> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::ToolFinished {
                call_id,
                output,
                is_error,
                blocked,
                ..
            } => Some((call_id.clone(), output.clone(), *is_error, *blocked)),
            _ => None,
        })
        .collect()
}

fn count(events: &[Event], k: &str) -> usize {
    events.iter().filter(|e| kind(e) == k).count()
}

/// `(id, output, is_error)` of every tool result in `req`, in order.
fn tool_results(req: &CanonicalRequest) -> Vec<(String, String, bool)> {
    req.messages
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|c| match c {
            Content::ToolResult {
                id,
                output,
                is_error,
            } => Some((id.clone(), output.clone(), *is_error)),
            _ => None,
        })
        .collect()
}

/// The text of the request's last message (the state tail).
fn tail_text(req: &CanonicalRequest) -> String {
    req.messages
        .last()
        .unwrap()
        .content
        .iter()
        .filter_map(|c| match c {
            Content::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn system_block(req: &CanonicalRequest, i: usize) -> String {
    match &req.system[i] {
        Block::Text(t) => t.clone(),
        other => panic!("not text: {other:?}"),
    }
}

fn records(engine: &Engine) -> Vec<SessionRecord> {
    let reader = SessionReader::<SessionRecord>::open(engine.session_path()).unwrap();
    reader.records().map(|r| r.unwrap().1).collect()
}

#[tokio::test]
async fn text_only_turn_completes() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![MockTurn::Events(vec![
        ProviderEvent::TextDelta("hel".into()),
        ProviderEvent::TextDelta("lo".into()),
        usage(),
        ProviderEvent::Done {
            stop: StopReason::EndTurn,
        },
    ])]);
    let mut run = start(fx.builder(config(Mode::Ask), &mock));
    let events = run.turn_with("hi").await;
    let kinds: Vec<String> = events.iter().map(kind).collect();
    assert_eq!(
        kinds,
        [
            "turn_started",
            "text_delta",
            "text_delta",
            "usage",
            "context_status",
            "turn_receipt",
            "turn_finished"
        ],
        "{events:?}"
    );
    assert_eq!(finish_reason(&events), FinishReason::Completed);
    assert_eq!(mock.requests().len(), 1);
}

#[tokio::test]
async fn tool_loop_runs_until_no_tool_calls() {
    let fx = Fx::new();
    fx.write("a.txt", "alpha line\n");
    let mock = MockProvider::new(vec![
        tools_step(vec![call("c1", "read", json!({"path": "a.txt"}))]),
        text_step("done"),
    ]);
    let mut run = start(fx.builder(config(Mode::Ask), &mock));
    let events = run.turn_with("read it").await;
    assert_eq!(finish_reason(&events), FinishReason::Completed);
    let reqs = mock.requests();
    assert_eq!(reqs.len(), 2);
    assert!(tool_results(&reqs[0]).is_empty());
    let results = tool_results(&reqs[1]);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].0, "c1");
    assert!(results[0].1.contains("alpha line"), "{results:?}");
    assert!(!results[0].2);
    // The assistant's call precedes its result.
    let calls: Vec<&Content> = reqs[1]
        .messages
        .iter()
        .flat_map(|m| &m.content)
        .filter(|c| matches!(c, Content::ToolUse { .. }))
        .collect();
    assert_eq!(calls.len(), 1);
}

#[tokio::test]
async fn ask_emits_approval_and_waits() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![
        tools_step(vec![call(
            "w1",
            "write",
            json!({"path": "out.txt", "content": "x"}),
        )]),
        text_step("ok"),
        tools_step(vec![call(
            "w2",
            "write",
            json!({"path": "out2.txt", "content": "y"}),
        )]),
        text_step("ok"),
    ]);
    let mut run = start(EngineBuilder {
        interactive: true,
        ..fx.builder(config(Mode::Ask), &mock)
    });

    run.say("write it").await;
    let events = run
        .until(|e| matches!(e, Event::ToolApprovalRequested { .. }))
        .await;
    let Some(Event::ToolApprovalRequested {
        request_id,
        call_id,
        tool,
        ..
    }) = events.last()
    else {
        unreachable!()
    };
    assert_eq!((call_id.as_str(), tool.as_str()), ("w1", "write"));
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        !fx.dir.path().join("out.txt").exists(),
        "written before approval"
    );
    run.engine
        .send(Command::Approve {
            request_id: request_id.clone(),
            session_rule: None,
        })
        .await
        .unwrap();
    let events = run.turn().await;
    assert_eq!(finish_reason(&events), FinishReason::Completed);
    let f = finished(&events);
    assert_eq!(f.len(), 1);
    assert!(!f[0].2, "{f:?}");
    assert_eq!(
        std::fs::read_to_string(fx.dir.path().join("out.txt")).unwrap(),
        "x"
    );

    run.say("again").await;
    let events = run
        .until(|e| matches!(e, Event::ToolApprovalRequested { .. }))
        .await;
    let Some(Event::ToolApprovalRequested { request_id, .. }) = events.last() else {
        unreachable!()
    };
    run.engine
        .send(Command::Deny {
            request_id: request_id.clone(),
            reason: None,
        })
        .await
        .unwrap();
    let events = run.turn().await;
    let f = finished(&events);
    assert_eq!(f.len(), 1);
    assert!(f[0].2);
    assert!(f[0].1.contains("denied by user"), "{f:?}");
    assert!(!fx.dir.path().join("out2.txt").exists());
    let results = tool_results(mock.requests().last().unwrap());
    assert!(results.last().unwrap().1.contains("denied by user"));
}

#[tokio::test]
async fn headless_ask_is_denied_without_waiting() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![
        tools_step(vec![call(
            "w1",
            "write",
            json!({"path": "out.txt", "content": "x"}),
        )]),
        text_step("ok"),
    ]);
    let mut run = start(fx.builder(config(Mode::Ask), &mock));
    let events = run.turn_with("write it").await;
    assert_eq!(count(&events, "tool_approval_requested"), 0);
    let f = finished(&events);
    assert!(f[0].2);
    assert!(f[0].1.contains("approval"), "{f:?}");
    assert!(!fx.dir.path().join("out.txt").exists());
    assert_eq!(finish_reason(&events), FinishReason::Completed);
}

/// A sandbox that makes reads and writes of `slow*` files take a while and records when
/// each one ran.
struct Probe {
    inner: LocalSandbox,
    log: Mutex<Vec<(&'static str, String, Instant, Instant)>>,
}

impl Probe {
    fn delay(p: &Path) -> Option<(String, Duration)> {
        let name = p.file_name()?.to_str()?.to_string();
        let ms = match name.as_str() {
            "slow_a.txt" => 400,
            n if n.starts_with("slow") => 150,
            _ => return None,
        };
        Some((name, Duration::from_millis(ms)))
    }

    async fn timed<T>(
        &self,
        op: &'static str,
        p: &Path,
        f: impl std::future::Future<Output = T>,
    ) -> T {
        let Some((name, delay)) = Self::delay(p) else {
            return f.await;
        };
        let begin = Instant::now();
        tokio::time::sleep(delay).await;
        let out = f.await;
        self.log
            .lock()
            .unwrap()
            .push((op, name, begin, Instant::now()));
        out
    }

    /// When the one `op` of file `name` ran.
    fn span(&self, op: &str, name: &str) -> (Instant, Instant) {
        let log = self.log.lock().unwrap();
        let hits: Vec<_> = log
            .iter()
            .filter(|(o, n, ..)| *o == op && n == name)
            .collect();
        assert_eq!(hits.len(), 1, "{op} {name}: {:?}", log);
        (hits[0].2, hits[0].3)
    }
}

#[async_trait]
impl Sandbox for Probe {
    fn root(&self) -> &Path {
        self.inner.root()
    }
    fn resolve(&self, p: &Path) -> Result<PathBuf, SandboxError> {
        self.inner.resolve(p)
    }
    async fn read(&self, p: &Path) -> Result<Vec<u8>, SandboxError> {
        self.timed("read", p, self.inner.read(p)).await
    }
    async fn metadata(&self, p: &Path) -> Result<FileMeta, SandboxError> {
        self.inner.metadata(p).await
    }
    async fn write_atomic(
        &self,
        p: &Path,
        bytes: &[u8],
        mode: Option<u32>,
    ) -> Result<(), SandboxError> {
        self.timed("write", p, self.inner.write_atomic(p, bytes, mode))
            .await
    }
    async fn remove_file(&self, p: &Path) -> Result<(), SandboxError> {
        self.inner.remove_file(p).await
    }
    async fn spill(&self, bytes: &[u8]) -> Result<PathBuf, SandboxError> {
        self.inner.spill(bytes).await
    }
    async fn spawn(&self, spec: SpawnSpec) -> Result<Child, SandboxError> {
        self.inner.spawn(spec).await
    }
    fn walker(&self, base: &Path) -> Result<ignore::WalkBuilder, SandboxError> {
        self.inner.walker(base)
    }
    async fn http_get(&self, url: &Url) -> Result<HttpResponse, SandboxError> {
        self.inner.http_get(url).await
    }
}

#[tokio::test]
async fn read_only_calls_run_concurrently_writes_sequentially() {
    let fx = Fx::new();
    fx.write("slow_a.txt", "needle a\n");
    fx.write("slow_b.txt", "needle b\n");
    let probe = Arc::new(Probe {
        inner: LocalSandbox::new(fx.root()).unwrap(),
        log: Mutex::new(Vec::new()),
    });
    // The shared index reads through its own sandbox, so only the tools' I/O is timed.
    let plain: Arc<dyn Sandbox> = Arc::new(LocalSandbox::new(fx.root()).unwrap());
    let mock = MockProvider::new(vec![
        tools_step(vec![
            call(
                "g1",
                "grep",
                json!({"pattern": "needle", "path": "slow_a.txt"}),
            ),
            call(
                "g2",
                "grep",
                json!({"pattern": "needle", "path": "slow_b.txt"}),
            ),
        ]),
        tools_step(vec![
            call("w1", "write", json!({"path": "slow_c.out", "content": "c"})),
            call("w2", "write", json!({"path": "slow_d.out", "content": "d"})),
        ]),
        text_step("done"),
    ]);
    let mut run = start(EngineBuilder {
        sandbox: Some(probe.clone()),
        workspace: Some(Workspace::new(plain, fx.home.path())),
        ..fx.builder(config(Mode::Yolo), &mock)
    });
    let events = run.turn_with("go").await;
    assert_eq!(finish_reason(&events), FinishReason::Completed);
    assert!(finished(&events).iter().all(|f| !f.2), "{events:?}");

    let (a0, a1) = probe.span("read", "slow_a.txt");
    let (b0, b1) = probe.span("read", "slow_b.txt");
    assert!(a0 < b1 && b0 < a1, "the two greps did not overlap");
    let (c0, c1) = probe.span("write", "slow_c.out");
    let (d0, d1) = probe.span("write", "slow_d.out");
    assert!(c1 <= d0 || d1 <= c0, "the two writes overlapped");
    assert!(c1 <= d0, "writes ran out of model order");

    // g2 finished first, but results reach the context in model order.
    let results = tool_results(&mock.requests()[1]);
    let ids: Vec<&str> = results.iter().map(|r| r.0.as_str()).collect();
    assert_eq!(ids, ["g1", "g2"]);
    let order: Vec<String> = finished(&events).into_iter().map(|f| f.0).collect();
    assert_eq!(order, ["g1", "g2", "w1", "w2"]);
}

#[tokio::test]
async fn malformed_tool_args_return_error_result_and_continue() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![
        tools_step(vec![ProviderEvent::ToolCall {
            id: "c1".into(),
            name: "read".into(),
            input_json: "{not json".into(),
        }]),
        text_step("sorry"),
    ]);
    let mut run = start(fx.builder(config(Mode::Ask), &mock));
    let events = run.turn_with("go").await;
    let f = finished(&events);
    assert!(f[0].2);
    assert!(f[0].1.contains("invalid"), "{f:?}");
    assert_eq!(mock.requests().len(), 2);
    let results = tool_results(&mock.requests()[1]);
    assert_eq!(results[0].0, "c1");
    assert!(results[0].2);
    assert_eq!(finish_reason(&events), FinishReason::Completed);
}

#[tokio::test]
async fn unknown_tool_name_returns_error_result() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![
        tools_step(vec![call("c1", "frobnicate", json!({}))]),
        text_step("sorry"),
    ]);
    let mut run = start(fx.builder(config(Mode::Ask), &mock));
    let events = run.turn_with("go").await;
    let f = finished(&events);
    assert!(f[0].2);
    assert!(f[0].1.contains("unknown tool"), "{f:?}");
    assert_eq!(finish_reason(&events), FinishReason::Completed);
}

#[tokio::test]
async fn policy_deny_reports_rule() {
    let fx = Fx::new();
    fx.write("x.txt", "keep");
    let mut cfg = config(Mode::Yolo);
    cfg.permissions.deny = vec!["bash(rm*)".into()];
    let mock = MockProvider::new(vec![
        tools_step(vec![call("b1", "bash", json!({"command": "rm -f x.txt"}))]),
        text_step("ok"),
    ]);
    let mut run = start(fx.builder(cfg, &mock));
    let events = run.turn_with("go").await;
    assert!(
        !events.iter().any(|e| matches!(
            e,
            Event::Error {
                kind: ErrorKind::Policy,
                ..
            }
        )),
        "{events:?}"
    );
    let f = finished(&events);
    assert!(f[0].2);
    assert!(f[0].1.contains("bash(rm*)"), "{f:?}");
    assert!(fx.dir.path().join("x.txt").exists());
    assert_eq!(count(&events, "tool_started"), 0);
}

/// Review B-M4: bash commands do not inherit the provider key variables, the configured
/// `api_key_env` ones included (`HOME` stands in for one here: setting a real key in this
/// process would race the other tests).
#[tokio::test]
async fn bash_commands_do_not_see_provider_key_variables() {
    let fx = Fx::new();
    let mut cfg = config(Mode::Yolo);
    cfg.providers.compat.push(tautly_core::CompatEntry {
        name: "work".into(),
        base_url: "https://llm.example.com/v1".parse().unwrap(),
        api_key_env: Some("HOME".into()),
    });
    let command = "echo \"home:${HOME-unset} path:${PATH:+set}\"";
    let mock = MockProvider::new(vec![
        tools_step(vec![call("b1", "bash", json!({ "command": command }))]),
        text_step("ok"),
    ]);
    let mut run = start(fx.builder(cfg, &mock));
    let f = finished(&run.turn_with("go").await);
    assert!(f[0].1.contains("home:unset path:set"), "{f:?}");
}

#[tokio::test]
async fn mid_stream_drop_reissues_from_last_complete_message() {
    let fx = Fx::new();
    let cmd = json!({"command": "echo run >> log.txt"});
    let mock = MockProvider::new(vec![
        MockTurn::DropAfter(vec![call("c1", "bash", cmd.clone())]),
        tools_step(vec![call("c1", "bash", cmd)]),
        text_step("done"),
    ]);
    let mut run = start(fx.builder(config(Mode::Yolo), &mock));
    let events = run.turn_with("go").await;
    assert_eq!(finish_reason(&events), FinishReason::Completed);
    assert_eq!(count(&events, "tool_started"), 1);
    assert_eq!(
        std::fs::read_to_string(fx.dir.path().join("log.txt")).unwrap(),
        "run\n"
    );
    assert_eq!(mock.requests().len(), 3);
    // The discarded attempt is not logged.
    let logged_calls = records(&run.engine)
        .into_iter()
        .filter(|r| matches!(r, SessionRecord::ToolCall { .. }))
        .count();
    assert_eq!(logged_calls, 1);
}

#[tokio::test]
async fn records_appended_before_events_visible() {
    let fx = Fx::new();
    fx.write("a.txt", "alpha\n");
    let mock = MockProvider::new(vec![
        tools_step(vec![call("c1", "read", json!({"path": "a.txt"}))]),
        text_step("done"),
    ]);
    let mut run = start(fx.builder(config(Mode::Ask), &mock));
    run.say("go").await;
    loop {
        let ev = run.next().await;
        match ev {
            Event::ToolFinished { call_id, .. } => {
                let recs = records(&run.engine);
                let call_at = recs.iter().position(
                    |r| matches!(r, SessionRecord::ToolCall { id, .. } if *id == call_id),
                );
                let result_at = recs.iter().position(
                    |r| matches!(r, SessionRecord::ToolResult { id, .. } if *id == call_id),
                );
                assert!(call_at.is_some() && result_at.is_some(), "{recs:?}");
                assert!(call_at < result_at);
            }
            Event::TurnFinished { reason, .. } => {
                let recs = records(&run.engine);
                assert_eq!(
                    recs.last(),
                    Some(&SessionRecord::TurnEnd {
                        reason,
                        evidence: None,
                        synthetic: false,
                    })
                );
                assert!(matches!(
                    recs.first(),
                    Some(SessionRecord::Command(Command::SendMessage { .. }))
                ));
                assert_eq!(
                    recs.iter()
                        .filter(|r| matches!(r, SessionRecord::Request { .. }))
                        .count(),
                    2
                );
                break;
            }
            _ => {}
        }
    }
}

#[tokio::test]
async fn pin_and_state_tools_update_context() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![
        tools_step(vec![
            call("p1", "pin", json!({"text": "never touch vendor/"})),
            call(
                "s1",
                "state",
                json!({"plan": [{"text": "write tests", "status": "in_progress"}],
                       "decisions_add": ["use tokio"]}),
            ),
        ]),
        text_step("ok"),
    ]);
    let mut run = start(fx.builder(config(Mode::Ask), &mock));
    let events = run.turn_with("go").await;
    assert!(finished(&events).iter().all(|f| !f.2), "{events:?}");
    let req = &mock.requests()[1];
    let all = serde_json::to_string(&req.messages).unwrap();
    assert!(all.contains("never touch vendor/"), "{all}");
    let tail = tail_text(req);
    assert!(tail.contains("write tests"), "{tail}");
    assert!(tail.contains("use tokio"), "{tail}");
}

#[tokio::test]
async fn refused_model_pin_is_reported_to_the_model() {
    let fx = Fx::new();
    let pins: Vec<ProviderEvent> = (0..21)
        .map(|i| {
            call(
                &format!("p{i}"),
                "pin",
                json!({"text": format!("rule {i}")}),
            )
        })
        .collect();
    let mock = MockProvider::new(vec![tools_step(pins), text_step("ok")]);
    let mut run = start(fx.builder(config(Mode::Ask), &mock));
    let events = run.turn_with("go").await;
    let f = finished(&events);
    assert_eq!(f.len(), 21);
    assert!(f[..20].iter().all(|f| !f.2));
    assert!(f[20].2, "{:?}", f[20]);
    assert!(f[20].1.contains("pins were added"), "{:?}", f[20]);
    let results = tool_results(&mock.requests()[1]);
    assert!(results[20].2 && results[20].1.contains("pins were added"));
    let all = serde_json::to_string(&mock.requests()[1].messages).unwrap();
    assert!(!all.contains("- [model] rule 20"), "a refused pin was kept");
    // The step start after the pins was a cold cache: they were folded, and logged.
    assert!(records(&run.engine).contains(&SessionRecord::PinsFolded));
}

#[tokio::test]
async fn user_pin_command_pins_and_reports_refusals() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![text_step("ok")]);
    let mut run = start(fx.builder(config(Mode::Ask), &mock));
    run.engine
        .send(Command::Pin {
            text: "the user rule".into(),
        })
        .await
        .unwrap();
    run.engine
        .send(Command::Pin { text: "   ".into() })
        .await
        .unwrap();
    let events = run
        .until(|e| matches!(e, Event::Warning { message } if message.contains("pin")))
        .await;
    assert!(!events.is_empty());
    run.turn_with("go").await;
    let all = serde_json::to_string(&mock.requests()[0].messages).unwrap();
    assert!(all.contains("the user rule"), "{all}");
    assert!(all.contains("[user]"), "{all}");
}

#[tokio::test]
async fn commands_and_budget_scopes_from_a_newer_client_are_refused_not_fatal() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![text_step("ok")]);
    let mut run = start(fx.builder(config(Mode::Ask), &mock));
    run.engine.send(Command::Unknown).await.unwrap();
    run.engine
        .send(Command::SetBudget {
            scope: tautly_core::BudgetScope::Unknown,
            max_usd: None,
            max_tokens: Some(1),
        })
        .await
        .unwrap();
    run.say("go").await;
    let events = run.until(|e| matches!(e, Event::TurnFinished { .. })).await;
    assert!(
        events.iter().any(|e| matches!(
            e,
            Event::Error { message, .. } if message.contains("does not know this command")
        )),
        "{events:?}"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            Event::Warning { message } if message.contains("SetBudget ignored: unknown scope")
        )),
        "{events:?}"
    );
    // The 1-token limit of the unknown scope applied to nothing.
    assert_eq!(finish_reason(&events), FinishReason::Completed);
    // Both are logged as received.
    let recs = records(&run.engine);
    assert!(recs.contains(&SessionRecord::Command(Command::Unknown)));
}

#[tokio::test]
async fn context_gc_runs_only_at_step_start_and_emits_context_collected() {
    let fx = Fx::new();
    let big: String = (0..400)
        .map(|i| format!("line {i}: some filler text to make this file large\n"))
        .collect();
    fx.write("big.txt", &big);
    let mut script: Vec<MockTurn> = (0..6)
        .map(|i| {
            tools_step(vec![call(
                &format!("r{i}"),
                "read",
                json!({"path": "big.txt", "full": true}),
            )])
        })
        .collect();
    script.push(text_step("done"));
    let mock = MockProvider::new(script);
    let mut run = start(fx.builder(config(Mode::Ask), &mock));
    let events = run.turn_with("read it six times").await;
    assert_eq!(finish_reason(&events), FinishReason::Completed);

    let collected: Vec<usize> = events
        .iter()
        .enumerate()
        .filter(|(_, e)| matches!(e, Event::ContextCollected { .. }))
        .map(|(i, _)| i)
        .collect();
    assert!(!collected.is_empty(), "{events:?}");
    let mut open = 0i32;
    for (i, e) in events.iter().enumerate() {
        match e {
            Event::ToolStarted { .. } => open += 1,
            Event::ToolFinished { .. } => open -= 1,
            Event::ContextCollected { .. } => {
                assert_eq!(open, 0, "collected while tools ran (event {i})");
                // Before the step's request: no tool runs between it and the response.
                let answered = events[i..]
                    .iter()
                    .position(|e| matches!(e, Event::ContextStatus { .. }))
                    .expect("no request after the collection");
                assert!(
                    !events[i..i + answered]
                        .iter()
                        .any(|e| matches!(e, Event::ToolStarted { .. }))
                );
            }
            _ => {}
        }
    }
    let last = mock.requests().last().unwrap().clone();
    let results = tool_results(&last);
    assert!(
        results
            .iter()
            .any(|r| r.1.starts_with("[elided: superseded at turn")),
        "{results:?}"
    );
    assert!(!results.last().unwrap().1.starts_with("[elided"));
    let recs = records(&run.engine);
    assert!(
        recs.iter()
            .any(|r| matches!(r, SessionRecord::GcApplied { elided, .. } if !elided.is_empty())),
        "{recs:?}"
    );
}

#[tokio::test]
async fn identical_failed_call_with_no_writes_since_is_blocked() {
    let fx = Fx::new();
    let cmd = json!({"command": "ls missing.txt"});
    let mock = MockProvider::new(vec![
        tools_step(vec![call("b1", "bash", cmd.clone())]),
        tools_step(vec![call("b2", "bash", cmd)]),
        text_step("ok"),
    ]);
    let mut run = start(fx.builder(config(Mode::Yolo), &mock));
    let events = run.turn_with("go").await;
    let f = finished(&events);
    assert!(f[0].2 && !f[0].3, "{f:?}");
    assert!(f[1].2 && f[1].3, "{f:?}");
    assert!(f[1].1.contains("already tried at turn"), "{f:?}");
    assert_eq!(count(&events, "tool_started"), 1);
}

#[tokio::test]
async fn blocked_call_repeated_immediately_runs() {
    let fx = Fx::new();
    let cmd = json!({"command": "ls missing.txt"});
    let mock = MockProvider::new(vec![
        tools_step(vec![call("b1", "bash", cmd.clone())]),
        tools_step(vec![call("b2", "bash", cmd.clone())]),
        tools_step(vec![call("b3", "bash", cmd)]),
        text_step("ok"),
    ]);
    let mut run = start(fx.builder(config(Mode::Yolo), &mock));
    let events = run.turn_with("go").await;
    let f = finished(&events);
    assert!(f[1].3, "{f:?}");
    assert!(!f[2].3, "{f:?}");
    assert_eq!(count(&events, "tool_started"), 2);
}

#[tokio::test]
async fn write_since_failure_unblocks_retry() {
    let fx = Fx::new();
    let cmd = json!({"command": "ls missing.txt"});
    let mock = MockProvider::new(vec![
        tools_step(vec![call("b1", "bash", cmd.clone())]),
        tools_step(vec![call(
            "w1",
            "write",
            json!({"path": "missing.txt", "content": "now here"}),
        )]),
        tools_step(vec![call("b2", "bash", cmd)]),
        text_step("ok"),
    ]);
    let mut run = start(fx.builder(config(Mode::Yolo), &mock));
    let events = run.turn_with("go").await;
    let f = finished(&events);
    assert!(f[0].2);
    assert!(!f[2].3, "{f:?}");
    assert!(!f[2].2, "{f:?}");
}

#[tokio::test]
async fn failed_attempts_appear_in_working_state_capped_at_10() {
    let fx = Fx::new();
    let calls: Vec<ProviderEvent> = (1..=12)
        .map(|i| {
            call(
                &format!("b{i}"),
                "bash",
                json!({"command": format!("ls m{i:02}.txt")}),
            )
        })
        .collect();
    let mock = MockProvider::new(vec![tools_step(calls), text_step("ok")]);
    let mut run = start(fx.builder(config(Mode::Yolo), &mock));
    let events = run.turn_with("go").await;
    assert!(finished(&events).iter().all(|f| f.2));
    let tail = tail_text(&mock.requests()[1]);
    let section = tail
        .split("failed attempts (do not repeat):\n")
        .nth(1)
        .unwrap_or_else(|| panic!("{tail}"));
    let lines: Vec<&str> = section.lines().filter(|l| l.starts_with("- ")).collect();
    assert_eq!(lines.len(), 10, "{tail}");
    assert!(section.contains("m12.txt") && section.contains("m03.txt"));
    assert!(!section.contains("m01.txt") && !section.contains("m02.txt"));
}

#[tokio::test]
async fn edits_reindex_before_result_returned() {
    let fx = Fx::new();
    fx.write("src.rs", "fn helper(x: u8) -> u8 {\n    x + 1\n}\n");
    let ws = fx.ready_workspace().await;
    let mock = MockProvider::new(vec![
        tools_step(vec![call("r1", "read", json!({"path": "src.rs"}))]),
        tools_step(vec![
            call(
                "e1",
                "edit",
                json!({"path": "src.rs", "old_text": "    x + 1\n}",
                       "new_text": "    x + 2\n}\nfn added() {}"}),
            ),
            call("s1", "symbol", json!({"name": "added"})),
        ]),
        text_step("ok"),
    ]);
    let mut run = start(EngineBuilder {
        workspace: Some(ws),
        ..fx.builder(config(Mode::AutoEdit), &mock)
    });
    let events = run.turn_with("go").await;
    let f = finished(&events);
    assert!(!f[1].2, "{f:?}");
    assert_eq!(f[2].0, "s1");
    assert!(f[2].1.contains("fn added() {}"), "{f:?}");
    assert!(!f[2].1.contains("index building"), "{f:?}");
}

#[tokio::test]
async fn max_turns_stops() {
    let fx = Fx::new();
    fx.write("a.txt", "a\n");
    let mut cfg = config(Mode::Ask);
    cfg.max_turns = 2;
    let mock = MockProvider::new(
        (0..4)
            .map(|i| {
                tools_step(vec![call(
                    &format!("c{i}"),
                    "read",
                    json!({"path": "a.txt"}),
                )])
            })
            .collect(),
    );
    let mut run = start(fx.builder(cfg, &mock));
    let events = run.turn_with("loop").await;
    assert_eq!(finish_reason(&events), FinishReason::MaxTurns);
    assert_eq!(mock.requests().len(), 2);
    assert_eq!(count(&events, "turn_started"), 1);
}

#[tokio::test]
async fn cancel_ends_the_turn_and_answers_every_call() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![
        tools_step(vec![
            call("w1", "write", json!({"path": "o.txt", "content": "x"})),
            call("r1", "read", json!({"path": "o.txt"})),
        ]),
        text_step("next"),
    ]);
    let mut run = start(EngineBuilder {
        interactive: true,
        ..fx.builder(config(Mode::Ask), &mock)
    });
    run.say("go").await;
    run.until(|e| matches!(e, Event::ToolApprovalRequested { .. }))
        .await;
    run.engine.send(Command::Cancel).await.unwrap();
    let events = run.turn().await;
    assert_eq!(finish_reason(&events), FinishReason::Cancelled);
    assert!(!fx.dir.path().join("o.txt").exists());

    // The next request answers both calls of the cancelled step.
    run.turn_with("carry on").await;
    let results = tool_results(mock.requests().last().unwrap());
    let ids: Vec<&str> = results.iter().map(|r| r.0.as_str()).collect();
    assert_eq!(ids, ["w1", "r1"]);
    assert!(results.iter().all(|r| r.2));
}

#[tokio::test]
async fn shutdown_denies_pending_approvals_and_stops() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![tools_step(vec![call(
        "w1",
        "write",
        json!({"path": "o.txt", "content": "x"}),
    )])]);
    let mut run = start(EngineBuilder {
        interactive: true,
        ..fx.builder(config(Mode::Ask), &mock)
    });
    run.say("go").await;
    run.until(|e| matches!(e, Event::ToolApprovalRequested { .. }))
        .await;
    run.engine.send(Command::Shutdown).await.unwrap();
    let events = run.until(|e| matches!(e, Event::TurnFinished { .. })).await;
    assert_eq!(finish_reason(&events), FinishReason::Cancelled);
    let closed =
        tokio::time::timeout(WAIT, async { while run.events.recv().await.is_some() {} }).await;
    assert!(closed.is_ok(), "the engine did not stop");
    assert!(!fx.dir.path().join("o.txt").exists());
    assert!(run.engine.send(Command::Cancel).await.is_err());
}

#[tokio::test]
async fn compaction_and_collection_apply_in_one_epoch_at_step_start() {
    let fx = Fx::new();
    for i in 1..=4 {
        fx.write(&format!("f{i}.txt"), &format!("file {i}\n"));
    }
    let mut cfg = config(Mode::Ask);
    cfg.catalog_overrides = Some(catalog(16_000, 1_000));
    let mut script: Vec<MockTurn> = (1..=4)
        .map(|i| {
            tools_step(vec![call(
                &format!("r{i}"),
                "read",
                json!({"path": format!("f{i}.txt")}),
            )])
        })
        .collect();
    script.push(text_step("SUMMARY-TEXT"));
    script.push(text_step("done"));
    let mock = MockProvider::new(script);
    let mut run = start(fx.builder(cfg, &mock));
    let prompt = format!(
        "PROMPT-MARKER {}",
        "lorem ipsum dolor sit amet ".repeat(1500)
    );
    let events = run.turn_with(&prompt).await;
    assert_eq!(
        finish_reason(&events),
        FinishReason::Completed,
        "{events:?}"
    );

    let reqs = mock.requests();
    assert_eq!(reqs.len(), 6);
    // The fifth request is the summary request; the step's own request follows it.
    assert!(system_block(&reqs[4], 0).contains("summar"));
    let last = serde_json::to_string(&reqs[5].messages).unwrap();
    assert!(last.contains("SUMMARY-TEXT"));
    assert!(!last.contains("PROMPT-MARKER"));
    let compacted = events
        .iter()
        .position(|e| matches!(e, Event::Compacted { .. }))
        .expect("no Compacted event");
    let Event::Compacted {
        removed_turns,
        summary,
        ..
    } = &events[compacted]
    else {
        unreachable!()
    };
    assert_eq!(*removed_turns, 1);
    assert_eq!(summary, "SUMMARY-TEXT");
    // Announced before it ran.
    assert!(events[..compacted].iter().any(|e| matches!(
        e,
        Event::ContextStatus { warnings, .. } if warnings.iter().any(|w| w.contains("compacting"))
    )));
    let recs = records(&run.engine);
    assert!(recs.iter().any(
        |r| matches!(r, SessionRecord::Compaction { removed, summary } if removed.len() == 1 && summary == "SUMMARY-TEXT")
    ));
}

#[tokio::test]
async fn failed_summary_falls_back_to_dropping_the_oldest_turns() {
    let fx = Fx::new();
    for i in 1..=4 {
        fx.write(&format!("f{i}.txt"), &format!("file {i}\n"));
    }
    let mut cfg = config(Mode::Ask);
    cfg.catalog_overrides = Some(catalog(16_000, 1_000));
    let mut script: Vec<MockTurn> = (1..=4)
        .map(|i| {
            tools_step(vec![call(
                &format!("r{i}"),
                "read",
                json!({"path": format!("f{i}.txt")}),
            )])
        })
        .collect();
    script.push(MockTurn::Error(ProviderError::InvalidRequest(
        "too long".into(),
    )));
    script.push(text_step("done"));
    let mock = MockProvider::new(script);
    let mut run = start(fx.builder(cfg, &mock));
    let prompt = format!(
        "PROMPT-MARKER {}",
        "lorem ipsum dolor sit amet ".repeat(1500)
    );
    let events = run.turn_with(&prompt).await;
    assert_eq!(
        finish_reason(&events),
        FinishReason::Completed,
        "{events:?}"
    );
    assert!(events.iter().any(
        |e| matches!(e, Event::Compacted { removed_turns: 1, summary, .. } if summary.is_empty())
    ));
    let last = serde_json::to_string(&mock.requests()[5].messages).unwrap();
    assert!(!last.contains("PROMPT-MARKER"));
}

#[tokio::test]
async fn start_up_warnings_are_reported() {
    let fx = Fx::new();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret.md"), "SECRET").unwrap();
    std::os::unix::fs::symlink(
        outside.path().join("secret.md"),
        fx.dir.path().join("AGENTS.md"),
    )
    .unwrap();
    let mock = MockProvider::new(vec![text_step("ok")]);
    let mut run = start(EngineBuilder {
        config_warnings: vec![".tautly/config.toml: ignored `telemetry`".into()],
        ..fx.builder(config(Mode::Ask), &mock)
    });
    let first = run.next().await;
    assert!(
        matches!(&first, Event::SessionChanged { session_id } if session_id == run.engine.session_id()),
        "{first:?}"
    );
    run.say("go").await;
    let events = run.until(|e| matches!(e, Event::TurnFinished { .. })).await;
    let warnings: Vec<&String> = events
        .iter()
        .filter_map(|e| match e {
            Event::Warning { message } => Some(message),
            _ => None,
        })
        .collect();
    assert!(
        warnings.iter().any(|w| w.contains("ignored `telemetry`")),
        "{warnings:?}"
    );
    assert!(
        warnings.iter().any(|w| w.contains("AGENTS.md")),
        "{warnings:?}"
    );
    let req = &mock.requests()[0];
    assert!(!serde_json::to_string(req).unwrap().contains("SECRET"));
}

#[tokio::test]
async fn first_request_carries_repo_map_and_memory_index_when_ready() {
    let fx = Fx::new();
    fx.write(
        "src/lib.rs",
        "pub fn alpha() {}\npub fn beta() { alpha() }\n",
    );
    fx.write(
        ".tautly/memory/use-pnpm.md",
        "---\nid: use-pnpm\ntitle: Use pnpm\nkind: convention\ncreated: 2026-01-01T00:00:00Z\n---\nAlways pnpm.\n",
    );
    let ws = fx.ready_workspace().await;
    let mock = MockProvider::new(vec![text_step("ok"), text_step("ok")]);
    let mut run = start(EngineBuilder {
        workspace: Some(ws),
        ..fx.builder(config(Mode::Ask), &mock)
    });
    run.turn_with("one").await;
    run.turn_with("two").await;
    let reqs = mock.requests();
    let memory = system_block(&reqs[0], 2);
    assert!(memory.contains("Use pnpm"), "{memory}");
    let map = system_block(&reqs[0], 3);
    assert!(map.contains("src"), "{map}");
    // The stable prefix does not change between requests.
    assert_eq!(reqs[0].system, reqs[1].system);
    assert!(records(&run.engine).iter().any(
        |r| matches!(r, SessionRecord::MemoryIndexApplied { text } if text.contains("Use pnpm"))
    ));
}

#[tokio::test]
async fn two_sessions_share_one_workspace() {
    let fx = Fx::new();
    fx.write("src.rs", "fn helper() {}\n");
    let ws = fx.ready_workspace().await;
    let mock_a = MockProvider::new(vec![
        tools_step(vec![call(
            "w1",
            "write",
            json!({"path": "two.rs", "content": "fn shared_fn() {}\n"}),
        )]),
        text_step("ok"),
    ]);
    let mock_b = MockProvider::new(vec![
        tools_step(vec![call("s1", "symbol", json!({"name": "shared_fn"}))]),
        text_step("ok"),
    ]);
    let mut a = start(EngineBuilder {
        workspace: Some(ws.clone()),
        ..fx.builder(config(Mode::AutoEdit), &mock_a)
    });
    let mut b = start(EngineBuilder {
        workspace: Some(ws.clone()),
        ..fx.builder(config(Mode::AutoEdit), &mock_b)
    });
    assert_ne!(a.engine.session_id(), b.engine.session_id());
    let ea = a.turn_with("write").await;
    assert!(!finished(&ea)[0].2, "{ea:?}");
    let eb = b.turn_with("find").await;
    let f = finished(&eb);
    assert!(f[0].1.contains("two.rs"), "{f:?}");
    assert!(!f[0].1.contains("index building"), "{f:?}");
}

/// Total bytes under `dir`, recursively (0 if missing).
fn dir_bytes(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|e| {
            let m = e.metadata().unwrap();
            if m.is_dir() {
                dir_bytes(&e.path())
            } else {
                m.len()
            }
        })
        .sum()
}

fn state_steps(from: usize, n: usize) -> Vec<MockTurn> {
    let mut script: Vec<MockTurn> = (from..from + n)
        .map(|i| {
            tools_step(vec![call(
                &format!("s{i}"),
                "state",
                json!({"decisions_add": [format!("decision number {i}")]}),
            )])
        })
        .collect();
    script.push(text_step("done"));
    script
}

#[tokio::test]
async fn request_records_keep_only_hashes_and_blob_bytes_grow_linearly() {
    let fx = Fx::new();
    let mut script = state_steps(0, 49);
    script.extend(state_steps(49, 49));
    let mock = MockProvider::new(script);
    let mut run = start(fx.builder(config(Mode::Ask), &mock));
    let blobs = fx
        .home
        .path()
        .join("sessions")
        .join(tautly_session::paths::project_id(&fx.root()))
        .join("blobs");
    let before = dir_bytes(&blobs);
    assert_eq!(
        finish_reason(&run.turn_with("one").await),
        FinishReason::Completed
    );
    let first = dir_bytes(&blobs) - before;
    assert_eq!(
        finish_reason(&run.turn_with("two").await),
        FinishReason::Completed
    );
    let second = dir_bytes(&blobs) - before - first;
    // Each half adds about as much as the first: no per-request copy of the whole
    // (ever-growing) request.
    assert!(
        second <= first + first / 2 + 4096,
        "first 50 steps added {first} bytes, the next 50 added {second}"
    );
    let reqs = mock.requests();
    assert_eq!(reqs.len(), 100);
    let recs = records(&run.engine);
    let hashes: Vec<_> = recs
        .iter()
        .filter_map(|r| match r {
            SessionRecord::Request { hash, .. } => Some(hash.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(hashes.len(), 100);
    let last_bytes = serde_json::to_vec(reqs.last().unwrap()).unwrap();
    assert_eq!(
        hashes.last().unwrap().as_str(),
        blake3::hash(&last_bytes).to_hex().as_str()
    );
    // No request is stored as a blob.
    let reader = SessionReader::<SessionRecord>::open(run.engine.session_path()).unwrap();
    assert!(hashes.iter().all(|h| reader.read_blob(h).is_err()));
    // The latest request, for `prompt show --full`.
    let saved: CanonicalRequest =
        serde_json::from_slice(&std::fs::read(run.engine.last_request_path()).unwrap()).unwrap();
    assert_eq!(&saved, reqs.last().unwrap());
}

fn transient() -> ProviderError {
    ProviderError::Transient {
        status: Some(503),
        retry_after: Some(Duration::from_millis(1)),
        msg: "overloaded".into(),
    }
}

#[tokio::test]
async fn transient_errors_before_content_are_retried_only_by_the_retry_wrapper() {
    let fx = Fx::new();
    let mock = MockProvider::new((0..8).map(|_| MockTurn::Error(transient())).collect());
    let mut run = start(fx.builder(config(Mode::Ask), &mock));
    let events = run.turn_with("hi").await;
    assert_eq!(finish_reason(&events), FinishReason::Error);
    assert_eq!(mock.requests().len(), 5, "spec 3.5: at most 5 attempts");
    assert!(events.iter().any(|e| matches!(
        e,
        Event::Error {
            kind: ErrorKind::ProviderTransient,
            retryable: true,
            ..
        }
    )));
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, Event::Warning { message } if message.contains("broke off")))
    );
}

#[tokio::test]
async fn a_break_after_usage_only_is_not_resent_by_the_engine() {
    let fx = Fx::new();
    // Usage alone is not content: Retrying holds it back and retries; the engine must not
    // add attempts of its own.
    let mock = MockProvider::new((0..8).map(|_| MockTurn::DropAfter(vec![usage()])).collect());
    let mut run = start(fx.builder(config(Mode::Ask), &mock));
    let events = run.turn_with("hi").await;
    assert_eq!(finish_reason(&events), FinishReason::Error);
    assert_eq!(mock.requests().len(), 5);
}

#[tokio::test]
async fn unpin_removes_a_pin_and_reports_unknown_text() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![text_step("ok")]);
    let mut run = start(fx.builder(config(Mode::Ask), &mock));
    for cmd in [
        Command::Pin {
            text: "keep this".into(),
        },
        Command::Pin {
            text: "drop this".into(),
        },
        Command::Unpin {
            text: "drop this".into(),
        },
        Command::Unpin {
            text: "never pinned".into(),
        },
    ] {
        run.engine.send(cmd).await.unwrap();
    }
    run.until(|e| matches!(e, Event::Warning { message } if message.contains("no pin")))
        .await;
    run.turn_with("go").await;
    let all = serde_json::to_string(&mock.requests()[0].messages).unwrap();
    assert!(all.contains("keep this"));
    assert!(!all.contains("drop this"));
}

/// A `LocalSandbox` whose directory walks fail, so the code index cannot refresh.
struct NoWalk(LocalSandbox);

#[async_trait]
impl Sandbox for NoWalk {
    fn root(&self) -> &Path {
        self.0.root()
    }
    fn resolve(&self, p: &Path) -> Result<PathBuf, SandboxError> {
        self.0.resolve(p)
    }
    async fn read(&self, p: &Path) -> Result<Vec<u8>, SandboxError> {
        self.0.read(p).await
    }
    async fn metadata(&self, p: &Path) -> Result<FileMeta, SandboxError> {
        self.0.metadata(p).await
    }
    async fn write_atomic(
        &self,
        p: &Path,
        bytes: &[u8],
        mode: Option<u32>,
    ) -> Result<(), SandboxError> {
        self.0.write_atomic(p, bytes, mode).await
    }
    async fn remove_file(&self, p: &Path) -> Result<(), SandboxError> {
        self.0.remove_file(p).await
    }
    async fn spill(&self, bytes: &[u8]) -> Result<PathBuf, SandboxError> {
        self.0.spill(bytes).await
    }
    async fn spawn(&self, spec: SpawnSpec) -> Result<Child, SandboxError> {
        self.0.spawn(spec).await
    }
    fn walker(&self, _base: &Path) -> Result<ignore::WalkBuilder, SandboxError> {
        Err(SandboxError::Denied(
            "walks are disabled in this test".into(),
        ))
    }
    async fn http_get(&self, url: &Url) -> Result<HttpResponse, SandboxError> {
        self.0.http_get(url).await
    }
}

#[tokio::test]
async fn a_failed_index_refresh_is_reported_once() {
    let fx = Fx::new();
    fx.write("a.txt", "a\n");
    let sb: Arc<dyn Sandbox> = Arc::new(NoWalk(LocalSandbox::new(fx.root()).unwrap()));
    let ws = Workspace::new(sb, fx.home.path());
    ws.start();
    let deadline = Instant::now() + WAIT;
    while ws.index().and_then(|i| i.last_error()).is_none() {
        assert!(Instant::now() < deadline, "the refresh never failed");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let mock = MockProvider::new(vec![
        tools_step(vec![call("c1", "read", json!({"path": "a.txt"}))]),
        text_step("ok"),
        tools_step(vec![call("c2", "read", json!({"path": "a.txt"}))]),
        text_step("ok"),
    ]);
    let mut run = start(EngineBuilder {
        workspace: Some(ws),
        ..fx.builder(config(Mode::Ask), &mock)
    });
    let mut events = run.turn_with("one").await;
    events.extend(run.turn_with("two").await);
    let n = events
        .iter()
        .filter(|e| matches!(e, Event::Warning { message } if message.contains("code index refresh failed")))
        .count();
    assert_eq!(n, 1, "{events:?}");
}

/// Review C M10 seam (Task 49's working-tree lease): a workspace knows how many sessions
/// run on it.
#[tokio::test]
async fn a_workspace_counts_its_live_sessions() {
    let fx = Fx::new();
    let sb: Arc<dyn Sandbox> = Arc::new(LocalSandbox::new(fx.root()).unwrap());
    let ws = Workspace::new(sb, fx.home.path());
    assert_eq!(ws.live_sessions(), 0);
    let session = |mock: &MockProvider| {
        start(EngineBuilder {
            workspace: Some(Arc::clone(&ws)),
            ..fx.builder(config(Mode::Ask), mock)
        })
    };
    let (one, two) = (MockProvider::new(vec![]), MockProvider::new(vec![]));
    let (mut a, mut b) = (session(&one), session(&two));
    assert_eq!(ws.live_sessions(), 2);
    for run in [&mut a, &mut b] {
        run.engine.send(Command::Shutdown).await.unwrap();
        while tokio::time::timeout(WAIT, run.events.recv())
            .await
            .expect("the engine did not stop")
            .is_some()
        {}
    }
    let deadline = Instant::now() + WAIT;
    while ws.live_sessions() != 0 {
        assert!(
            Instant::now() < deadline,
            "{} still live",
            ws.live_sessions()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn old_spill_files_are_cleaned_at_start() {
    let fx = Fx::new();
    fx.write(".tautly/tmp/spill/old", "stale output");
    fx.write(".tautly/tmp/spill/new", "fresh output");
    let old = std::fs::File::options()
        .write(true)
        .open(fx.dir.path().join(".tautly/tmp/spill/old"))
        .unwrap();
    old.set_modified(std::time::SystemTime::now() - Duration::from_secs(30 * 24 * 3600))
        .unwrap();
    drop(old);
    let mock = MockProvider::new(vec![text_step("ok")]);
    let mut run = start(fx.builder(config(Mode::Ask), &mock));
    run.turn_with("go").await;
    let deadline = Instant::now() + WAIT;
    while fx.dir.path().join(".tautly/tmp/spill/old").exists() {
        assert!(Instant::now() < deadline, "old spill file kept");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(fx.dir.path().join(".tautly/tmp/spill/new").exists());
}

#[tokio::test]
async fn a_client_that_stops_reading_cannot_wedge_cancel_or_the_actor() {
    let fx = Fx::new();
    // One step that emits far more events than the channel holds.
    let pins: Vec<ProviderEvent> = (0..20)
        .map(|i| {
            call(
                &format!("p{i}"),
                "pin",
                json!({"text": format!("rule {i}")}),
            )
        })
        .collect();
    let mut script = vec![MockTurn::Events(
        (0..300)
            .map(|i| ProviderEvent::TextDelta(format!("t{i} ")))
            .collect(),
    )];
    script[0] = MockTurn::Events({
        let mut evs: Vec<ProviderEvent> = (0..300)
            .map(|i| ProviderEvent::TextDelta(format!("t{i} ")))
            .collect();
        evs.extend(pins);
        evs
    });
    let mock = MockProvider::new(script);
    let run = start(fx.builder(config(Mode::Ask), &mock));
    // The client never reads `run.events`.
    run.say("go").await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let sends = async {
        for i in 0..200 {
            run.engine
                .send(Command::Deny {
                    request_id: format!("stale-{i}"),
                    reason: None,
                })
                .await
                .unwrap();
        }
        run.engine.send(Command::Cancel).await.unwrap();
    };
    tokio::time::timeout(WAIT, sends)
        .await
        .expect("the actor stopped taking commands");
    let deadline = Instant::now() + WAIT;
    loop {
        let ended = records(&run.engine)
            .iter()
            .any(|r| matches!(r, SessionRecord::TurnEnd { .. }));
        if ended {
            break;
        }
        assert!(Instant::now() < deadline, "the cancelled turn never ended");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// A provider that panics: an engine bug inside the turn task.
struct Exploding;

impl tautly_provider::Provider for Exploding {
    fn name(&self) -> &str {
        "exploding"
    }
    fn stream(
        &self,
        _req: CanonicalRequest,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> futures::stream::BoxStream<'static, Result<ProviderEvent, ProviderError>> {
        panic!("provider exploded on purpose");
    }
}

#[tokio::test]
async fn after_a_turn_panic_every_command_is_answered_and_the_panic_is_logged() {
    let fx = Fx::new();
    let mut run = start(EngineBuilder {
        provider: Some(Arc::new(Exploding)),
        data_home: Some(fx.home.path().to_path_buf()),
        ..EngineBuilder::new(config(Mode::Ask), fx.dir.path())
    });
    let events = run.turn_with("boom").await;
    assert_eq!(finish_reason(&events), FinishReason::Error);
    assert!(events.iter().any(
        |e| matches!(e, Event::Error { message, .. } if message.contains("provider exploded on purpose"))
    ));
    assert!(records(&run.engine).iter().any(
        |r| matches!(r, SessionRecord::EngineError { message } if message.contains("provider exploded on purpose"))
    ));
    run.engine
        .send(Command::Pin { text: "x".into() })
        .await
        .unwrap();
    let ev = run.next().await;
    assert!(
        matches!(&ev, Event::Error { message, .. } if message.contains("takes no more")),
        "{ev:?}"
    );
}

#[tokio::test]
async fn a_pin_queued_behind_a_message_reaches_that_turn() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![
        tools_step(vec![call(
            "w1",
            "write",
            json!({"path": "o.txt", "content": "x"}),
        )]),
        text_step("first done"),
        text_step("second done"),
    ]);
    let mut run = start(EngineBuilder {
        interactive: true,
        ..fx.builder(config(Mode::Ask), &mock)
    });
    run.say("first").await;
    let events = run
        .until(|e| matches!(e, Event::ToolApprovalRequested { .. }))
        .await;
    let Some(Event::ToolApprovalRequested { request_id, .. }) = events.last() else {
        unreachable!()
    };
    // While the first turn waits: a second message, then a pin behind it.
    run.say("second").await;
    run.engine
        .send(Command::Pin {
            text: "queued pin".into(),
        })
        .await
        .unwrap();
    run.engine
        .send(Command::Approve {
            request_id: request_id.clone(),
            session_rule: None,
        })
        .await
        .unwrap();
    run.turn().await;
    run.turn().await;
    let reqs = mock.requests();
    assert_eq!(reqs.len(), 3);
    let second = serde_json::to_string(&reqs[2].messages).unwrap();
    assert!(second.contains("queued pin"), "{second}");
}

fn flood(deltas: usize, bash_calls: usize) -> MockTurn {
    let mut evs: Vec<ProviderEvent> = (0..deltas)
        .map(|i| ProviderEvent::TextDelta(format!("t{i} ")))
        .collect();
    evs.extend((0..bash_calls).map(|i| {
        call(
            &format!("b{i}"),
            "bash",
            json!({"command": format!("echo {i}")}),
        )
    }));
    evs.push(ProviderEvent::Done {
        stop: StopReason::ToolUse,
    });
    MockTurn::Events(evs)
}

async fn turn_end_logged_within(engine: &Engine, limit: Duration) {
    let begin = Instant::now();
    loop {
        if records(engine)
            .iter()
            .any(|r| matches!(r, SessionRecord::TurnEnd { .. }))
        {
            return;
        }
        assert!(
            begin.elapsed() < limit,
            "TurnEnd not logged within {limit:?} of Cancel"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn cancel_with_many_pending_calls_and_a_stopped_reader_ends_quickly() {
    for calls in [40, 80] {
        let fx = Fx::new();
        let mock = MockProvider::new(vec![flood(200, calls)]);
        let run = start(fx.builder(config(Mode::Ask), &mock));
        // The client never reads `run.events`.
        run.say("go").await;
        tokio::time::sleep(Duration::from_millis(1000)).await;
        run.engine.send(Command::Cancel).await.unwrap();
        // About 1.2-1.5 s alone (one 1 s grace, then every event drops at once); one second
        // per pending event before the fix (28 s for 40 calls). The bound leaves room for
        // fsync latency when the whole suite runs in parallel.
        turn_end_logged_within(&run.engine, Duration::from_secs(10)).await;
    }
}

#[tokio::test]
async fn a_reader_stalled_after_cancel_still_gets_turn_finished() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![flood(400, 0)]);
    let mut run = start(fx.builder(config(Mode::Ask), &mock));
    run.say("go").await;
    // Past the start-up wait, with the stream blocked on a full channel.
    tokio::time::sleep(Duration::from_millis(1000)).await;
    run.engine.send(Command::Cancel).await.unwrap();
    // A busy client: it reads again only after two seconds.
    tokio::time::sleep(Duration::from_secs(2)).await;
    let events = run.until(|e| matches!(e, Event::TurnFinished { .. })).await;
    assert_eq!(finish_reason(&events), FinishReason::Cancelled);
    // The drops are announced right before the turn's receipt, which precedes it.
    let n = events.len();
    assert!(
        matches!(&events[n - 3], Event::Warning { message } if message.contains("dropped"))
            && matches!(&events[n - 2], Event::TurnReceipt { .. }),
        "{:?}",
        &events[n.saturating_sub(4)..]
    );
}
