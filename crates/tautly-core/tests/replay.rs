//! Playback (spec 7.12.5, Task 51): a session's logged stream re-emitted from its log, in
//! order and paced by its timestamps, with no provider request, no tool run and no
//! approval that can be answered (Review Focus 13).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tautly_core::record::ApprovalKind;
use tautly_core::replay::{self, REPLAYED_ID_PREFIX};
use tautly_core::{Command, Config, EngineBuilder, Event, SessionRecord, UsageRole};
use tautly_provider::{
    Catalog, MockProvider, MockTurn, ModelId, ProviderEvent, RawUsage, StopReason,
};
use tautly_sandbox::{
    Child, FileMeta, HttpResponse, LocalSandbox, Mode, Sandbox, SandboxError, SpawnSpec, Url,
};
use tautly_session::{BlobRef, HeaderInit, SessionReader, SessionWriter};
use tautly_tools::ToolMeta;
use tempfile::TempDir;
use tokio::sync::mpsc;

const MODEL: &str = "mock/test";
const SIDE: &str = "mock/side";
const WAIT: Duration = Duration::from_secs(30);

// ------------------------------------------------------------------------ fixtures

struct Fx {
    dir: TempDir,
    home: TempDir,
}

impl Fx {
    /// A work tree that is not a git repository (no checkpoints, so nothing spawns git).
    fn new() -> Fx {
        let fx = Fx {
            dir: tempfile::tempdir().unwrap(),
            home: tempfile::tempdir().unwrap(),
        };
        std::fs::write(fx.root().join("a.txt"), "zero\n").unwrap();
        fx
    }

    fn root(&self) -> PathBuf {
        self.dir.path().canonicalize().unwrap()
    }

    fn home(&self) -> &Path {
        self.home.path()
    }

    fn catalog() -> Catalog {
        let entry = |id: &str, input: f64| {
            format!(
                r#"[models."{id}"]
context_window = 200000
max_output = 8000
parallel_tools = true
reasoning = "none"
cache = "none"
images = false
edit_format = "str_replace"
[models."{id}".price]
input = {input}
output = 15.0
cache_read = 0.3
cache_write = 3.75
"#
            )
        };
        Catalog::from_toml(&(entry(MODEL, 3.0) + &entry(SIDE, 1.0))).unwrap()
    }

    fn config(mode: Mode) -> Config {
        let mut cfg = Config {
            model: ModelId::from(MODEL),
            catalog_overrides: Some(Fx::catalog()),
            ..Config::default()
        };
        cfg.permissions.mode = mode;
        cfg
    }

    fn builder(&self, mock: &MockProvider, mode: Mode, interactive: bool) -> EngineBuilder {
        EngineBuilder {
            provider: Some(Arc::new(mock.clone())),
            data_home: Some(self.home().to_path_buf()),
            interactive,
            ..EngineBuilder::new(Fx::config(mode), self.dir.path())
        }
    }

    /// A session log of this work tree holding `recs`, the `i`th written at `t[i]`
    /// (Unix milliseconds); its id. Records named in `unknown` are rewritten as a record
    /// type from a newer version.
    fn craft(&self, recs: &[(u64, SessionRecord)], unknown: &[usize]) -> (String, PathBuf) {
        let mut w = SessionWriter::<SessionRecord>::create_in(
            self.home(),
            &self.root(),
            HeaderInit {
                model: Some(MODEL.into()),
                ..HeaderInit::default()
            },
        )
        .unwrap();
        for (_, r) in recs {
            w.append(r).unwrap();
        }
        w.checkpoint().unwrap();
        let (id, path) = (w.session_id().to_string(), w.path().to_path_buf());
        drop(w);
        let text = std::fs::read_to_string(&path).unwrap();
        let mut lines: Vec<String> = Vec::new();
        for (i, line) in text.lines().enumerate() {
            if i == 0 {
                lines.push(line.to_string());
                continue;
            }
            let mut v: Value = serde_json::from_str(line).unwrap();
            v["t"] = json!(recs[i - 1].0);
            if unknown.contains(&(i - 1)) {
                v["data"] = json!({"type": "from_the_future", "data": {"x": 1}});
            }
            lines.push(v.to_string());
        }
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        (id, path)
    }

    fn blob(&self, path: &Path, bytes: &[u8]) -> BlobRef {
        let w = SessionWriter::<SessionRecord>::reopen(path).unwrap();
        w.blob(bytes).unwrap()
    }
}

/// Counts what the tools would run.
struct Counting {
    inner: LocalSandbox,
    spawns: Arc<AtomicUsize>,
}

#[async_trait]
impl Sandbox for Counting {
    fn root(&self) -> &Path {
        self.inner.root()
    }
    fn resolve(&self, p: &Path) -> Result<PathBuf, SandboxError> {
        self.inner.resolve(p)
    }
    async fn read(&self, p: &Path) -> Result<Vec<u8>, SandboxError> {
        self.inner.read(p).await
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
        self.inner.write_atomic(p, bytes, mode).await
    }
    async fn remove_file(&self, p: &Path) -> Result<(), SandboxError> {
        self.inner.remove_file(p).await
    }
    async fn spill(&self, bytes: &[u8]) -> Result<PathBuf, SandboxError> {
        self.inner.spill(bytes).await
    }
    async fn spawn(&self, spec: SpawnSpec) -> Result<Child, SandboxError> {
        self.spawns.fetch_add(1, Ordering::SeqCst);
        self.inner.spawn(spec).await
    }
    fn walker(&self, base: &Path) -> Result<ignore::WalkBuilder, SandboxError> {
        self.inner.walker(base)
    }
    async fn http_get(&self, url: &Url) -> Result<HttpResponse, SandboxError> {
        self.inner.http_get(url).await
    }
}

