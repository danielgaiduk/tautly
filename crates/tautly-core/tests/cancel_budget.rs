//! Cancellation, turn timeouts, budgets, the cost ledger and signals (Task 25), against a
//! scripted provider (`MockProvider`) and a real `LocalSandbox` on a temporary directory.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, UNIX_EPOCH};

use async_trait::async_trait;
use serde_json::{Value, json};
use tautly_core::{
    BudgetLimits, BudgetScope, Command, Config, Engine, EngineBuilder, Event, FinishReason, Ledger,
    SessionRecord, Signal, SignalRouter, UsageRole, handle_signals, install_signal_handlers,
};
use tautly_provider::{
    CanonicalRequest, Catalog, Content, MockProvider, MockTurn, ModelId, ProviderEvent, Quota,
    RawUsage, StopReason,
};
use tautly_sandbox::{
    Child, FileMeta, HttpResponse, LocalSandbox, Mode, Sandbox, SandboxError, SpawnSpec, Url,
};
use tautly_session::SessionReader;
use tempfile::TempDir;
use tokio::sync::mpsc;

const MODEL: &str = "mock/test";
const WAIT: Duration = Duration::from_secs(30);

fn catalog(price: Option<&str>) -> Catalog {
    let price = price
        .map(|p| format!("\n[models.\"{MODEL}\".price]\n{p}\n"))
        .unwrap_or_default();
    Catalog::from_toml(&format!(
        r#"[models."{MODEL}"]
context_window = 200000
max_output = 8000
parallel_tools = true
reasoning = "none"
cache = "none"
images = false
edit_format = "str_replace"
{price}"#
    ))
    .unwrap()
}

const PRICE: &str = "input = 3.0\noutput = 15.0\ncache_read = 0.3\ncache_write = 3.75";

/// Yolo (bash runs without asking), no price unless `priced`.
fn config(priced: bool) -> Config {
    let mut c = Config {
        model: ModelId::from(MODEL),
        catalog_overrides: Some(catalog(priced.then_some(PRICE))),
        ..Config::default()
    };
    c.permissions.mode = Mode::Yolo;
    c
}

fn call(id: &str, name: &str, args: Value) -> ProviderEvent {
    ProviderEvent::ToolCall {
        id: id.into(),
        name: name.into(),
        input_json: args.to_string(),
    }
}

fn bash(id: &str, cmd: &str) -> ProviderEvent {
    call(id, "bash", json!({ "command": cmd }))
}

fn tokens(input: u64, output: u64) -> RawUsage {
    RawUsage {
        input,
        output,
        cache_read: 0,
        cache_write: 0,
    }
}

/// A response with tool calls and, if given, usage.
fn tools_step(calls: Vec<ProviderEvent>, usage: Option<RawUsage>) -> MockTurn {
    let mut evs = calls;
    evs.extend(usage.map(ProviderEvent::Usage));
    evs.push(ProviderEvent::Done {
        stop: StopReason::ToolUse,
    });
    MockTurn::Events(evs)
}

fn text_step(text: &str, usage: RawUsage) -> MockTurn {
    MockTurn::Events(vec![
        ProviderEvent::TextDelta(text.into()),
        ProviderEvent::Usage(usage),
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
        std::fs::write(self.dir.path().join(rel), text).unwrap();
    }

    fn builder(&self, cfg: Config, mock: &MockProvider) -> EngineBuilder {
        EngineBuilder {
            provider: Some(Arc::new(mock.clone())),
            data_home: Some(self.home.path().to_path_buf()),
            ..EngineBuilder::new(cfg, self.dir.path())
        }
    }

    fn ledger(&self) -> Ledger {
        Ledger::open(&self.home.path().join("ledger.sqlite")).unwrap()
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
        self.send(Command::SendMessage { text: text.into() }).await;
    }

    async fn send(&self, cmd: Command) {
        self.engine.send(cmd).await.unwrap();
    }

    async fn next(&mut self) -> Event {
        tokio::time::timeout(WAIT, self.events.recv())
            .await
            .expect("timed out waiting for an event")
            .expect("event channel closed")
    }

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

    async fn turn(&mut self) -> Vec<Event> {
        self.until(|e| matches!(e, Event::TurnFinished { .. }))
            .await
    }

    async fn turn_with(&mut self, text: &str) -> Vec<Event> {
        self.say(text).await;
        self.turn().await
    }

    /// Every event until the engine closes the channel.
    async fn until_closed(&mut self) -> Vec<Event> {
        let mut out = Vec::new();
        let closed = tokio::time::timeout(WAIT, async {
            while let Some(ev) = self.events.recv().await {
                out.push(ev);
            }
        })
        .await;
        assert!(closed.is_ok(), "the engine did not stop: {out:?}");
        out
    }
}

fn finish_reason(events: &[Event]) -> FinishReason {
    match events.last() {
        Some(Event::TurnFinished { reason }) => *reason,
        other => panic!("turn did not finish: {other:?}"),
    }
}

/// `(call_id, output, is_error)` of every `ToolFinished`, in order.
fn finished(events: &[Event]) -> Vec<(String, String, bool)> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::ToolFinished {
                call_id,
                output,
                is_error,
                ..
            } => Some((call_id.clone(), output.clone(), *is_error)),
            _ => None,
        })
        .collect()
}

fn warnings(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::Warning { message } => Some(message.clone()),
            _ => None,
        })
        .collect()
}

fn budget_warnings(events: &[Event]) -> Vec<String> {
    warnings(events)
        .into_iter()
        .filter(|w| w.contains("budget"))
        .collect()
}

fn records(engine: &Engine) -> Vec<SessionRecord> {
    let reader = SessionReader::<SessionRecord>::open(engine.session_path()).unwrap();
    reader.records().map(|r| r.unwrap().1).collect()
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

/// Whether a process whose whole command line matches `pattern` (anchored) is running.
fn running(pattern: &str) -> bool {
    std::process::Command::new("pgrep")
        .args(["-f", pattern])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

async fn wait_running(pattern: &str) {
    let end = Instant::now() + WAIT;
    while !running(pattern) {
        assert!(Instant::now() < end, "{pattern} never started");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

// ---- cancellation ----------------------------------------------------------------------

#[tokio::test]
async fn cancel_ends_turn_with_cancelled_and_kills_children_within_200ms() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![tools_step(vec![bash("c1", "sleep 300")], None)]);
    let mut run = start(fx.builder(config(false), &mock));
    run.say("go").await;
    run.until(|e| matches!(e, Event::ToolStarted { .. })).await;
    wait_running("^sleep 300$").await;

    let t0 = Instant::now();
    run.send(Command::Cancel).await;
    let events = run.turn().await;
    let took = t0.elapsed();
    eprintln!("TurnFinished {took:?} after Cancel");
    assert_eq!(finish_reason(&events), FinishReason::Cancelled);
    assert!(
        took < Duration::from_millis(250),
        "TurnFinished after {took:?}"
    );
    assert!(!running("^sleep 300$"), "the child survived the cancel");
    let done = finished(&events);
    assert_eq!(done.len(), 1, "{events:?}");
    assert!(done[0].1.contains("killed: cancelled"), "{}", done[0].1);
}

/// A sandbox whose reads of `hang.txt` never finish and ignore cancellation, and which
/// can make every spill after a write stall (also ignoring cancellation).
struct Hang {
    sb: LocalSandbox,
    stall_after_write: Option<Duration>,
    written: std::sync::atomic::AtomicBool,
}

impl Hang {
    fn new(sb: LocalSandbox) -> Self {
        Hang {
            sb,
            stall_after_write: None,
            written: Default::default(),
        }
    }
}

fn hangs(p: &Path) -> bool {
    p.file_name().is_some_and(|n| n == "hang.txt")
}

#[async_trait]
impl Sandbox for Hang {
    fn root(&self) -> &Path {
        self.sb.root()
    }
    fn resolve(&self, p: &Path) -> Result<PathBuf, SandboxError> {
        self.sb.resolve(p)
    }
    async fn read(&self, p: &Path) -> Result<Vec<u8>, SandboxError> {
        if hangs(p) {
            std::future::pending::<()>().await;
        }
        self.sb.read(p).await
    }
    async fn metadata(&self, p: &Path) -> Result<FileMeta, SandboxError> {
        if hangs(p) {
            std::future::pending::<()>().await;
        }
        self.sb.metadata(p).await
    }
    async fn write_atomic(
        &self,
        p: &Path,
        bytes: &[u8],
        mode: Option<u32>,
    ) -> Result<(), SandboxError> {
        if hangs(p) {
            std::future::pending::<()>().await;
        }
        let r = self.sb.write_atomic(p, bytes, mode).await;
        if r.is_ok() {
            self.written
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
        r
    }
    async fn remove_file(&self, p: &Path) -> Result<(), SandboxError> {
        self.sb.remove_file(p).await
    }
    async fn spill(&self, bytes: &[u8]) -> Result<PathBuf, SandboxError> {
        if let Some(d) = self.stall_after_write
            && self.written.load(std::sync::atomic::Ordering::SeqCst)
        {
            tokio::time::sleep(d).await;
        }
        self.sb.spill(bytes).await
    }
    async fn spawn(&self, spec: SpawnSpec) -> Result<Child, SandboxError> {
        self.sb.spawn(spec).await
    }
    fn walker(&self, base: &Path) -> Result<ignore::WalkBuilder, SandboxError> {
        self.sb.walker(base)
    }
    async fn http_get(&self, url: &Url) -> Result<HttpResponse, SandboxError> {
        self.sb.http_get(url).await
    }
}

#[tokio::test]
async fn a_call_that_ignores_cancel_is_abandoned_and_its_step_baselines_forgotten() {
    let fx = Fx::new();
    fx.write("a.txt", "alpha line\n");
    fx.write("hang.txt", "never read\n");
    let mock = MockProvider::new(vec![
        tools_step(
            vec![
                call("r1", "read", json!({"path": "a.txt"})),
                call("r2", "read", json!({"path": "hang.txt"})),
            ],
            None,
        ),
        tools_step(vec![call("r3", "read", json!({"path": "a.txt"}))], None),
        text_step("done", tokens(10, 1)),
    ]);
    let sb: Arc<dyn Sandbox> = Arc::new(Hang::new(LocalSandbox::new(fx.root()).unwrap()));
    let mut run = start(EngineBuilder {
        sandbox: Some(sb),
        ..fx.builder(config(false), &mock)
    });
    run.say("go").await;
    run.until(|e| matches!(e, Event::ToolStarted { call_id, .. } if call_id == "r2"))
        .await;
    // Let r1 finish (and record its baseline) while r2 hangs.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let t0 = Instant::now();
    run.send(Command::Cancel).await;
    let events = run.turn().await;
    let took = t0.elapsed();
    assert_eq!(finish_reason(&events), FinishReason::Cancelled);
    assert!(took < Duration::from_secs(1), "TurnFinished after {took:?}");
    let done = finished(&events);
    assert_eq!(done.len(), 2, "{events:?}");
    assert!(!done[0].2, "r1: {}", done[0].1);
    assert!(done[1].2);
    assert!(done[1].1.contains("did not stop"), "{}", done[1].1);

    // r1's baseline was recorded in the abandoned step, so it was dropped: the next read
    // of a.txt shows the content, not "unchanged since turn 1".
    run.turn_with("again").await;
    let results = tool_results(mock.requests().last().unwrap());
    let r3 = results.iter().find(|r| r.0 == "r3").expect("r3 result");
    assert!(r3.1.contains("alpha line"), "{}", r3.1);
}

#[tokio::test]
async fn a_write_cancelled_after_its_commit_is_awaited_and_its_edit_recorded() {
    let fx = Fx::new();
    fx.write("a.txt", "alpha\n");
    let mock = MockProvider::new(vec![tools_step(
        vec![call(
            "w1",
            "write",
            json!({"path": "a.txt", "content": "beta\n"}),
        )],
        None,
    )]);
    let sb: Arc<dyn Sandbox> = Arc::new(Hang {
        // Longer than the 150 ms an abandoned call gets.
        stall_after_write: Some(Duration::from_millis(600)),
        ..Hang::new(LocalSandbox::new(fx.root()).unwrap())
    });
    let mut run = start(EngineBuilder {
        sandbox: Some(sb),
        ..fx.builder(config(false), &mock)
    });
    run.say("go").await;
    run.until(|e| matches!(e, Event::ToolStarted { .. })).await;
    let end = Instant::now() + WAIT;
    while std::fs::read_to_string(fx.dir.path().join("a.txt")).unwrap() != "beta\n" {
        assert!(Instant::now() < end, "the write never landed");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // The file is committed; the tool is still busy after it.
    run.send(Command::Cancel).await;
    let events = run.turn().await;
    assert_eq!(finish_reason(&events), FinishReason::Cancelled);
    let done = finished(&events);
    assert!(
        !done[0].2,
        "the committed write must report its result: {:?}",
        done[0]
    );

    // The log knows the change and its pre-image, which is what undo restores from.
    let reader = SessionReader::<SessionRecord>::open(run.engine.session_path()).unwrap();
    let pre = reader
        .records()
        .filter_map(|r| match r.unwrap().1 {
            SessionRecord::Edit {
                path, pre_image, ..
            } if path.ends_with("a.txt") => Some(pre_image),
            _ => None,
        })
        .next()
        .expect("an Edit record for a.txt");
    let pre = pre.expect("a.txt existed before");
    assert_eq!(reader.read_blob(&pre).unwrap(), b"alpha\n");
}

#[tokio::test]
async fn a_write_stuck_past_its_bound_is_abandoned_and_reported() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![tools_step(
        vec![call(
            "w1",
            "write",
            json!({"path": "hang.txt", "content": "x"}),
        )],
        None,
    )]);
    let sb: Arc<dyn Sandbox> = Arc::new(Hang::new(LocalSandbox::new(fx.root()).unwrap()));
    let mut run = start(EngineBuilder {
        sandbox: Some(sb),
        ..fx.builder(config(false), &mock)
    });
    run.say("go").await;
    run.until(|e| matches!(e, Event::ToolStarted { .. })).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let t0 = Instant::now();
    run.send(Command::Cancel).await;
    let events = run.turn().await;
    let took = t0.elapsed();
    assert_eq!(finish_reason(&events), FinishReason::Cancelled);
    // Not the 150 ms other calls get: a write is waited for up to 10 s.
    assert!(took >= Duration::from_secs(10), "{took:?}");
    let done = finished(&events);
    assert!(
        done[0].2 && done[0].1.contains("undo cannot restore"),
        "{done:?}"
    );
    assert!(
        warnings(&events)
            .iter()
            .any(|w| w.contains("hang.txt may have changed")),
        "{events:?}"
    );
}

#[tokio::test]
async fn turn_timeout_finishes_with_timeout() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![MockTurn::Stall]);
    let cfg = Config {
        turn_timeout_s: Some(1),
        ..config(false)
    };
    let mut run = start(fx.builder(cfg, &mock));
    let t0 = Instant::now();
    let events = run.turn_with("go").await;
    let took = t0.elapsed();
    assert_eq!(finish_reason(&events), FinishReason::Timeout);
    assert!(
        took >= Duration::from_secs(1) && took < Duration::from_secs(5),
        "{took:?}"
    );
    let end = records(&run.engine)
        .into_iter()
        .rev()
        .find(|r| matches!(r, SessionRecord::TurnEnd { .. }));
    assert!(
        matches!(
            end,
            Some(SessionRecord::TurnEnd {
                reason: FinishReason::Timeout
            })
        ),
        "{end:?}"
    );
}