struct Run {
    engine: tautly_core::Engine,
    events: mpsc::Receiver<Event>,
}

impl Run {
    fn start(b: EngineBuilder) -> Run {
        let (engine, events) = b.build().unwrap();
        Run { engine, events }
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

    /// Events until one `until` accepts (included).
    async fn until(&mut self, until: impl Fn(&Event) -> bool) -> Vec<Event> {
        let mut seen = Vec::new();
        loop {
            let ev = self.next().await;
            let done = until(&ev);
            seen.push(ev);
            if done {
                return seen;
            }
        }
    }

    /// Wait until the session has started (a command answered between turns).
    async fn settle(&mut self) {
        self.send(Command::Diff {
            id: None,
            turn: None,
        })
        .await;
        self.until(|e| matches!(e, Event::SessionDiff { .. })).await;
    }

    /// Replay `session` and collect everything up to its `ReplayFinished` (included).
    async fn replay(&mut self, session: &str, speed: f32, from_step: Option<u32>) -> Vec<Event> {
        self.send(Command::Replay {
            id: None,
            session_id: Some(session.into()),
            speed,
            from_step,
        })
        .await;
        self.until(|e| matches!(e, Event::ReplayFinished { .. }))
            .await
    }

    async fn stop(mut self) {
        self.send(Command::Shutdown).await;
        while tokio::time::timeout(WAIT, self.events.recv())
            .await
            .expect("the engine did not stop")
            .is_some()
        {}
    }
}

/// The events inside the `Replayed` ones, in order.
fn inner(seen: &[Event]) -> Vec<Event> {
    seen.iter()
        .filter_map(|e| match e {
            Event::Replayed { event, .. } => Some((**event).clone()),
            _ => None,
        })
        .collect()
}

fn finished(seen: &[Event]) -> (u32, bool) {
    match seen.last() {
        Some(Event::ReplayFinished {
            events, cancelled, ..
        }) => (*events, *cancelled),
        other => panic!("no ReplayFinished at the end: {other:?}"),
    }
}

fn call(id: &str, name: &str, args: Value) -> ProviderEvent {
    ProviderEvent::ToolCall {
        id: id.into(),
        name: name.into(),
        input_json: args.to_string(),
    }
}

fn usage(input: u64, output: u64) -> ProviderEvent {
    ProviderEvent::Usage(RawUsage {
        input,
        output,
        cache_read: 0,
        cache_write: 0,
    })
}

fn done(stop: StopReason) -> ProviderEvent {
    ProviderEvent::Done { stop }
}

/// One turn: reasoning and text streamed in pieces, a write, a command (whose response
/// reports no usage, so it is charged as an estimate), then text.
fn golden_script() -> MockProvider {
    MockProvider::new(vec![
        MockTurn::Events(vec![
            ProviderEvent::ReasoningDelta("think ".into()),
            ProviderEvent::ReasoningDelta("hard".into()),
            ProviderEvent::TextDelta("let me ".into()),
            ProviderEvent::TextDelta("write".into()),
            call("w1", "write", json!({"path": "a.txt", "content": "one\n"})),
            usage(100, 20),
            done(StopReason::ToolUse),
        ]),
        MockTurn::Events(vec![
            call("b1", "bash", json!({"command": "echo hi"})),
            done(StopReason::ToolUse),
        ]),
        MockTurn::Events(vec![
            ProviderEvent::TextDelta("done".into()),
            usage(300, 5),
            done(StopReason::EndTurn),
        ]),
    ])
}

/// Run the golden turn in an asking, interactive session, approving every call; the
/// session id and its live events.
async fn golden_session(fx: &Fx) -> (String, Vec<Event>) {
    let mock = golden_script();
    let mut run = Run::start(fx.builder(&mock, Mode::Ask, true));
    run.send(Command::SendMessage {
        id: None,
        text: "do it".into(),
    })
    .await;
    let mut live = Vec::new();
    loop {
        let ev = run.next().await;
        if let Event::ToolApprovalRequested { request_id, .. } = &ev {
            run.send(Command::Approve {
                request_id: request_id.clone(),
                session_rule: None,
            })
            .await;
        }
        let end = matches!(ev, Event::TurnFinished { .. });
        live.push(ev);
        if end {
            break;
        }
    }
    let id = run.engine.session_id().to_string();
    run.stop().await;
    (id, live)
}

/// The events a replay reproduces, with consecutive deltas merged (the log merges them)
/// and the diffs it cannot show left out; replayed request ids lose their prefix.
fn comparable(events: &[Event]) -> Vec<Event> {
    let mut out: Vec<Event> = Vec::new();
    for e in events {
        let e = match e.clone() {
            e @ (Event::TurnStarted
            | Event::ToolCallProposed { .. }
            | Event::Usage { .. }
            | Event::Compacted { .. }
            | Event::ContextCollected { .. }
            | Event::MemoryProposed { .. }
            | Event::ModelChanged { .. }
            | Event::TurnFinished { .. }) => e,
            Event::ToolApprovalRequested {
                request_id,
                call_id,
                tool,
                args,
                reason,
                ..
            } => Event::ToolApprovalRequested {
                request_id: request_id
                    .strip_prefix(REPLAYED_ID_PREFIX)
                    .unwrap_or(&request_id)
                    .to_string(),
                call_id,
                tool,
                args,
                reason,
                diff: None,
            },
            Event::ToolFinished {
                call_id,
                tool,
                output,
                is_error,
                blocked,
                effects,
                meta,
                ..
            } => Event::ToolFinished {
                call_id,
                tool,
                output,
                is_error,
                blocked,
                effects,
                meta,
                diff: None,
            },
            Event::TextDelta { text } => match out.last_mut() {
                Some(Event::TextDelta { text: t }) => {
                    t.push_str(&text);
                    continue;
                }
                _ => Event::TextDelta { text },
            },
            Event::ReasoningDelta { text } => match out.last_mut() {
                Some(Event::ReasoningDelta { text: t }) => {
                    t.push_str(&text);
                    continue;
                }
                _ => Event::ReasoningDelta { text },
            },
            _ => continue,
        };
        out.push(e);
    }
    out
}

// --------------------------------------------------------------------------- tests

#[tokio::test]
async fn replay_reemits_logged_stream_in_order() {
    let fx = Fx::new();
    let (id, live) = golden_session(&fx).await;
    let mut run = Run::start(fx.builder(&MockProvider::new(vec![]), Mode::Ask, true));
    let seen = run.replay(&id, 0.0, None).await;
    let replayed = inner(&seen);
    assert_eq!(
        replayed.first(),
        Some(&Event::UserMessage {
            text: "do it".into()
        }),
        "{replayed:#?}"
    );
    assert_eq!(replayed[1], Event::TurnStarted);
    let want = comparable(&live);
    assert_eq!(comparable(&replayed), want);
    // The golden stream is not trivially empty: what it covers is all there.
    assert!(
        want.iter()
            .any(|e| matches!(e, Event::ToolApprovalRequested { .. }))
    );
    assert!(
        want.iter()
            .any(|e| matches!(e, Event::ReasoningDelta { .. }))
    );
    assert!(want.iter().any(|e| matches!(
        e,
        Event::Usage {
            estimated: true,
            cost_usd: Some(_),
            ..
        }
    )));
    assert_eq!(finished(&seen), (replayed.len() as u32, false));
    // Replayed approval ids can never name a live request.
    for e in &replayed {
        if let Event::ToolApprovalRequested { request_id, .. } = e {
            assert!(request_id.starts_with(REPLAYED_ID_PREFIX), "{request_id}");
        }
    }
    run.stop().await;
}

#[tokio::test]
async fn replay_makes_no_provider_requests_or_spawns() {
    let fx = Fx::new();
    let (id, _) = golden_session(&fx).await;
    // A human edit since: a replay must not write the logged content back.
    std::fs::write(fx.root().join("a.txt"), "human\n").unwrap();
    let mock = MockProvider::new(vec![]);
    let spawns = Arc::new(AtomicUsize::new(0));
    let sandbox: Arc<dyn Sandbox> = Arc::new(Counting {
        inner: LocalSandbox::new(fx.root()).unwrap(),
        spawns: Arc::clone(&spawns),
    });
    let mut run = Run::start(EngineBuilder {
        sandbox: Some(sandbox),
        ..fx.builder(&mock, Mode::Yolo, false)
    });
    run.settle().await;
    let before = spawns.load(Ordering::SeqCst);
    let seen = run.replay(&id, 0.0, None).await;
    let replayed = inner(&seen);
    assert!(
        replayed
            .iter()
            .any(|e| matches!(e, Event::ToolFinished { .. }))
    );
    assert_eq!(
        spawns.load(Ordering::SeqCst),
        before,
        "a replay ran a command"
    );
    assert!(
        mock.requests().is_empty(),
        "a replay sent a provider request"
    );
    assert_eq!(
        std::fs::read_to_string(fx.root().join("a.txt")).unwrap(),
        "human\n"
    );
    // Nothing outside `Replayed` but the end: no live tool call, no live turn.
    for e in &seen[..seen.len() - 1] {
        assert!(
            matches!(e, Event::Replayed { .. } | Event::Warning { .. }),
            "{e:?}"
        );
    }
    run.stop().await;
}

/// TurnStart (t 1000: UserMessage, TurnStarted), two texts (1400, 1600), TurnEnd (9400).
fn paced_log(fx: &Fx) -> String {
    let recs = vec![
        (
            1000,
            SessionRecord::TurnStart {
                turn: 1,
                text: "go".into(),
            },
        ),
        (
            1400,
            SessionRecord::Provider(ProviderEvent::TextDelta("a".into())),
        ),
        (
            1600,
            SessionRecord::Provider(ProviderEvent::TextDelta("b".into())),
        ),
        (
            9400,
            SessionRecord::TurnEnd {
                reason: tautly_core::FinishReason::Completed,
                evidence: None,
            },
        ),
    ];
    fx.craft(&recs, &[]).0
}

#[tokio::test]
async fn replay_paces_by_timestamps_scaled_and_capped_at_2s() {
    // The pure rule.
    assert_eq!(replay::delay(1000, 1400, 2.0), Duration::from_millis(200));
    assert_eq!(replay::delay(1000, 1000, 2.0), Duration::ZERO);
    assert_eq!(replay::delay(1600, 9400, 2.0), Duration::from_secs(2));
    assert_eq!(replay::delay(1000, 1400, 0.0), Duration::ZERO);
    // Out of order (a clock step back) is no pause; a tiny speed is still capped.
    assert_eq!(replay::delay(1400, 1000, 1.0), Duration::ZERO);
    assert_eq!(replay::delay(0, 1, 1e-30), Duration::from_secs(2));

    // And the engine, on a paused clock.
    let fx = Fx::new();
    let id = paced_log(&fx);
    let mut run = Run::start(fx.builder(&MockProvider::new(vec![]), Mode::Yolo, false));
    run.settle().await;
    tokio::time::pause();
    run.send(Command::Replay {
        id: None,
        session_id: Some(id),
        speed: 2.0,
        from_step: None,
    })
    .await;
    let mut at = Vec::new();
    loop {
        // The clock is moved by hand, a millisecond at a time: the workspace's background
        // threads (blocking tasks) keep tokio from advancing a paused clock by itself.
        let ev = tokio::select! {
            biased;
            ev = run.events.recv() => ev.unwrap(),
            () = tokio::time::advance(Duration::from_millis(1)) => continue,
        };
        match ev {
            Event::Replayed { .. } => at.push(tokio::time::Instant::now()),
            Event::ReplayFinished { .. } => break,
            _ => {}
        }
    }
    let gaps: Vec<u64> = at
        .windows(2)
        .map(|w| (w[1] - w[0]).as_millis() as u64)
        .collect();
    let want = [0u64, 200, 100, 2000];
    assert_eq!(gaps.len(), want.len(), "{gaps:?}");
    for (got, want) in gaps.iter().zip(want) {
        // Never early; late only by the hand-moved clock steps the engine takes to run.
        assert!(*got >= want && *got - want <= 10, "{gaps:?}");
    }
    tokio::time::resume();
    run.stop().await;
}

#[tokio::test]
async fn speed_zero_is_instant() {
    let fx = Fx::new();
    let recs = vec![
        (
            1_000,
            SessionRecord::TurnStart {
                turn: 1,
                text: "go".into(),
            },
        ),
        (
            600_000,
            SessionRecord::Provider(ProviderEvent::TextDelta("a".into())),
        ),
        (
            3_600_000,
            SessionRecord::TurnEnd {
                reason: tautly_core::FinishReason::Completed,
                evidence: None,
            },
        ),
    ];
    let (id, _) = fx.craft(&recs, &[]);
    let mut run = Run::start(fx.builder(&MockProvider::new(vec![]), Mode::Yolo, false));
    run.settle().await;
    let begin = std::time::Instant::now();
    let seen = run.replay(&id, 0.0, None).await;
    assert!(
        begin.elapsed() < Duration::from_secs(5),
        "{:?}",
        begin.elapsed()
    );
    assert_eq!(finished(&seen), (4, false));
    run.stop().await;
}

/// Two turns: 1 user "one", 2 text "first"; 3 user "two", 4 text "second".
fn two_turns(fx: &Fx) -> (String, PathBuf) {
    let request = |turn: u32, step: u32| SessionRecord::Request {
        hash: BlobRef::parse(&"a".repeat(64)).unwrap(),
        turn,
        step,
        model: ModelId::from(MODEL),
        blocks: tautly_core::record::BlockHashes {
            stable: "s".into(),
            pinned: "p".into(),
            history: "h".into(),
            tail: "t".into(),
        },
    };
    let end = || SessionRecord::TurnEnd {
        reason: tautly_core::FinishReason::Completed,
        evidence: None,
    };
    let recs = vec![
        (
            1,
            SessionRecord::TurnStart {
                turn: 1,
                text: "one".into(),
            },
        ),
        (2, request(1, 2)),
        (
            3,
            SessionRecord::Provider(ProviderEvent::TextDelta("first".into())),
        ),
        (4, end()),
        (
            5,
            SessionRecord::TurnStart {
                turn: 2,
                text: "two".into(),
            },
        ),
        (6, request(2, 4)),
        (
            7,
            SessionRecord::Provider(ProviderEvent::TextDelta("second".into())),
        ),
        (8, end()),
    ];
    fx.craft(&recs, &[])
}

#[tokio::test]
async fn from_step_skips_earlier_steps() {
    let fx = Fx::new();
    let (id, path) = two_turns(&fx);
    let reader = SessionReader::<SessionRecord>::open(&path).unwrap();
    let texts = |from: Option<u32>| -> Vec<Event> {
        replay::events(&reader, from).map(|(_, _, e)| e).collect()
    };
    let all = texts(None);
    assert_eq!(all.len(), 8, "{all:#?}");
    assert_eq!(
        texts(Some(3)),
        [
            Event::UserMessage { text: "two".into() },
            Event::TurnStarted,
            Event::TextDelta {
                text: "second".into()
            },
            Event::TurnFinished {
                id: None,
                reason: tautly_core::FinishReason::Completed,
                evidence: None
            },
        ]
    );
    // A provider step starts at its request: its user message is before it.
    assert_eq!(texts(Some(4)), all[6..]);
    // Offsets are the records'.
    let offsets: Vec<u64> = replay::events(&reader, Some(3)).map(|(o, ..)| o).collect();
    assert_eq!(offsets, [4, 4, 6, 7]);

    // The engine: the same, and a step past the last is refused.
    let mut run = Run::start(fx.builder(&MockProvider::new(vec![]), Mode::Yolo, false));
    run.settle().await;
    let seen = run.replay(&id, 0.0, Some(3)).await;
    assert_eq!(inner(&seen), texts(Some(3)));
    let seen = run.replay(&id, 0.0, Some(9)).await;
    assert!(
        seen.iter().any(|e| matches!(e, Event::Error { message, .. }
            if message.contains("no step 9") && message.contains("last step is 4"))),
        "{seen:#?}"
    );
    assert_eq!(finished(&seen), (0, false));
    run.stop().await;
}

#[tokio::test]
async fn cancel_stops_with_finished_cancelled() {
    let fx = Fx::new();
    let recs: Vec<(u64, SessionRecord)> = (0..6)
        .map(|i| {
            (
                i * 10_000,
                SessionRecord::Provider(ProviderEvent::TextDelta(format!("t{i}"))),
            )
        })
        .collect();
    let (id, _) = fx.craft(&recs, &[]);
    let mut run = Run::start(fx.builder(&MockProvider::new(vec![]), Mode::Yolo, false));
    run.settle().await;
    run.send(Command::Replay {
        id: None,
        session_id: Some(id.clone()),
        speed: 1.0,
        from_step: None,
    })
    .await;
    run.until(|e| matches!(e, Event::Replayed { .. })).await;
    let begin = std::time::Instant::now();
    run.send(Command::Cancel).await;
    let seen = run
        .until(|e| matches!(e, Event::ReplayFinished { .. }))
        .await;
    assert!(
        begin.elapsed() < Duration::from_secs(5),
        "{:?}",
        begin.elapsed()
    );
    let Some(Event::ReplayFinished {
        id: None,
        session_id,
        events,
        cancelled,
    }) = seen.last()
    else {
        unreachable!()
    };
    assert_eq!(session_id, &id);
    assert!(*cancelled);
    assert!(*events < 6, "{events}");
    // The engine takes commands again.
    run.settle().await;
    run.stop().await;
}

#[tokio::test]
async fn replay_during_turn_errors() {
    let fx = Fx::new();
    let (id, _) = two_turns(&fx);
    let mock = MockProvider::new(vec![MockTurn::Stall]);
    let mut run = Run::start(fx.builder(&mock, Mode::Yolo, false));
    run.send(Command::SendMessage {
        id: None,
        text: "hi".into(),
    })
    .await;
    run.until(|e| matches!(e, Event::TurnStarted)).await;
    run.send(Command::Replay {
        id: None,
        session_id: Some(id),
        speed: 0.0,
        from_step: None,
    })
    .await;
    let seen = run
        .until(|e| matches!(e, Event::Error { .. } | Event::Replayed { .. }))
        .await;
    let Some(Event::Error { kind, message, .. }) = seen.last() else {
        panic!("{seen:#?}")
    };
    assert_eq!(*kind, tautly_core::ErrorKind::Internal);
    assert_eq!(message, "replay needs an idle session");
    run.send(Command::Cancel).await;
    let rest = run.until(|e| matches!(e, Event::TurnFinished { .. })).await;
    assert!(!rest.iter().any(|e| matches!(e, Event::Replayed { .. })));
    run.stop().await;
}

#[tokio::test]
async fn replayed_request_ids_not_answerable() {
    let fx = Fx::new();
    let (id, path) = fx.craft(&[], &[]);
    let result_blob = fx.blob(&path, b"wrote a.txt");
    // Append a call that waited for approval, by hand (the log holds only data).
    let mut w = SessionWriter::<SessionRecord>::reopen(&path).unwrap();
    for r in [
        SessionRecord::TurnStart {
            turn: 1,
            text: "go".into(),
        },
        SessionRecord::ToolCall {
            id: "w1".into(),
            name: "write".into(),
            args: json!({"path": "a.txt", "content": "pwned\n"}),
        },
        SessionRecord::ApprovalRequested {
            request_id: "approval-1".into(),
            kind: ApprovalKind::Tool,
            call_id: Some("w1".into()),
            reason: "writes a file".into(),
        },
        SessionRecord::ToolResult {
            id: "w1".into(),
            result_blob,
            is_error: false,
            effects: vec![],
            meta: ToolMeta::default(),
            ran: true,
            blocked: false,
        },
    ] {
        w.append(&r).unwrap();
    }
    w.checkpoint().unwrap();
    drop(w);

    let mock = MockProvider::new(vec![]);
    let mut run = Run::start(fx.builder(&mock, Mode::Ask, true));
    run.settle().await;
    let seen = run.replay(&id, 0.0, None).await;
    let replayed = inner(&seen);
    let Some(Event::ToolApprovalRequested {
        request_id, tool, ..
    }) = replayed
        .iter()
        .find(|e| matches!(e, Event::ToolApprovalRequested { .. }))
    else {
        panic!("{replayed:#?}")
    };
    assert_eq!(tool, "write");
    assert_eq!(request_id, &format!("{REPLAYED_ID_PREFIX}approval-1"));
    for rid in [request_id.clone(), "approval-1".to_string()] {
        run.send(Command::Approve {
            request_id: rid.clone(),
            session_rule: None,
        })
        .await;
        let seen = run.until(|e| matches!(e, Event::Warning { .. })).await;
        let Some(Event::Warning { message }) = seen.last() else {
            unreachable!()
        };
        assert!(message.contains("no approval request"), "{message}");
    }
    assert!(mock.requests().is_empty());
    assert_eq!(
        std::fs::read_to_string(fx.root().join("a.txt")).unwrap(),
        "zero\n"
    );
    run.stop().await;
}

#[test]
fn unknown_records_are_skipped_with_one_note() {
    let fx = Fx::new();
    let text = |s: &str| SessionRecord::Provider(ProviderEvent::TextDelta(s.into()));
    let recs = vec![
        (1, text("a")),
        (2, SessionRecord::PinsFolded),
        (3, text("b")),
        (4, SessionRecord::PinsFolded),
    ];
    let (_, path) = fx.craft(&recs, &[1, 3]);
    let reader = SessionReader::<SessionRecord>::open(&path).unwrap();
    let got: Vec<Event> = replay::events(&reader, None).map(|(_, _, e)| e).collect();
    assert_eq!(
        got,
        [
            Event::TextDelta { text: "a".into() },
            Event::TextDelta { text: "b".into() },
            Event::Warning {
                message: "2 records from a newer version skipped".into()
            },
        ]
    );
}

#[test]
fn record_paths_escaping_the_root_are_refused() {
    let fx = Fx::new();
    let (_, path) = fx.craft(&[], &[]);
    let blob = fx.blob(&path, b"ok");
    let effect = |p: &str| {
        serde_json::from_value(json!({"type": "deleted", "path": p,
            "pre_image_blob": "b".repeat(64), "pre_mode": 420}))
        .unwrap()
    };
    let mut w = SessionWriter::<SessionRecord>::reopen(&path).unwrap();
    w.append(&SessionRecord::ToolResult {
        id: "c1".into(),
        result_blob: blob,
        is_error: false,
        effects: vec![
            effect("../outside.txt"),
            effect("/etc/passwd"),
            effect("ok.txt"),
        ],
        meta: ToolMeta::default(),
        ran: true,
        blocked: false,
    })
    .unwrap();
    w.checkpoint().unwrap();
    drop(w);
    let reader = SessionReader::<SessionRecord>::open(&path).unwrap();
    let got: Vec<Event> = replay::events(&reader, None).map(|(_, _, e)| e).collect();
    let warnings = got
        .iter()
        .filter(|e| matches!(e, Event::Warning { .. }))
        .count();
    assert_eq!(warnings, 2, "{got:#?}");
    let Some(Event::ToolFinished {
        effects, output, ..
    }) = got.last()
    else {
        panic!("{got:#?}")
    };
    assert_eq!(output, "ok");
    assert_eq!(
        effects,
        &[tautly_core::EffectView::Deleted {
            path: fx.root().join("ok.txt")
        }]
    );
}

#[test]
fn usage_is_priced_by_its_own_model_and_estimates_are_marked() {
    let fx = Fx::new();
    let request = SessionRecord::Request {
        hash: BlobRef::parse(&"a".repeat(64)).unwrap(),
        turn: 1,
        step: 2,
        model: ModelId::from(MODEL),
        blocks: tautly_core::record::BlockHashes {
            stable: "s".into(),
            pinned: "p".into(),
            history: "h".into(),
            tail: "t".into(),
        },
    };
    let raw = |input| RawUsage {
        input,
        output: 0,
        cache_read: 0,
        cache_write: 0,
    };
    let recs = vec![
        (1, request),
        (2, SessionRecord::Provider(ProviderEvent::Usage(raw(10)))),
        // Cumulative: the response's last usage is what it cost.
        (
            3,
            SessionRecord::Provider(ProviderEvent::Usage(raw(1_000_000))),
        ),
        (4, SessionRecord::Provider(done(StopReason::EndTurn))),
        (
            5,
            SessionRecord::Charged {
                role: UsageRole::Side,
                model: ModelId::from(SIDE),
                usage: raw(1_000_000),
                estimated: true,
            },
        ),
        (
            6,
            SessionRecord::Charged {
                role: UsageRole::Side,
                model: ModelId::from("mock/unpriced"),
                usage: raw(5),
                estimated: false,
            },
        ),
    ];
    let (_, path) = fx.craft(&recs, &[]);
    let reader = SessionReader::<SessionRecord>::open(&path).unwrap();
    let got: Vec<(UsageRole, u64, Option<f64>, bool)> = replay::events(&reader, None)
        .priced(Fx::catalog())
        .filter_map(|(_, _, e)| match e {
            Event::Usage {
                role,
                tokens_in,
                cost_usd,
                estimated,
                ..
            } => Some((role, tokens_in, cost_usd, estimated)),
            _ => None,
        })
        .collect();
    assert_eq!(
        got,
        [
            (UsageRole::Main, 1_000_000, Some(3.0), false),
            (UsageRole::Side, 1_000_000, Some(1.0), true),
            (UsageRole::Side, 5, None, false),
        ]
    );
    // Without a catalog nothing is priced.
    let unpriced: Vec<Option<f64>> = replay::events(&reader, None)
        .filter_map(|(_, _, e)| match e {
            Event::Usage { cost_usd, .. } => Some(cost_usd),
            _ => None,
        })
        .collect();
    assert_eq!(unpriced, [None, None, None]);
}

// ------------------------------------------------------------------- fix round 1

/// `n` text records `t0`.., `gap` ms apart.
fn texts(n: u64, gap: u64) -> Vec<(u64, SessionRecord)> {
    (0..n)
        .map(|i| {
            (
                i * gap,
                SessionRecord::Provider(ProviderEvent::TextDelta(format!("t{i}"))),
            )
        })
        .collect()
}

#[tokio::test]
async fn replay_streams_with_bounded_memory() {
    // Many results naming one large blob: what a replay holds must not grow with them,
    // and the first event goes out before the log has been read to its end.
    let fx = Fx::new();
    let (id, path) = fx.craft(&[], &[]);
    let blob = fx.blob(&path, &vec![b'x'; 25_000]);
    const N: u32 = 2_000;
    let mut w = SessionWriter::<SessionRecord>::reopen(&path).unwrap();
    for i in 0..N {
        w.append(&SessionRecord::ToolResult {
            id: format!("c{i}"),
            result_blob: blob.clone(),
            is_error: false,
            effects: vec![],
            meta: ToolMeta::default(),
            ran: true,
            blocked: false,
        })
        .unwrap();
    }
    w.checkpoint().unwrap();
    drop(w);
    let mut run = Run::start(fx.builder(&MockProvider::new(vec![]), Mode::Yolo, false));
    run.settle().await;
    run.send(Command::Replay {
        id: None,
        session_id: Some(id),
        speed: 0.0,
        from_step: None,
    })
    .await;
    run.until(|e| matches!(e, Event::Replayed { .. })).await;
    // The blob goes away now: results not read yet can no longer find it. Had the whole
    // log been read before the first event, every output would be whole.
    let (head, rest) = blob.as_str().split_at(2);
    std::fs::remove_file(path.parent().unwrap().join("blobs").join(head).join(rest)).unwrap();
    let seen = run
        .until(|e| matches!(e, Event::ReplayFinished { .. }))
        .await;
    let outputs: Vec<String> = inner(&seen)
        .into_iter()
        .filter_map(|e| match e {
            Event::ToolFinished { output, .. } => Some(output),
            _ => None,
        })
        .collect();
    let missing = outputs
        .iter()
        .filter(|o| o.starts_with("[the output is not in the session's blob store"))
        .count();
    assert!(
        missing > 0,
        "the log was read to its end before the first event"
    );
    // At most the event channel, the reader's queue and a few in flight were read ahead.
    let read_ahead = outputs.len() - missing;
    let bound = tautly_core::engine::EVENT_CAPACITY + replay::STREAM_BOUND + 8;
    assert!(read_ahead <= bound, "{read_ahead} read ahead");
    assert_eq!(finished(&seen), (N, false));
    run.stop().await;
}

#[test]
fn compaction_collection_memory_and_model_rows_are_mapped() {
    let fx = Fx::new();
    let (_, path) = fx.craft(&[], &[]);
    let removed = fx.blob(&path, b"{}");
    let change = tautly_core::MemoryChange {
        upsert: None,
        delete: vec!["old".into()],
    };
    let mut w = SessionWriter::<SessionRecord>::reopen(&path).unwrap();
    for r in [
        SessionRecord::Compaction {
            removed: vec![removed],
            summary: "sum".into(),
        },
        SessionRecord::GcApplied {
            elided: vec![("c1".into(), 3)],
            reason: tautly_core::MutationReason::ColdCache,
            tokens_freed: 50,
            forgotten_paths: vec![],
        },
        SessionRecord::MemoryProposal {
            id: "mem-1".into(),
            change: change.clone(),
        },
        SessionRecord::ModelSet {
            model: ModelId::from("openai/gpt-5"),
        },
    ] {
        w.append(&r).unwrap();
    }
    w.checkpoint().unwrap();
    drop(w);
    let reader = SessionReader::<SessionRecord>::open(&path).unwrap();
    let got: Vec<Event> = replay::events(&reader, None).map(|(_, _, e)| e).collect();
    assert_eq!(
        got,
        [
            Event::Compacted {
                removed_turns: 1,
                summary: "sum".into(),
                reason: tautly_core::MutationReason::Unknown,
            },
            Event::ContextCollected {
                elided_results: 1,
                tokens_freed: 50,
                reason: tautly_core::MutationReason::ColdCache,
            },
            Event::MemoryProposed {
                proposal_id: format!("{REPLAYED_ID_PREFIX}mem-1"),
                change,
                replaces: vec![],
            },
            Event::ModelChanged {
                id: None,
                model: ModelId::from("openai/gpt-5"),
                provider: "openai".into(),
            },
        ]
    );
}

#[tokio::test]
async fn replayed_memory_proposals_cannot_be_approved() {
    let fx = Fx::new();
    let (id, path) = fx.craft(&[], &[]);
    let mut w = SessionWriter::<SessionRecord>::reopen(&path).unwrap();
    w.append(&SessionRecord::MemoryProposal {
        id: "mem-1".into(),
        change: tautly_core::MemoryChange {
            upsert: None,
            delete: vec!["old".into()],
        },
    })
    .unwrap();
    w.checkpoint().unwrap();
    drop(w);
    let mut run = Run::start(fx.builder(&MockProvider::new(vec![]), Mode::Ask, true));
    run.settle().await;
    let seen = run.replay(&id, 0.0, None).await;
    let Some(Event::MemoryProposed { proposal_id, .. }) = inner(&seen).first().cloned() else {
        panic!("{seen:#?}")
    };
    assert_eq!(proposal_id, format!("{REPLAYED_ID_PREFIX}mem-1"));
    for pid in [proposal_id, "mem-1".to_string()] {
        run.send(Command::ApproveMemory {
            proposal_id: pid.clone(),
        })
        .await;
        let seen = run.until(|e| matches!(e, Event::Warning { .. })).await;
        let Some(Event::Warning { message }) = seen.last() else {
            unreachable!()
        };
        assert_eq!(message, &format!("no memory proposal `{pid}` is pending"));
    }
    run.stop().await;
}

#[tokio::test]
async fn shutdown_during_a_replay_finishes_it_cancelled() {
    let fx = Fx::new();
    let (id, _) = fx.craft(&texts(6, 10_000), &[]);
    let mut run = Run::start(fx.builder(&MockProvider::new(vec![]), Mode::Yolo, false));
    run.settle().await;
    run.send(Command::Replay {
        id: None,
        session_id: Some(id),
        speed: 1.0,
        from_step: None,
    })
    .await;
    run.until(|e| matches!(e, Event::Replayed { .. })).await;
    run.send(Command::Shutdown).await;
    let mut rest = Vec::new();
    while let Some(ev) = tokio::time::timeout(WAIT, run.events.recv())
        .await
        .expect("the engine did not stop")
    {
        rest.push(ev);
    }
    assert!(
        rest.iter().any(|e| matches!(
            e,
            Event::ReplayFinished {
                cancelled: true,
                ..
            }
        )),
        "{rest:#?}"
    );
}

#[tokio::test]
async fn a_malformed_line_mid_log_ends_the_replay_with_a_warning() {
    let fx = Fx::new();
    let (id, path) = fx.craft(&texts(3, 1), &[]);
    // Break the second record's line (the header is line 1).
    let text = std::fs::read_to_string(&path).unwrap();
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    lines[2] = "{this is not json".into();
    std::fs::write(&path, lines.join("\n") + "\n").unwrap();

    let reader = SessionReader::<SessionRecord>::open(&path).unwrap();
    let mut events = replay::events(&reader, None);
    let got: Vec<Event> = events.by_ref().map(|(_, _, e)| e).collect();
    assert_eq!(got.len(), 2, "{got:#?}");
    assert_eq!(got[0], Event::TextDelta { text: "t0".into() });
    assert!(
        matches!(&got[1], Event::Warning { message } if message.contains("cannot be read past")),
        "{got:#?}"
    );
    assert!(events.truncated());

    let mut run = Run::start(fx.builder(&MockProvider::new(vec![]), Mode::Yolo, false));
    run.settle().await;
    let seen = run.replay(&id, 0.0, None).await;
    assert_eq!(inner(&seen), got);
    assert_eq!(finished(&seen), (2, false));
    run.stop().await;
}

#[tokio::test]
async fn replay_during_a_fork_is_refused_not_held() {
    let fx = Fx::new();
    let (id, _) = two_turns(&fx);
    let mut run = Run::start(fx.builder(&MockProvider::new(vec![]), Mode::Yolo, false));
    run.settle().await;
    // Sent back to back: the replay arrives while the fork runs.
    run.send(Command::Fork {
        id: None,
        at: None,
        label: None,
    })
    .await;
    run.send(Command::Replay {
        id: None,
        session_id: Some(id),
        speed: 0.0,
        from_step: None,
    })
    .await;
    let seen = run
        .until(|e| {
            matches!(e, Event::Error { message, .. } if message == replay::REPLAY_BUSY)
                || matches!(e, Event::ReplayFinished { .. })
        })
        .await;
    assert!(
        matches!(seen.last(), Some(Event::Error { .. })),
        "{seen:#?}"
    );
    assert!(!seen.iter().any(|e| matches!(e, Event::Replayed { .. })));
    run.stop().await;
}