#[tokio::test]
async fn shutdown_kills_all_process_groups_and_session_is_resumable() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![
        // A background child that outlives its call (a dev server, say).
        tools_step(vec![bash("c1", "sleep 302 >/dev/null 2>&1 &")], None),
        tools_step(vec![bash("c2", "sleep 301")], None),
    ]);
    let mut run = start(fx.builder(config(false), &mock));
    run.say("go").await;
    run.until(|e| matches!(e, Event::ToolStarted { call_id, .. } if call_id == "c2"))
        .await;
    wait_running("^sleep 301$").await;
    assert!(running("^sleep 302$"), "the background child should run");

    run.send(Command::Shutdown).await;
    let events = run.until_closed().await;
    assert!(
        events.iter().any(|e| matches!(
            e,
            Event::TurnFinished {
                reason: FinishReason::Cancelled
            }
        )),
        "{events:?}"
    );
    assert!(!running("^sleep 301$"), "the running command survived");
    assert!(!running("^sleep 302$"), "the background child survived");

    let reader = SessionReader::<SessionRecord>::open(run.engine.session_path()).unwrap();
    assert!(!reader.truncated_tail());
    let last = reader.records().last().unwrap().unwrap().1;
    assert!(
        matches!(
            last,
            SessionRecord::TurnEnd {
                reason: FinishReason::Cancelled
            }
        ),
        "{last:?}"
    );
}

// ---- budgets ---------------------------------------------------------------------------

#[tokio::test]
async fn budget_warns_at_80_percent() {
    let fx = Fx::new();
    fx.write("a.txt", "alpha\n");
    let mut cfg = config(false);
    cfg.budget.turn = BudgetLimits {
        max_usd: None,
        max_tokens: Some(1000),
    };
    let mock = MockProvider::new(vec![
        tools_step(
            vec![call("r1", "read", json!({"path": "a.txt"}))],
            Some(tokens(700, 50)),
        ),
        text_step("done", tokens(80, 20)),
    ]);
    let mut run = start(fx.builder(cfg, &mock));
    let events = run.turn_with("go").await;
    assert_eq!(finish_reason(&events), FinishReason::Completed);
    let w = budget_warnings(&events);
    assert_eq!(w.len(), 1, "{w:?}");
    assert!(w[0].contains("turn budget 85% used"), "{}", w[0]);
    // Shown after the usage that crossed 80%, not after the first (75%).
    let usages: Vec<usize> = events
        .iter()
        .enumerate()
        .filter(|(_, e)| matches!(e, Event::Usage { .. }))
        .map(|(i, _)| i)
        .collect();
    let at = events
        .iter()
        .position(|e| matches!(e, Event::Warning { message } if message.contains("budget")))
        .unwrap();
    assert!(at > usages[1], "{events:?}");
}

#[tokio::test]
async fn budget_exceeded_finishes_inflight_tool_then_stops() {
    let fx = Fx::new();
    fx.write("a.txt", "alpha\n");
    let mock = MockProvider::new(vec![
        tools_step(
            vec![
                bash("c1", "sleep 0.4; echo finished"),
                call("c2", "read", json!({"path": "a.txt"})),
            ],
            Some(tokens(100, 10)),
        ),
        text_step("never sent", tokens(1, 1)),
    ]);
    let mut run = start(fx.builder(config(false), &mock));
    run.say("go").await;
    run.until(|e| matches!(e, Event::ToolStarted { call_id, .. } if call_id == "c1"))
        .await;
    // Lower the limit below what the turn has spent while c1 runs.
    run.send(Command::SetBudget {
        scope: BudgetScope::Turn,
        max_usd: None,
        max_tokens: Some(100),
    })
    .await;
    let events = run.turn().await;
    assert_eq!(finish_reason(&events), FinishReason::Budget);
    let done = finished(&events);
    assert_eq!(done.len(), 2, "{events:?}");
    assert!(
        !done[0].2 && done[0].1.contains("finished"),
        "{:?}",
        done[0]
    );
    assert!(done[1].2 && done[1].1.contains("budget"), "{:?}", done[1]);
    assert_eq!(
        mock.requests().len(),
        1,
        "no request after the budget ran out"
    );
    assert!(records(&run.engine).iter().any(|r| matches!(
        r,
        SessionRecord::ToolResult { id, is_error: false, .. } if id == "c1"
    )));
}

#[tokio::test]
async fn a_response_over_budget_runs_none_of_its_calls() {
    let fx = Fx::new();
    let mut cfg = config(false);
    cfg.budget.turn.max_tokens = Some(100);
    let mock = MockProvider::new(vec![
        tools_step(
            vec![call(
                "w1",
                "write",
                json!({"path": "o.txt", "content": "x"}),
            )],
            Some(tokens(100, 10)),
        ),
        text_step("never sent", tokens(1, 1)),
    ]);
    let mut run = start(fx.builder(cfg, &mock));
    let events = run.turn_with("go").await;
    assert_eq!(finish_reason(&events), FinishReason::Budget);
    assert!(!fx.dir.path().join("o.txt").exists());
    let done = finished(&events);
    assert!(done[0].2 && done[0].1.contains("budget"), "{done:?}");
    assert_eq!(mock.requests().len(), 1);
    assert!(
        budget_warnings(&events)
            .iter()
            .any(|w| w.contains("turn budget exhausted")),
        "{events:?}"
    );
}

#[tokio::test]
async fn an_exhausted_session_budget_stops_later_turns_until_raised() {
    let fx = Fx::new();
    let mut cfg = config(false);
    cfg.budget.session.max_tokens = Some(100);
    let mock = MockProvider::new(vec![
        text_step("one", tokens(100, 10)),
        text_step("two", tokens(1, 1)),
    ]);
    let mut run = start(fx.builder(cfg, &mock));
    // The response that crosses the limit still completes the turn.
    assert_eq!(
        finish_reason(&run.turn_with("first").await),
        FinishReason::Completed
    );
    let blocked = run.turn_with("second").await;
    assert_eq!(finish_reason(&blocked), FinishReason::Budget);
    assert_eq!(
        mock.requests().len(),
        1,
        "no request once the session is out"
    );

    run.send(Command::SetBudget {
        scope: BudgetScope::Session,
        max_usd: None,
        max_tokens: Some(10_000),
    })
    .await;
    assert_eq!(
        finish_reason(&run.turn_with("third").await),
        FinishReason::Completed
    );
    assert_eq!(mock.requests().len(), 2);
}

#[tokio::test]
async fn an_unpriced_model_with_a_usd_budget_warns_once_and_keeps_token_limits() {
    let fx = Fx::new();
    let mut cfg = config(false);
    cfg.budget.turn = BudgetLimits {
        max_usd: Some(0.01),
        max_tokens: Some(100),
    };
    let mock = MockProvider::new(vec![
        tools_step(
            vec![call(
                "w1",
                "write",
                json!({"path": "o.txt", "content": "x"}),
            )],
            Some(tokens(100, 10)),
        ),
        text_step("two", tokens(10, 1)),
    ]);
    let mut run = start(fx.builder(cfg, &mock));
    let first = run.turn_with("go").await;
    assert_eq!(finish_reason(&first), FinishReason::Budget);
    let second = run.turn_with("again").await;
    assert_eq!(finish_reason(&second), FinishReason::Completed);
    let priceless: Vec<String> = warnings(&first)
        .into_iter()
        .chain(warnings(&second))
        .filter(|w| w.contains("no known price"))
        .collect();
    assert_eq!(priceless.len(), 1, "{priceless:?}");
}

// ---- usage, cost and the ledger --------------------------------------------------------

#[tokio::test]
async fn usage_event_includes_cost_and_quota() {
    let fx = Fx::new();
    let usage = RawUsage {
        input: 1000,
        output: 200,
        cache_read: 5000,
        cache_write: 100,
    };
    let quota = Quota {
        remaining_requests: Some(42),
        remaining_tokens: Some(9000),
        reset_at: None,
    };
    let mock = MockProvider::new(vec![MockTurn::Events(vec![
        ProviderEvent::Quota(quota.clone()),
        ProviderEvent::TextDelta("hi".into()),
        ProviderEvent::Usage(usage),
        ProviderEvent::Done {
            stop: StopReason::EndTurn,
        },
    ])]);
    let mut run = start(fx.builder(config(true), &mock));
    let events = run.turn_with("go").await;
    // 1000 * 3 + 200 * 15 + 5000 * 0.3 + 100 * 3.75, per million.
    let expected = 0.007875;
    let got: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            Event::Usage {
                role,
                cost_usd,
                quota,
                tokens_in,
                cache_read,
                ..
            } => Some((*role, *cost_usd, quota.clone(), *tokens_in, *cache_read)),
            _ => None,
        })
        .collect();
    assert_eq!(got.len(), 1, "{events:?}");
    assert_eq!(got[0].0, UsageRole::Main);
    assert!(
        (got[0].1.unwrap() - expected).abs() < 1e-12,
        "{:?}",
        got[0].1
    );
    assert_eq!(got[0].2, Some(quota));
    assert_eq!((got[0].3, got[0].4), (1000, 5000));

    // The same response is in the ledger before the turn is reported finished.
    let t = fx.ledger().totals(UNIX_EPOCH).unwrap();
    assert_eq!(t.requests, 1);
    assert_eq!(t.tokens(), 6300);
    assert!((t.usd - expected).abs() < 1e-12, "{}", t.usd);
}

#[tokio::test]
async fn usage_of_a_discarded_attempt_counts_in_ledger_and_budget() {
    let fx = Fx::new();
    let mut cfg = config(true);
    cfg.budget.turn.max_tokens = Some(400);
    let mock = MockProvider::new(vec![
        // Breaks off after content: the engine sends the request again.
        MockTurn::DropAfter(vec![
            ProviderEvent::TextDelta("par".into()),
            ProviderEvent::Usage(tokens(100, 10)),
        ]),
        text_step("full", tokens(200, 20)),
    ]);
    let mut run = start(fx.builder(cfg, &mock));
    let events = run.turn_with("go").await;
    assert_eq!(finish_reason(&events), FinishReason::Completed);
    assert_eq!(mock.requests().len(), 2);
    let usages = events
        .iter()
        .filter(|e| matches!(e, Event::Usage { .. }))
        .count();
    assert_eq!(usages, 2, "{events:?}");
    // 330 of 400 tokens: only the sum of both attempts reaches 80%.
    let w = budget_warnings(&events);
    assert_eq!(w.len(), 1, "{w:?}");
    let t = fx.ledger().totals(UNIX_EPOCH).unwrap();
    assert_eq!((t.requests, t.tokens()), (2, 330));
}

/// What an Anthropic stream reports at `message_start`: the input, and one output token.
fn start_usage(input: u64) -> ProviderEvent {
    ProviderEvent::Usage(tokens(input, 1))
}

fn usage_events(events: &[Event]) -> Vec<(u64, u64, bool)> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::Usage {
                tokens_in,
                tokens_out,
                estimated,
                ..
            } => Some((*tokens_in, *tokens_out, *estimated)),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn usage_of_a_stream_broken_mid_way_is_estimated_and_counted() {
    let fx = Fx::new();
    let partial = "partial answer ".repeat(40);
    let mock = MockProvider::new(vec![
        // Shaped like a real Anthropic stream: usage at message_start, text, then a break.
        MockTurn::DropAfter(vec![start_usage(1000), ProviderEvent::TextDelta(partial)]),
        MockTurn::Events(vec![
            start_usage(1000),
            ProviderEvent::TextDelta("full".into()),
            ProviderEvent::Usage(tokens(1000, 20)),
            ProviderEvent::Done {
                stop: StopReason::EndTurn,
            },
        ]),
    ]);
    let mut run = start(fx.builder(config(true), &mock));
    let events = run.turn_with("go").await;
    assert_eq!(finish_reason(&events), FinishReason::Completed);
    let u = usage_events(&events);
    assert_eq!(u.len(), 2, "{events:?}");
    // The broken attempt: its reported input, and the output its streamed text is worth.
    assert_eq!(u[0].0, 1000);
    assert!(u[0].1 > 20, "{u:?}");
    assert!(u[0].2, "the broken attempt is marked estimated");
    assert_eq!(u[1], (1000, 20, false));
    let t = fx.ledger().totals(UNIX_EPOCH).unwrap();
    assert_eq!((t.requests, t.estimated), (2, 1));
    assert_eq!(t.input, 2000);
}

#[tokio::test]
async fn a_cancelled_request_counts_its_estimated_input() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![MockTurn::Stall]);
    let mut run = start(fx.builder(config(true), &mock));
    run.say("go").await;
    run.until(|e| matches!(e, Event::TurnStarted)).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    run.send(Command::Cancel).await;
    let events = run.turn().await;
    assert_eq!(finish_reason(&events), FinishReason::Cancelled);
    let u = usage_events(&events);
    assert_eq!(u.len(), 1, "{events:?}");
    assert!(u[0].0 > 0 && u[0].2, "{u:?}");
    let t = fx.ledger().totals(UNIX_EPOCH).unwrap();
    assert_eq!((t.requests, t.estimated), (1, 1));
}

#[tokio::test]
async fn usage_of_an_attempt_the_retry_wrapper_discarded_is_counted() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![
        // Input reported, then a transient failure before any content: retried inside.
        MockTurn::DropAfter(vec![start_usage(700)]),
        text_step("ok", tokens(300, 5)),
    ]);
    let mut run = start(fx.builder(config(true), &mock));
    let events = run.turn_with("go").await;
    assert_eq!(finish_reason(&events), FinishReason::Completed);
    assert_eq!(mock.requests().len(), 2);
    let t = fx.ledger().totals(UNIX_EPOCH).unwrap();
    assert_eq!((t.requests, t.input, t.estimated), (2, 1000, 0));
    // The calibrated context status comes from the completed response only.
    assert_eq!(usage_events(&events).last(), Some(&(300, 5, false)));
}

#[tokio::test]
async fn an_unopenable_ledger_is_reported_once_and_the_session_runs() {
    let fx = Fx::new();
    // A directory where the file should be: nothing to recover.
    std::fs::create_dir(fx.home.path().join("ledger.sqlite")).unwrap();
    let mock = MockProvider::new(vec![
        text_step("one", tokens(1, 1)),
        text_step("two", tokens(1, 1)),
    ]);
    let mut run = start(fx.builder(config(true), &mock));
    let first = run.turn_with("go").await;
    let second = run.turn_with("again").await;
    assert_eq!(finish_reason(&first), FinishReason::Completed);
    assert_eq!(finish_reason(&second), FinishReason::Completed);
    let about_ledger: Vec<String> = warnings(&first)
        .into_iter()
        .chain(warnings(&second))
        .filter(|w| w.contains("ledger"))
        .collect();
    assert_eq!(about_ledger.len(), 1, "{about_ledger:?}");
}

#[tokio::test]
async fn a_corrupt_ledger_is_moved_aside_with_one_warning_and_recording_goes_on() {
    let fx = Fx::new();
    std::fs::write(
        fx.home.path().join("ledger.sqlite"),
        "this is not a database at all, really",
    )
    .unwrap();
    let mock = MockProvider::new(vec![
        text_step("one", tokens(1, 1)),
        text_step("two", tokens(1, 1)),
    ]);
    let mut run = start(fx.builder(config(true), &mock));
    let first = run.turn_with("go").await;
    let second = run.turn_with("again").await;
    let about_ledger: Vec<String> = warnings(&first)
        .into_iter()
        .chain(warnings(&second))
        .filter(|w| w.contains("ledger"))
        .collect();
    assert_eq!(about_ledger.len(), 1, "{about_ledger:?}");
    assert!(about_ledger[0].contains("corrupt"), "{about_ledger:?}");
    assert_eq!(fx.ledger().totals(UNIX_EPOCH).unwrap().requests, 2);
}

#[tokio::test]
async fn a_locked_ledger_does_not_delay_the_session_and_its_rows_arrive_later() {
    let fx = Fx::new();
    let path = fx.home.path().join("ledger.sqlite");
    // Another process holds the (rollback-journal) file exclusively while we start.
    let holder = rusqlite::Connection::open(&path).unwrap();
    holder
        .execute_batch("CREATE TABLE other (x); BEGIN EXCLUSIVE; INSERT INTO other VALUES (1);")
        .unwrap();
    let mock = MockProvider::new(vec![text_step("one", tokens(10, 2))]);
    let t0 = Instant::now();
    let mut run = start(fx.builder(config(true), &mock));
    let events = run.turn_with("go").await;
    assert_eq!(finish_reason(&events), FinishReason::Completed);
    assert!(t0.elapsed() < Duration::from_secs(3), "{:?}", t0.elapsed());
    holder.execute_batch("COMMIT;").unwrap();
    drop(holder);
    // The buffered row is written once the ledger opens.
    let end = Instant::now() + WAIT;
    loop {
        if let Ok(l) = Ledger::open(&path)
            && l.totals(UNIX_EPOCH).unwrap().requests == 1
        {
            break;
        }
        assert!(Instant::now() < end, "the buffered row never arrived");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// ---- signals ---------------------------------------------------------------------------

/// A router whose escalation is recorded instead of ending the process.
fn router() -> (Arc<SignalRouter>, Arc<std::sync::Mutex<Vec<Signal>>>) {
    let escalated = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = Arc::clone(&escalated);
    let router = SignalRouter::new(move |sig| seen.lock().unwrap().push(sig));
    (router, escalated)
}

#[tokio::test]
async fn an_interrupt_cancels_the_turn_a_second_shuts_down_and_a_third_escalates() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![MockTurn::Stall]);
    let mut run = start(fx.builder(config(false), &mock));
    let (router, escalated) = router();
    router.add(&run.engine);
    run.say("go").await;
    run.until(|e| matches!(e, Event::TurnStarted)).await;
    router.deliver(Signal::Interrupt).await;
    assert_eq!(finish_reason(&run.turn().await), FinishReason::Cancelled);
    router.deliver(Signal::Interrupt).await;
    assert!(escalated.lock().unwrap().is_empty());
    // Shutting down (or already gone): the next one ends the process.
    router.deliver(Signal::Interrupt).await;
    assert_eq!(*escalated.lock().unwrap(), [Signal::Interrupt]);
    run.until_closed().await;
    assert!(run.engine.send(Command::Cancel).await.is_err());
}

#[tokio::test]
async fn sigterm_shuts_every_session_down() {
    let fx = Fx::new();
    let other = Fx::new();
    let mut a = start(fx.builder(config(false), &MockProvider::new(vec![])));
    let mut b = start(other.builder(config(false), &MockProvider::new(vec![])));
    let (router, escalated) = router();
    router.add(&a.engine);
    router.add(&b.engine);
    router.deliver(Signal::Terminate).await;
    a.until_closed().await;
    b.until_closed().await;
    assert!(escalated.lock().unwrap().is_empty());
}

#[tokio::test]
async fn with_no_session_alive_a_signal_escalates_and_the_router_keeps_none_alive() {
    let fx = Fx::new();
    let run = start(fx.builder(config(false), &MockProvider::new(vec![])));
    let (router, escalated) = router();
    router.add(&run.engine);
    let Run { engine, mut events } = run;
    drop(engine);
    let closed = tokio::time::timeout(WAIT, async { while events.recv().await.is_some() {} }).await;
    assert!(closed.is_ok(), "the router kept the engine running");
    router.deliver(Signal::Hangup).await;
    assert_eq!(*escalated.lock().unwrap(), [Signal::Hangup]);
}

#[tokio::test]
async fn handle_signals_feeds_the_router() {
    let fx = Fx::new();
    let mut run = start(fx.builder(config(false), &MockProvider::new(vec![])));
    let (router, _) = router();
    router.add(&run.engine);
    let (tx, rx) = mpsc::unbounded_channel();
    let signals =
        futures::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|s| (s, rx)) });
    let listener = handle_signals(&router, signals);
    tx.send(Signal::Terminate).unwrap();
    run.until_closed().await;
    drop(tx);
    tokio::time::timeout(WAIT, listener).await.unwrap().unwrap();
}

/// Child half of the real-signal tests: run only when the parent asks (by env var).
#[test]
#[ignore = "run by real_signals_end_the_process_after_shutdown in a child process"]
fn signal_child() {
    let Ok(mode) = std::env::var("TAUTLY_SIGNAL_CHILD") else {
        return;
    };
    let sig = match mode.as_str() {
        "int" => libc::SIGINT,
        "term" => libc::SIGTERM,
        _ => libc::SIGHUP,
    };
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let fx = Fx::new();
        let mock = MockProvider::new(vec![MockTurn::Stall]);
        let mut run = start(fx.builder(config(false), &mock));
        install_signal_handlers(&run.engine).unwrap();
        // SAFETY (all raises): delivers a signal to this process, which handles it.
        if sig == libc::SIGINT {
            run.say("go").await;
            run.until(|e| matches!(e, Event::TurnStarted)).await;
            assert_eq!(unsafe { libc::raise(sig) }, 0);
            assert_eq!(finish_reason(&run.turn().await), FinishReason::Cancelled);
            println!("CANCELLED");
        }
        assert_eq!(unsafe { libc::raise(sig) }, 0);
        run.until_closed().await;
        println!("CLOSED");
        assert_eq!(unsafe { libc::raise(sig) }, 0);
        tokio::time::sleep(Duration::from_secs(10)).await;
    });
    println!("STILL ALIVE");
}

#[test]
fn real_signals_end_the_process_after_shutdown() {
    use std::os::unix::process::ExitStatusExt;
    let exe = std::env::current_exe().unwrap();
    for (mode, sig) in [
        ("int", libc::SIGINT),
        ("term", libc::SIGTERM),
        ("hup", libc::SIGHUP),
    ] {
        let out = std::process::Command::new(&exe)
            .args(["--ignored", "--exact", "signal_child", "--nocapture"])
            .env("TAUTLY_SIGNAL_CHILD", mode)
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("CLOSED"),
            "{mode}: {stdout}\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(!stdout.contains("STILL ALIVE"), "{mode}: {stdout}");
        if mode == "int" {
            assert!(stdout.contains("CANCELLED"), "{stdout}");
        }
        // Killed by the signal: a shell reports 128 + n (130 for SIGINT, 143 for SIGTERM).
        assert_eq!(out.status.signal(), Some(sig), "{mode}: {:?}", out.status);
    }
}
