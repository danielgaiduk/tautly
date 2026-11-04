//! Resume, fork, undo and compaction wiring (spec 6.4, 6.5, 7.12.1) against a scripted
//! provider (`MockProvider`) and a real `LocalSandbox` on a temporary directory.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, UNIX_EPOCH};

use serde_json::{Value, json};
use tautly_core::record::UndoneFile;
use tautly_core::resume::{self, Rebuilt, UndoError};
use tautly_core::{
    BudgetScope, Command, Config, Engine, EngineBuilder, Event, FinishReason, Ledger,
    SessionRecord, Workspace,
};
use tautly_provider::{
    CanonicalRequest, Catalog, Content, MockProvider, MockTurn, ModelId, ProviderError,
    ProviderEvent, RawUsage, StopReason, ToolSpec,
};
use tautly_sandbox::{LocalSandbox, Mode, Sandbox};
use tautly_session::{Origin, SessionReader};
use tempfile::TempDir;
use tokio::sync::mpsc;

const MODEL: &str = "mock/test";
const WAIT: Duration = Duration::from_secs(30);

fn catalog(window: u32, max_output: u32, edit_format: &str) -> Catalog {
    Catalog::from_toml(&format!(
        r#"[models."{MODEL}"]
context_window = {window}
max_output = {max_output}
parallel_tools = true
reasoning = "none"
cache = "none"
images = false
edit_format = "{edit_format}"
"#
    ))
    .unwrap()
}

fn config(mode: Mode) -> Config {
    let mut c = Config {
        model: ModelId::from(MODEL),
        catalog_overrides: Some(catalog(200_000, 8_000, "str_replace")),
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

    fn read(&self, rel: &str) -> Option<String> {
        std::fs::read_to_string(self.dir.path().join(rel)).ok()
    }

    fn builder(&self, cfg: Config, mock: &MockProvider) -> EngineBuilder {
        EngineBuilder {
            provider: Some(Arc::new(mock.clone())),
            data_home: Some(self.home.path().to_path_buf()),
            ..EngineBuilder::new(cfg, self.dir.path())
        }
    }

    fn resume(&self, cfg: Config, mock: &MockProvider, id: &str) -> EngineBuilder {
        EngineBuilder {
            resume: Some(id.to_string()),
            ..self.builder(cfg, mock)
        }
    }

    fn sandbox(&self) -> LocalSandbox {
        LocalSandbox::new(self.root()).unwrap()
    }

    /// A workspace over this root whose index has finished its first refresh.
    async fn ready_workspace(&self) -> Arc<Workspace> {
        let sb: Arc<dyn Sandbox> = Arc::new(self.sandbox());
        let ws = Workspace::new(sb, self.home.path());
        ws.start();
        assert!(ws.wait_ready(WAIT).await, "index never became ready");
        ws
    }

    fn ledger(&self) -> Ledger {
        Ledger::open(&self.home.path().join("ledger.sqlite")).unwrap()
    }

    /// Waits for the background writer to have written `n` rows in all.
    async fn ledger_rows(&self, n: u64) {
        let end = Instant::now() + WAIT;
        loop {
            if self
                .ledger()
                .totals(UNIX_EPOCH)
                .is_ok_and(|t| t.requests >= n)
            {
                return;
            }
            assert!(Instant::now() < end, "the ledger never reached {n} rows");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

/// Make the cost ledger unusable: a directory where its file should be.
fn break_ledger(fx: &Fx) {
    let ledger = fx.home.path().join("ledger.sqlite");
    std::fs::remove_file(&ledger).unwrap();
    for ext in ["-wal", "-shm"] {
        let mut p = ledger.clone().into_os_string();
        p.push(ext);
        let _ = std::fs::remove_file(PathBuf::from(p));
    }
    std::fs::create_dir(&ledger).unwrap();
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
    async fn send(&self, cmd: Command) {
        self.engine.send(cmd).await.unwrap();
    }

    async fn say(&self, text: &str) {
        self.send(Command::SendMessage { text: text.into() }).await;
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

    async fn turn_with(&mut self, text: &str) -> Vec<Event> {
        self.say(text).await;
        self.until(|e| matches!(e, Event::TurnFinished { .. }))
            .await
    }

    /// The `SessionChanged` of a fork.
    async fn fork(&mut self, at: Option<u32>, label: &str) -> String {
        self.send(Command::Fork {
            at,
            label: Some(label.into()),
        })
        .await;
        let events = self
            .until(|e| matches!(e, Event::SessionChanged { .. } | Event::Error { .. }))
            .await;
        match events.last() {
            Some(Event::SessionChanged { session_id }) => session_id.clone(),
            other => panic!("the fork failed: {other:?}"),
        }
    }

    /// Shut down and wait until the engine has stopped (its log is closed).
    async fn stop(self) {
        self.stop_events().await;
    }

    /// Shut down; the events until the stream closed.
    async fn stop_events(mut self) -> Vec<Event> {
        self.send(Command::Shutdown).await;
        let mut out = Vec::new();
        let closed = tokio::time::timeout(WAIT, async {
            while let Some(ev) = self.events.recv().await {
                out.push(ev);
            }
        })
        .await;
        assert!(closed.is_ok(), "the engine did not stop");
        out
    }
}

fn finish_reason(events: &[Event]) -> FinishReason {
    match events.last() {
        Some(Event::TurnFinished { reason, .. }) => *reason,
        other => panic!("turn did not finish: {other:?}"),
    }
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

fn records_at(path: &Path) -> Vec<SessionRecord> {
    let reader = SessionReader::<SessionRecord>::open(path).unwrap();
    reader.records().map(|r| r.unwrap().1).collect()
}

/// The log of session `id`, next to `sibling` (another log of the project).
fn log_of(sibling: &Path, id: &str) -> PathBuf {
    sibling.with_file_name(format!("{id}.jsonl"))
}

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

fn text_of(req: &CanonicalRequest) -> String {
    serde_json::to_string(&req.messages).unwrap()
}

/// The data and time of every record line of the log at `path`, raw.
fn raw_records(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .skip(1)
        .map(|l| {
            let v: Value = serde_json::from_str(l).unwrap();
            json!({"t": v["t"], "data": v["data"]})
        })
        .collect()
}

/// What the engine assembles a request with for the test model.
fn assembly() -> (tautly_provider::Capabilities, Vec<ToolSpec>) {
    let caps = catalog(200_000, 8_000, "str_replace")
        .get(&ModelId::from(MODEL))
        .caps;
    let specs = tautly_tools::registry(&caps, Vec::new())
        .iter()
        .map(|t| t.spec(&caps))
        .collect();
    (caps, specs)
}

fn assemble(rebuilt: &Rebuilt) -> CanonicalRequest {
    let (caps, specs) = assembly();
    rebuilt
        .context
        .assemble(&caps, &specs, &ModelId::from(MODEL))
        .0
}

fn rebuild_at(path: &Path, until: Option<u32>) -> Rebuilt {
    let reader = SessionReader::<SessionRecord>::open(path).unwrap();
    resume::rebuild(&reader, until).unwrap()
}

// ------------------------------------------------------------------------------ resume

/// Turn A of the identical-rebuild test: GC, the files list, a model pin, a failed call.
fn turn_a_script() -> Vec<MockTurn> {
    vec![
        tools_step(vec![call("r0", "read", json!({"path": "big.txt"}))]),
        tools_step(vec![call(
            "r1",
            "read",
            json!({"path": "big.txt", "full": true}),
        )]),
        tools_step(vec![call(
            "w1",
            "write",
            json!({"path": "out.txt", "content": "written\n"}),
        )]),
        tools_step(vec![
            call(
                "p1",
                "pin",
                json!({"text": "MODEL-PIN keep the API stable"}),
            ),
            call("m1", "read", json!({"path": "missing.txt"})),
        ]),
        text_step("done A"),
    ]
}

async fn run_turn_a(run: &mut Run) {
    let events = run.turn_with("turn A").await;
    assert_eq!(
        finish_reason(&events),
        FinishReason::Completed,
        "{events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::ContextCollected { .. })),
        "the scenario must collect: {events:?}"
    );
    run.send(Command::Pin {
        text: "USER-PIN no new dependencies".into(),
    })
    .await;
}

#[tokio::test]
async fn resume_rebuilds_context_identically() {
    let fx = Fx::new();
    let big: String = (0..400)
        .map(|i| format!("line {i}: some filler text to make this file large\n"))
        .collect();
    fx.write("big.txt", &big);
    fx.write(
        "src/lib.rs",
        "pub fn alpha() {}\npub fn beta() { alpha() }\n",
    );
    fx.write(
        "AGENTS.md",
        "# Project\nORIGINAL-AGENTS text.\n\n## Rules\n- ORIGINAL-RULE never push\n",
    );
    fx.write(
        ".tautly/memory/use-pnpm.md",
        "---\nid: use-pnpm\ntitle: Use pnpm\nkind: convention\ncreated: 2026-01-01T00:00:00Z\n---\nAlways pnpm.\n",
    );

    // The session to resume: turn A, then it stops.
    let mock = MockProvider::new(turn_a_script());
    let mut run = start(EngineBuilder {
        workspace: Some(fx.ready_workspace().await),
        ..fx.builder(config(Mode::AutoEdit), &mock)
    });
    run_turn_a(&mut run).await;
    let id = run.engine.session_id().to_string();
    run.stop().await;
    std::fs::remove_file(fx.dir.path().join("out.txt")).unwrap();

    // The same conversation live: turn A, then turn B.
    let mut script = turn_a_script();
    script.push(text_step("done B"));
    let live_mock = MockProvider::new(script);
    let mut live = start(EngineBuilder {
        workspace: Some(fx.ready_workspace().await),
        ..fx.builder(config(Mode::AutoEdit), &live_mock)
    });
    run_turn_a(&mut live).await;
    live.turn_with("turn B").await;
    // (Stopping adds the session's episode request.)
    let expected = live_mock.requests().last().unwrap().clone();
    live.stop().await;
    let prefix = serde_json::to_string(&expected.system).unwrap();
    assert!(prefix.contains("ORIGINAL-AGENTS"), "{prefix}");
    assert!(prefix.contains("Use pnpm"), "the memory index: {prefix}");
    assert!(prefix.contains("alpha"), "the repo map: {prefix}");
    let state = text_of(&expected);
    assert!(state.contains("[AGENTS.md] - ORIGINAL-RULE"), "{state}");
    assert!(state.contains("out.txt"), "the files list: {state}");
    assert!(state.contains("[elided: superseded at turn"), "{state}");
    assert!(state.contains("MODEL-PIN") && state.contains("USER-PIN"));
    assert!(state.contains("missing.txt"), "the failed attempt: {state}");

    // AGENTS.md and the repo map change on disk; the resumed session keeps the logged ones.
    fx.write(
        "AGENTS.md",
        "# Project\nCHANGED-AGENTS text.\n\n## Rules\n- CHANGED-RULE\n",
    );
    fx.write("src/fresh_module.rs", "pub fn fresh_function() {}\n");
    let resumed_mock = MockProvider::new(vec![text_step("done B")]);
    let mut resumed = start(EngineBuilder {
        workspace: Some(fx.ready_workspace().await),
        ..fx.resume(config(Mode::AutoEdit), &resumed_mock, &id)
    });
    let events = resumed.turn_with("turn B").await;
    assert_eq!(finish_reason(&events), FinishReason::Completed);
    let got = resumed_mock.requests().last().unwrap().clone();
    assert!(!text_of(&got).contains("CHANGED"));
    assert!(
        !serde_json::to_string(&got.system)
            .unwrap()
            .contains("fresh_module")
    );
    assert_eq!(got.system, expected.system);
    assert_eq!(got.messages, expected.messages);
    assert_eq!(got, expected);
}

/// Run `f` on its own runtime and drop that runtime afterwards, killing every task the
/// engine had: a crash, with nothing written after the last record that reached the file.
fn on_crashing_runtime<T>(f: impl std::future::Future<Output = T>) -> T {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let out = rt.block_on(f);
    rt.shutdown_timeout(Duration::from_secs(2));
    out
}

#[test]
fn resume_after_crash_mid_turn_marks_incomplete_and_continues_on_next_message() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![tools_step(vec![call(
        "w1",
        "write",
        json!({"path": "o.txt", "content": "x"}),
    )])]);
    let run = on_crashing_runtime(async {
        let mut run = start(EngineBuilder {
            interactive: true,
            ..fx.builder(config(Mode::Ask), &mock)
        });
        run.say("first message").await;
        run.until(|e| matches!(e, Event::ToolApprovalRequested { .. }))
            .await;
        run
    });
    let id = run.engine.session_id().to_string();
    let path = run.engine.session_path().to_path_buf();
    drop(run);
    let crashed = records_at(&path);
    assert!(
        !crashed
            .iter()
            .any(|r| matches!(r, SessionRecord::TurnEnd { .. })),
        "{crashed:?}"
    );
    let rebuilt = rebuild_at(&path, None);
    assert!(rebuilt.incomplete_turn);
    assert_eq!(rebuilt.turn_count, 1);

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let mock = MockProvider::new(vec![text_step("continued")]);
        let mut run = start(EngineBuilder {
            interactive: true,
            ..fx.resume(config(Mode::Ask), &mock, &id)
        });
        let events = run.turn_with("second message").await;
        assert_eq!(finish_reason(&events), FinishReason::Completed);
        let req = mock.requests().last().unwrap().clone();
        let text = text_of(&req);
        assert!(text.contains("first message") && text.contains("second message"));
        // The open call has a synthetic cancelled result; nothing ran.
        let results = tool_results(&req);
        assert_eq!(results.len(), 1, "{results:?}");
        assert_eq!(results[0].0, "w1");
        assert!(
            results[0].2 && results[0].1.contains("cancelled"),
            "{results:?}"
        );
        assert!(fx.read("o.txt").is_none());
        run.stop().await;
    });

    // The missing TurnEnd was written first, before the next message.
    let recs = records_at(&path);
    let n = crashed.len();
    assert!(
        matches!(
            recs[n],
            SessionRecord::TurnEnd {
                reason: FinishReason::Error,
                ..
            }
        ),
        "{:?}",
        &recs[n..]
    );
    assert!(recs[n + 1..].iter().any(|r| matches!(
        r,
        SessionRecord::Command(Command::SendMessage { text }) if text == "second message"
    )));
    let after = rebuild_at(&path, None);
    assert!(!after.incomplete_turn);
    assert_eq!(after.turn_count, 2);
}

#[tokio::test]
async fn resume_restores_budget_spend_and_set_budget() {
    let fx = Fx::new();
    let cfg = || config(Mode::Ask);
    let mock = MockProvider::new((0..3).map(|_| text_step("ok")).collect());
    let mut run = start(fx.builder(cfg(), &mock));
    run.send(Command::SetBudget {
        scope: BudgetScope::Session,
        max_usd: None,
        max_tokens: Some(500),
    })
    .await;
    for i in 0..3 {
        run.turn_with(&format!("turn {i}")).await;
    }
    let id = run.engine.session_id().to_string();
    run.stop().await;
    // 3 x 110 tokens.
    fx.ledger_rows(3).await;
    assert_eq!(fx.ledger().session_totals(&id).unwrap().tokens(), 330);

    // Resumed: 330 spent before, 110 more is past 80% of the logged 500-token limit.
    let mock = MockProvider::new(vec![text_step("ok")]);
    let mut run = start(fx.resume(cfg(), &mock, &id));
    let events = run.turn_with("turn 3").await;
    assert_eq!(finish_reason(&events), FinishReason::Completed);
    let w = warnings(&events);
    assert!(
        w.iter().any(|w| w.contains("session budget 88% used")),
        "{w:?}"
    );
    run.stop().await;
    fx.ledger_rows(4).await;

    // Without the ledger the spend comes from the log's usage records, with a warning.
    break_ledger(&fx);
    let mock = MockProvider::new(vec![text_step("ok"), text_step("ok")]);
    let mut run = start(fx.resume(cfg(), &mock, &id));
    // 440 spent: this turn's 110 uses up the 500, so the next one sends nothing.
    let events = run.turn_with("turn 4").await;
    assert_eq!(finish_reason(&events), FinishReason::Completed);
    let w = warnings(&events);
    assert!(w.iter().any(|w| w.contains("session log")), "{w:?}");
    assert!(w.iter().any(|w| w.contains("exhausted")), "{w:?}");
    let events = run.turn_with("turn 5").await;
    assert_eq!(finish_reason(&events), FinishReason::Budget, "{events:?}");
    assert_eq!(mock.requests().len(), 1);
    run.stop().await;
}

/// Review C M11: a cut at step N leaves out an undo done after it, as `state_at(N)` (the
/// files before that undo) does: the rebuilt context is told nothing is stale.
#[tokio::test]
async fn a_cut_before_a_later_undo_leaves_the_undo_out() {
    let fx = Fx::new();
    let mut cfg = config(Mode::Ask);
    cfg.permissions.mode = Mode::AutoEdit;
    let mock = MockProvider::new(vec![
        tools_step(vec![call(
            "w1",
            "write",
            json!({"path": "a.txt", "content": "one\n"}),
        )]),
        text_step("written"),
    ]);
    let mut run = start(fx.builder(cfg, &mock));
    run.turn_with("write it").await;
    let path = run.engine.session_path().to_path_buf();
    let before = rebuild_at(&path, None);
    let last = before.context.next_turn() - 1;
    run.send(Command::Undo {
        turn: Some(1),
        dry_run: false,
    })
    .await;
    run.until(|e| matches!(e, Event::Undone { .. } | Event::Error { .. }))
        .await;
    run.stop().await;
    assert!(
        records_at(&path)
            .iter()
            .any(|r| matches!(r, SessionRecord::Undone { .. }))
    );
    let stale = |r: &Rebuilt| r.context.state.files.values().any(|n| n.maybe_stale);
    assert!(!stale(&before));
    // At the tip the undo is part of the state; cut at the turn's last step, it is not.
    assert!(stale(&rebuild_at(&path, None)));
    assert!(!stale(&rebuild_at(&path, Some(last))));
}

/// Review C M4: records of a type this version does not know are skipped, and the rebuild
/// says so once.
#[tokio::test]
async fn a_rebuild_warns_once_about_records_of_a_newer_version() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![text_step("first")]);
    let mut run = start(fx.builder(config(Mode::Ask), &mock));
    run.turn_with("one").await;
    let path = run.engine.session_path().to_path_buf();
    run.stop().await;
    let text = std::fs::read_to_string(&path).unwrap();
    let last: Value = serde_json::from_str(text.lines().last().unwrap()).unwrap();
    let (offset, t) = (last["offset"].as_u64().unwrap() + 1, last["t"].clone());
    let mut appended = String::new();
    for (i, data) in [
        json!({"type": "tools_applied", "data": {"tools": []}}),
        json!({"type": "hologram", "data": 1}),
    ]
    .into_iter()
    .enumerate()
    {
        let line = json!({"type": "record", "offset": offset + i as u64, "t": t, "data": data});
        appended.push_str(&format!("{line}\n"));
    }
    std::fs::write(&path, format!("{text}{appended}")).unwrap();

    let rebuilt = rebuild_at(&path, None);
    let newer: Vec<&String> = rebuilt
        .warnings
        .iter()
        .filter(|w| w.contains("newer version"))
        .collect();
    assert_eq!(newer.len(), 1, "{:?}", rebuilt.warnings);
    assert!(newer[0].contains('2'), "{}", newer[0]);
}

#[tokio::test]
async fn a_log_holding_values_from_a_newer_version_still_resumes() {
    let fx = Fx::new();
    let cfg = || config(Mode::Ask);
    let mock = MockProvider::new(vec![text_step("first")]);
    let mut run = start(fx.builder(cfg(), &mock));
    run.turn_with("one").await;
    let id = run.engine.session_id().to_string();
    let path = run.engine.session_path().to_path_buf();
    run.stop().await;

    // A newer version appended records with commands, provider events, stop reasons,
    // budget scopes and read kinds this version does not know.
    let text = std::fs::read_to_string(&path).unwrap();
    let last: Value = serde_json::from_str(text.lines().last().unwrap()).unwrap();
    let (mut offset, t) = (last["offset"].as_u64().unwrap() + 1, last["t"].clone());
    let future = [
        json!({"type": "command", "data": {"type": "teleport", "to": "mars"}}),
        json!({"type": "command", "data": {"type": "set_budget", "scope": "week",
                                           "max_usd": null, "max_tokens": 1}}),
        json!({"type": "provider", "data": {"citation": {"url": "u"}}}),
        json!({"type": "provider", "data": {"done": {"stop": {"paused": {"s": 3}}}}}),
        json!({"type": "tool_result", "data": {"id": "fut", "result_blob": "a".repeat(64),
            "is_error": false, "meta": {"exit_code": null, "error_kind": null},
            "effects": [{"type": "read", "path": "a.txt",
                         "range": {"start": null, "end": null},
                         "file_hash": "b".repeat(64), "shown": {"type": "folded"}}]}}),
    ];
    let mut appended = String::new();
    for data in future {
        let line = json!({"type": "record", "offset": offset, "t": t, "data": data});
        appended.push_str(&format!("{line}\n"));
        offset += 1;
    }
    std::fs::write(&path, format!("{text}{appended}")).unwrap();

    let recs = records_at(&path);
    assert!(recs.contains(&SessionRecord::Command(Command::Unknown)));
    assert!(recs.contains(&SessionRecord::Provider(ProviderEvent::Unknown)));
    rebuild_at(&path, None);

    let mock = MockProvider::new(vec![text_step("second")]);
    let mut run = start(fx.resume(cfg(), &mock, &id));
    let events = run.turn_with("two").await;
    assert!(
        !events.iter().any(|e| matches!(e, Event::Error { .. })),
        "{events:?}"
    );
    // The unknown scope's 1-token limit applied to nothing.
    assert_eq!(finish_reason(&events), FinishReason::Completed);
    assert!(text_of(&mock.requests()[0]).contains("first"));
    run.stop().await;
}

#[tokio::test]
async fn resume_warns_when_the_logged_system_prompt_differs() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![text_step("ok")]);
    let mut run = start(fx.builder(config(Mode::Ask), &mock));
    run.turn_with("one").await;
    let id = run.engine.session_id().to_string();
    let path = run.engine.session_path().to_path_buf();
    run.stop().await;
    // As if an older version, with another system prompt, had written the log.
    let text = std::fs::read_to_string(&path).unwrap();
    let mut rewritten = 0;
    let lines: Vec<String> = text
        .lines()
        .map(|l| {
            let mut v: Value = serde_json::from_str(l).unwrap();
            if v["data"]["type"] == "prefix_applied" {
                v["data"]["data"]["system_prompt_hash"] = json!("0".repeat(64));
                rewritten += 1;
                v.to_string()
            } else {
                l.to_string()
            }
        })
        .collect();
    assert_eq!(rewritten, 1, "{text}");
    std::fs::write(&path, lines.join("\n") + "\n").unwrap();

    let mock = MockProvider::new(vec![text_step("ok")]);
    let mut run = start(fx.resume(config(Mode::Ask), &mock, &id));
    let events = run.turn_with("two").await;
    let w = warnings(&events);
    assert!(
        w.iter()
            .any(|w| w.contains("system prompt differs") && w.contains("cold")),
        "{w:?}"
    );
    assert!(!w.iter().any(|w| w.contains("tools differ")), "{w:?}");
    run.stop().await;
}

/// A call that failed, the same call blocked, then another call: the escape hatch is
/// spent, so the same call is blocked again after a resume, as it would be live.
#[tokio::test]
async fn resume_replays_the_attempt_ledger_escape_hatch() {
    let fx = Fx::new();
    fx.write("f1.txt", "file one\n");
    let missing = |id: &str| call(id, "read", json!({"path": "missing.txt"}));
    let mock = MockProvider::new(vec![
        tools_step(vec![missing("m1")]),
        tools_step(vec![missing("m2")]),
        tools_step(vec![call("r1", "read", json!({"path": "f1.txt"}))]),
        text_step("done"),
    ]);
    let mut run = start(fx.builder(config(Mode::Ask), &mock));
    run.turn_with("try").await;
    let id = run.engine.session_id().to_string();
    let results = tool_results(mock.requests().last().unwrap());
    run.stop().await;
    assert!(results[1].1.contains("already tried"), "{results:?}");

    let mock = MockProvider::new(vec![tools_step(vec![missing("m3")]), text_step("end")]);
    let mut run = start(fx.resume(config(Mode::Ask), &mock, &id));
    run.turn_with("again").await;
    let results = tool_results(mock.requests().last().unwrap());
    let last = results.last().unwrap();
    assert!(last.2 && last.1.contains("already tried"), "{results:?}");
    run.stop().await;
}

/// A session stopped before its first message is resumed: its start pins are the logged
/// ones, never added or logged again, even when AGENTS.md changed.
#[tokio::test]
async fn resuming_a_session_without_requests_keeps_its_start_pins() {
    let fx = Fx::new();
    fx.write("AGENTS.md", "# Project\n\n## Rules\n- OLD-RULE\n");
    let cfg = || {
        let mut c = config(Mode::Ask);
        c.pins = vec!["CFG-PIN stay small".into()];
        c
    };
    let mock = MockProvider::new(vec![]);
    let run = start(fx.builder(cfg(), &mock));
    let id = run.engine.session_id().to_string();
    let path = run.engine.session_path().to_path_buf();
    run.stop().await;
    let start_pins = || {
        records_at(&path)
            .iter()
            .filter(|r| matches!(r, SessionRecord::StartPins { .. }))
            .count()
    };
    assert_eq!(start_pins(), 1);

    fx.write("AGENTS.md", "# Project\n\n## Rules\n- NEW-RULE\n");
    let run = start(fx.resume(cfg(), &mock, &id));
    run.stop().await;
    assert_eq!(start_pins(), 1);

    let mock = MockProvider::new(vec![text_step("ok")]);
    let mut run = start(fx.resume(cfg(), &mock, &id));
    run.turn_with("hello").await;
    let text = text_of(mock.requests().last().unwrap());
    assert!(text.contains("OLD-RULE"), "{text}");
    assert!(!text.contains("NEW-RULE"), "{text}");
    assert_eq!(text.matches("CFG-PIN").count(), 1, "{text}");
    run.stop().await;
    assert_eq!(start_pins(), 1);
}

// ------------------------------------------------------------------------------- fork

#[tokio::test]
async fn fork_creates_child_with_parent_offset() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![text_step("answer one"), text_step("answer two")]);
    let mut run = start(fx.builder(config(Mode::Ask), &mock));
    // 110 tokens a turn: the parent's spend would put the branch's first turn at 88%.
    run.send(Command::SetBudget {
        scope: BudgetScope::Session,
        max_usd: None,
        max_tokens: Some(250),
    })
    .await;
    run.turn_with("one").await;
    let parent_id = run.engine.session_id().to_string();
    let parent = run.engine.session_path().to_path_buf();
    let child_id = run.fork(None, "branch-b").await;
    assert_ne!(child_id, parent_id);
    let child = log_of(&parent, &child_id);
    assert_eq!(
        run.engine.current_session(),
        (child_id.clone(), child.clone())
    );
    // A client that sends no step or label forks at the tip.
    assert_eq!(
        serde_json::from_str::<Command>(r#"{"type":"fork"}"#).unwrap(),
        Command::Fork {
            at: None,
            label: None
        }
    );

    let reader = SessionReader::<SessionRecord>::open(&child).unwrap();
    let header = reader.header().clone();
    let parent_records = raw_records(&parent);
    // The fork command itself is the parent's last record; the branch starts before it.
    let tip = parent_records.len() as u64 - 2;
    assert_eq!(header.parent, Some((parent_id.clone(), tip)));
    assert_eq!(header.origin, Origin::Fork);
    assert_eq!(header.label.as_deref(), Some("branch-b"));
    // Copied verbatim, with their original times.
    let copied = raw_records(&child);
    assert_eq!(copied[..=tip as usize], parent_records[..=tip as usize]);

    // The conversation continues on the branch; the parent's log is left alone. The
    // branch is a new session: nothing spent yet, under the same limit.
    let events = run.turn_with("two").await;
    assert_eq!(finish_reason(&events), FinishReason::Completed);
    let w = warnings(&events);
    assert!(!w.iter().any(|w| w.contains("budget")), "{w:?}");
    let text = text_of(mock.requests().last().unwrap());
    assert!(text.contains("one") && text.contains("answer one") && text.contains("two"));
    assert_eq!(raw_records(&parent), parent_records);
    assert!(records_at(&child).iter().any(|r| matches!(
        r,
        SessionRecord::TurnStart { text, .. } if text == "two"
    )));
    assert!(!records_at(&parent).iter().any(|r| matches!(
        r,
        SessionRecord::TurnStart { text, .. } if text == "two"
    )));
    run.stop().await;
}

/// Pins and budgets set between turns are part of the tip: the branch keeps them, live and
/// once resumed.
#[tokio::test]
async fn fork_at_tip_keeps_idle_pins_and_budgets() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![text_step("answer one"), text_step("answer two")]);
    let mut run = start(fx.builder(config(Mode::Ask), &mock));
    run.turn_with("one").await;
    run.send(Command::Pin {
        text: "IDLE-PIN keep it".into(),
    })
    .await;
    // 110 tokens a turn: the branch's second turn reaches 88% of it.
    run.send(Command::SetBudget {
        scope: BudgetScope::Session,
        max_usd: None,
        max_tokens: Some(250),
    })
    .await;
    let parent = run.engine.session_path().to_path_buf();
    let child_id = run.fork(None, "tip").await;
    let child = log_of(&parent, &child_id);
    let recs = records_at(&child);
    assert!(recs.iter().any(|r| matches!(
        r,
        SessionRecord::Command(Command::Pin { text }) if text.starts_with("IDLE-PIN")
    )));
    assert!(
        recs.iter()
            .any(|r| matches!(r, SessionRecord::Command(Command::SetBudget { .. })))
    );
    assert!(
        !recs
            .iter()
            .any(|r| matches!(r, SessionRecord::Command(Command::Fork { .. }))),
        "{recs:?}"
    );

    run.turn_with("two").await;
    let text = text_of(mock.requests().last().unwrap());
    assert!(text.contains("IDLE-PIN"), "{text}");
    run.stop().await;
    fx.ledger_rows(2).await;

    let mock = MockProvider::new(vec![text_step("answer three")]);
    let mut run = start(fx.resume(config(Mode::Ask), &mock, &child_id));
    let events = run.turn_with("three").await;
    let text = text_of(mock.requests().last().unwrap());
    assert!(text.contains("IDLE-PIN"), "{text}");
    let w = warnings(&events);
    assert!(
        w.iter().any(|w| w.contains("session budget 88% used")),
        "{w:?}"
    );
    run.stop().await;
}

/// A fork at the last step takes the commands sent between turns after it as a fork at
/// the tip does, whatever their order.
#[tokio::test]
async fn fork_at_the_last_step_keeps_idle_commands_like_a_fork_at_the_tip() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![text_step("answer one"), text_step("answer two")]);
    let mut run = start(fx.builder(config(Mode::Ask), &mock));
    run.turn_with("one").await;
    run.send(Command::SetBudget {
        scope: BudgetScope::Session,
        max_usd: None,
        max_tokens: Some(250),
    })
    .await;
    run.send(Command::Pin {
        text: "IDLE-PIN keep it".into(),
    })
    .await;
    let parent = run.engine.session_path().to_path_buf();
    // The user message is step 1, its answer step 2.
    let child_id = run.fork(Some(2), "last step").await;
    let recs = records_at(&log_of(&parent, &child_id));
    assert!(
        recs.iter().any(|r| matches!(
            r,
            SessionRecord::Command(Command::Pin { text }) if text.starts_with("IDLE-PIN")
        )),
        "{recs:?}"
    );
    assert!(
        recs.iter()
            .any(|r| matches!(r, SessionRecord::Command(Command::SetBudget { .. }))),
        "{recs:?}"
    );
    run.turn_with("two").await;
    let text = text_of(mock.requests().last().unwrap());
    assert!(text.contains("IDLE-PIN"), "{text}");
    run.stop().await;
}

/// A branch is a new session with nothing spent, even resumed before its first request
/// (no ledger rows of its own) or without the ledger (its own usage records only).
#[tokio::test]
async fn a_resumed_fork_counts_only_its_own_spend() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![text_step("answer one")]);
    let mut run = start(fx.builder(config(Mode::Ask), &mock));
    run.send(Command::SetBudget {
        scope: BudgetScope::Session,
        max_usd: None,
        max_tokens: Some(300),
    })
    .await;
    run.turn_with("one").await;
    let child_id = run.fork(None, "branch").await;
    run.stop().await;
    fx.ledger_rows(1).await;

    // 110 of 300: no warning, and the ledger (no rows of the branch yet) is the source.
    let mock = MockProvider::new(vec![text_step("answer two")]);
    let mut run = start(fx.resume(config(Mode::Ask), &mock, &child_id));
    let events = run.turn_with("two").await;
    let w = warnings(&events);
    assert!(!w.iter().any(|w| w.contains("budget")), "{w:?}");
    assert!(!w.iter().any(|w| w.contains("session log")), "{w:?}");
    run.stop().await;
    fx.ledger_rows(2).await;

    // Without the ledger: the branch's own 110 plus this turn's is 220 of 300 (73%); the
    // parent's usage, copied into the branch's log, would make it 330.
    break_ledger(&fx);
    let mock = MockProvider::new(vec![text_step("answer three")]);
    let mut run = start(fx.resume(config(Mode::Ask), &mock, &child_id));
    let events = run.turn_with("three").await;
    assert_eq!(finish_reason(&events), FinishReason::Completed);
    let w = warnings(&events);
    assert!(w.iter().any(|w| w.contains("session log")), "{w:?}");
    assert!(
        !w.iter()
            .any(|w| w.contains("exhausted") || w.contains("budget")),
        "{w:?}"
    );
    run.stop().await;
}

#[tokio::test]
async fn fork_with_nothing_to_branch_from_is_refused() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![]);
    let mut run = start(fx.builder(config(Mode::Ask), &mock));
    let parent = run.engine.session_path().to_path_buf();
    run.send(Command::Fork {
        at: None,
        label: None,
    })
    .await;
    let events = run
        .until(|e| matches!(e, Event::SessionChanged { .. } | Event::Error { .. }))
        .await;
    // The first event is the session's own SessionChanged.
    let events = if matches!(events.last(), Some(Event::SessionChanged { .. })) {
        run.until(|e| matches!(e, Event::SessionChanged { .. } | Event::Error { .. }))
            .await
    } else {
        events
    };
    assert!(
        matches!(events.last(), Some(Event::Error { message, .. }) if message.contains("nothing")),
        "{events:?}"
    );
    let logs = std::fs::read_dir(parent.parent().unwrap())
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .path()
                .extension()
                .is_some_and(|x| x == "jsonl")
        })
        .count();
    assert_eq!(logs, 1);
    assert_eq!(run.engine.current_session().1, parent);
    run.stop().await;
}

#[tokio::test]
async fn fork_at_step_copies_records_and_sets_parent_label_origin() {
    let fx = Fx::new();
    fx.write("f1.txt", "file one\n");
    fx.write("f2.txt", "file two\n");
    let mock = MockProvider::new(vec![
        tools_step(vec![call("r1", "read", json!({"path": "f1.txt"}))]),
        tools_step(vec![call("r2", "read", json!({"path": "f2.txt"}))]),
        text_step("done"),
        text_step("on the branch"),
    ]);
    let mut run = start(fx.builder(config(Mode::Ask), &mock));
    run.turn_with("read both").await;
    let parent_id = run.engine.session_id().to_string();
    let parent = run.engine.session_path().to_path_buf();
    // Steps: the message is 1, the reads 2 and 3, the answer 4.
    let parent_recs = records_at(&parent);
    let steps: Vec<u32> = parent_recs
        .iter()
        .filter_map(|r| match r {
            SessionRecord::Request { step, .. } => Some(*step),
            _ => None,
        })
        .collect();
    assert_eq!(steps, [2, 3, 4]);
    // Step 2 ends with the last record before step 3 starts.
    let step3 = parent_recs
        .iter()
        .position(|r| matches!(r, SessionRecord::Request { step: 3, .. }))
        .unwrap();
    let end = parent_recs[..step3]
        .iter()
        .rposition(|r| !matches!(r, SessionRecord::Command(_)))
        .unwrap();
    let expected_prefix = assemble(&rebuild_at(&parent, Some(2)));

    let child_id = run.fork(Some(2), "at-two").await;
    let child = log_of(&parent, &child_id);
    let header = SessionReader::<SessionRecord>::open(&child)
        .unwrap()
        .header()
        .clone();
    assert_eq!(header.parent, Some((parent_id, end as u64)));
    assert_eq!(header.origin, Origin::Fork);
    assert_eq!(header.label.as_deref(), Some("at-two"));
    let copied = raw_records(&child);
    assert_eq!(copied[..=end], raw_records(&parent)[..=end]);
    // The copy is marked, and the cut turn is closed in the branch.
    let own = &records_at(&child)[end + 1..];
    assert!(matches!(own[0], SessionRecord::Forked), "{own:?}");
    assert!(
        matches!(
            own[1],
            SessionRecord::TurnEnd {
                reason: FinishReason::Cancelled,
                ..
            }
        ),
        "{own:?}"
    );

    // The branch's next request continues from step 2: f1 read, f2 not.
    run.turn_with("and now?").await;
    let req = mock.requests().last().unwrap().clone();
    let ids: Vec<String> = tool_results(&req).into_iter().map(|r| r.0).collect();
    assert_eq!(ids, ["r1"]);
    assert!(text_of(&req).contains("and now?"));
    assert_eq!(req.system, expected_prefix.system);
    assert_eq!(
        req.messages[..expected_prefix.messages.len() - 1],
        expected_prefix.messages[..expected_prefix.messages.len() - 1]
    );
    run.stop().await;
}

#[tokio::test]
async fn rebuild_until_step_matches_live_prefix_at_that_step() {
    let fx = Fx::new();
    fx.write("f1.txt", "file one\n");
    fx.write("f2.txt", "file two\n");
    fx.write("f3.txt", "file three\n");
    // No result is superseded: a collection at a step start would change the history
    // between the end of one step and the request of the next.
    let mock = MockProvider::new(vec![
        tools_step(vec![call("r1", "read", json!({"path": "f1.txt"}))]),
        tools_step(vec![
            call("r2", "read", json!({"path": "f2.txt"})),
            call("s1", "state", json!({"decisions_add": ["DECIDED"]})),
        ]),
        text_step("done"),
        tools_step(vec![call("r3", "read", json!({"path": "f3.txt"}))]),
        text_step("again"),
    ]);
    let mut run = start(fx.builder(config(Mode::Ask), &mock));
    run.turn_with("first").await;
    run.turn_with("second").await;
    let path = run.engine.session_path().to_path_buf();
    // Before the session-end episode request.
    let live = mock.requests();
    run.stop().await;
    let steps: Vec<u32> = records_at(&path)
        .iter()
        .filter_map(|r| match r {
            SessionRecord::Request { step, .. } => Some(*step),
            _ => None,
        })
        .collect();
    assert_eq!(steps.len(), live.len());
    // Each request was built from the context at the end of the step before it.
    for (step, req) in steps.iter().zip(&live) {
        let rebuilt = rebuild_at(&path, Some(step - 1));
        assert_eq!(&assemble(&rebuilt), req, "step {step}");
    }
    // A step past the tip does not exist.
    let reader = SessionReader::<SessionRecord>::open(&path).unwrap();
    assert!(resume::rebuild(&reader, Some(99)).is_err());
}

// ------------------------------------------------------------------------- compaction

fn compaction_fixture(fx: &Fx) -> Config {
    for i in 1..=4 {
        fx.write(&format!("f{i}.txt"), &format!("file {i}\n"));
    }
    let mut cfg = config(Mode::Ask);
    cfg.catalog_overrides = Some(catalog(16_000, 1_000, "str_replace"));
    cfg
}

fn four_reads() -> Vec<MockTurn> {
    (1..=4)
        .map(|i| {
            tools_step(vec![call(
                &format!("r{i}"),
                "read",
                json!({"path": format!("f{i}.txt")}),
            )])
        })
        .collect()
}

#[tokio::test]
async fn auto_compaction_announced_before_running() {
    let fx = Fx::new();
    let cfg = compaction_fixture(&fx);
    let mut script = four_reads();
    script.push(text_step("SUMMARY-TEXT"));
    script.push(text_step("done"));
    let mock = MockProvider::new(script);
    let mut run = start(fx.builder(cfg, &mock));
    let prompt = format!(
        "PROMPT-MARKER {}",
        "lorem ipsum dolor sit amet ".repeat(1500)
    );
    let events = run.turn_with(&prompt).await;
    assert_eq!(finish_reason(&events), FinishReason::Completed);
    let compacted = events
        .iter()
        .position(|e| matches!(e, Event::Compacted { .. }))
        .expect("no Compacted event");
    let announced = events[..compacted].iter().rposition(|e| {
        matches!(e, Event::ContextStatus { warnings, .. }
            if warnings.iter().any(|w| w.contains("compacting")))
    });
    assert!(announced.is_some(), "{events:?}");
    // Nothing else of the step comes between the announcement and the result.
    assert!(
        !events[announced.unwrap()..compacted]
            .iter()
            .any(|e| matches!(e, Event::ToolStarted { .. } | Event::TextDelta { .. }))
    );
}

#[tokio::test]
async fn context_overflow_compacts_and_retries_once() {
    let fx = Fx::new();
    let cfg = compaction_fixture(&fx);
    let overflow = || MockTurn::Error(ProviderError::ContextOverflow("too long".into()));
    let mut script = four_reads();
    script.push(text_step("first done"));
    // Turn 2: overflow, compaction summary, the retry succeeds.
    script.push(overflow());
    script.push(text_step("SUMMARY-ONE"));
    script.push(text_step("second done"));
    // Turn 3: overflow, summary, and the retry overflows too: no second retry.
    script.push(overflow());
    script.push(text_step("SUMMARY-TWO"));
    script.push(overflow());
    script.push(text_step("never sent"));
    let mock = MockProvider::new(script);
    let mut run = start(fx.builder(cfg, &mock));
    run.turn_with("first").await;
    let before = mock.requests().len();

    let events = run.turn_with("second").await;
    assert_eq!(
        finish_reason(&events),
        FinishReason::Completed,
        "{events:?}"
    );
    let compactions: Vec<&Event> = events
        .iter()
        .filter(|e| matches!(e, Event::Compacted { .. }))
        .collect();
    assert_eq!(compactions.len(), 1, "{events:?}");
    assert!(matches!(
        compactions[0],
        Event::Compacted { reason: tautly_core::MutationReason::Forced, summary, .. }
            if summary == "SUMMARY-ONE"
    ));
    let reqs = mock.requests();
    assert_eq!(reqs.len(), before + 3);
    assert!(text_of(&reqs[before + 2]).contains("SUMMARY-ONE"));
    assert!(!events.iter().any(|e| matches!(e, Event::Error { .. })));

    let events = run.turn_with("third").await;
    assert_eq!(finish_reason(&events), FinishReason::Error, "{events:?}");
    assert!(events.iter().any(|e| matches!(
        e,
        Event::Error {
            kind: tautly_core::ErrorKind::ContextOverflow,
            ..
        }
    )));
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, Event::Compacted { .. }))
            .count(),
        1
    );
    assert_eq!(mock.requests().len(), before + 6);
}

/// The records between the last `TurnStart` and the `Request` after the last
/// `Compaction`, by type.
fn after_last_compaction(recs: &[SessionRecord]) -> Vec<&SessionRecord> {
    let at = recs
        .iter()
        .rposition(|r| matches!(r, SessionRecord::Compaction { .. }))
        .expect("a compaction");
    recs[at + 1..]
        .iter()
        .take_while(|r| !matches!(r, SessionRecord::Request { .. }))
        .collect()
}

/// A catalog with an explicit (warm) prompt cache and a window that never asks for
/// threshold compaction.
fn warm_cache_catalog() -> Catalog {
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
    .unwrap()
}

/// The records of the last turn before its first `Request`.
fn before_last_request(recs: &[SessionRecord]) -> Vec<&SessionRecord> {
    let at = recs
        .iter()
        .rposition(|r| matches!(r, SessionRecord::TurnStart { .. }))
        .unwrap();
    recs[at..]
        .iter()
        .take_while(|r| !matches!(r, SessionRecord::Request { .. }))
        .collect()
}

/// Re-review N6: a manual `Compact` or `Uncompact` breaks the cached prefix between
/// turns; the next step's epoch takes a pin sent after it along, so the prefix breaks
/// once.
#[tokio::test]
async fn a_manual_compact_or_uncompact_makes_the_next_epoch_cold() {
    for uncompact in [false, true] {
        let fx = Fx::new();
        let mut cfg = compaction_fixture(&fx);
        cfg.catalog_overrides = Some(warm_cache_catalog());
        let mut script = four_reads();
        script.push(text_step("first done"));
        script.push(text_step("SUMMARY-ONE"));
        script.push(text_step("between"));
        script.push(text_step("second done"));
        let mock = MockProvider::new(script);
        let mut run = start(fx.builder(cfg, &mock));
        run.turn_with("first").await;
        run.send(Command::Compact).await;
        run.until(|e| matches!(e, Event::Compacted { .. })).await;
        if uncompact {
            // A request after the compaction warms the cache again.
            run.turn_with("between").await;
            run.send(Command::Uncompact).await;
        }
        run.send(Command::Pin {
            text: "PIN-AFTER".into(),
        })
        .await;
        let events = run.turn_with("second").await;
        assert_eq!(
            finish_reason(&events),
            FinishReason::Completed,
            "{events:?}"
        );
        let path = run.engine.session_path().to_path_buf();
        run.stop().await;
        let recs = records_at(&path);
        if uncompact {
            assert!(
                recs.iter()
                    .any(|r| matches!(r, SessionRecord::Uncompacted { .. })),
                "{recs:?}"
            );
        }
        let before = before_last_request(&recs);
        assert!(
            before
                .iter()
                .any(|r| matches!(r, SessionRecord::PinsFolded)),
            "uncompact {uncompact}: {before:?}"
        );
    }
}

/// Review A M6: the overflow retry's compaction breaks the cached prefix outside the
/// epoch; the retried step's epoch takes the pending pins with it, so it breaks once.
#[tokio::test]
async fn an_overflow_compaction_folds_pending_pins_into_the_same_prefix_break() {
    let fx = Fx::new();
    let mut cfg = compaction_fixture(&fx);
    // A warm cache (and a window that never asks for threshold compaction).
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
    let mut script = four_reads();
    script.push(text_step("first done"));
    script.push(MockTurn::Error(ProviderError::ContextOverflow(
        "too long".into(),
    )));
    script.push(text_step("SUMMARY-ONE"));
    script.push(text_step("second done"));
    let mock = MockProvider::new(script);
    let mut run = start(fx.builder(cfg, &mock));
    run.turn_with("first").await;
    run.send(Command::Pin {
        text: "PIN-AFTER-TURN-ONE".into(),
    })
    .await;
    let events = run.turn_with("second").await;
    assert_eq!(
        finish_reason(&events),
        FinishReason::Completed,
        "{events:?}"
    );
    let path = run.engine.session_path().to_path_buf();
    run.stop().await;
    let recs = records_at(&path);
    let between = after_last_compaction(&recs);
    assert!(
        between
            .iter()
            .any(|r| matches!(r, SessionRecord::PinsFolded)),
        "{between:?}"
    );
}

#[tokio::test]
async fn uncompact_command_restores() {
    let fx = Fx::new();
    let big: String = (0..400)
        .map(|i| format!("line {i}: some filler text to make this file large\n"))
        .collect();
    fx.write("big.txt", &big);
    let mut script: Vec<MockTurn> = (0..5)
        .map(|i| {
            tools_step(vec![call(
                &format!("r{i}"),
                "read",
                json!({"path": "big.txt", "full": true}),
            )])
        })
        .collect();
    script.push(text_step("done"));
    script.push(text_step("THE-SUMMARY"));
    script.push(text_step("after compaction"));
    script.push(text_step("after uncompact"));
    let mock = MockProvider::new(script);
    let mut run = start(fx.builder(config(Mode::Ask), &mock));
    let events = run.turn_with("PROMPT-MARKER read it five times").await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::ContextCollected { .. }))
    );
    let full = mock.requests().last().unwrap().clone();

    run.send(Command::Compact).await;
    let events = run
        .until(|e| matches!(e, Event::Compacted { .. } | Event::Error { .. }))
        .await;
    assert!(
        matches!(
            events.last(),
            Some(Event::Compacted {
                reason: tautly_core::MutationReason::Manual,
                ..
            })
        ),
        "{events:?}"
    );
    run.turn_with("compacted?").await;
    let compacted = text_of(mock.requests().last().unwrap());
    assert!(compacted.contains("THE-SUMMARY") && !compacted.contains("PROMPT-MARKER"));

    run.send(Command::Uncompact).await;
    run.turn_with("restored?").await;
    let restored = mock.requests().last().unwrap().clone();
    let text = text_of(&restored);
    assert!(text.contains("PROMPT-MARKER") && !text.contains("THE-SUMMARY"));
    // Every collected result has its output back.
    let results = tool_results(&restored);
    assert_eq!(results.len(), 5);
    assert!(
        results.iter().all(|r| !r.1.starts_with("[elided")),
        "{results:?}"
    );
    assert_eq!(tool_results(&full).len(), 5);
    let path = run.engine.session_path().to_path_buf();
    run.stop().await;
    assert!(records_at(&path).iter().any(|r| matches!(
        r,
        SessionRecord::Uncompacted { compaction: true, restored } if !restored.is_empty()
    )));
    // A resume rebuilds the restored context too.
    let rebuilt = rebuild_at(&path, None);
    assert!(rebuilt.warnings.is_empty(), "{:?}", rebuilt.warnings);
    let rebuilt = assemble(&rebuilt);
    let results = tool_results(&rebuilt);
    assert!(results.iter().all(|r| !r.1.starts_with("[elided")));
    assert!(text_of(&rebuilt).contains("PROMPT-MARKER"));

    // A restored result whose blob is gone stays collected, and the rebuild says so.
    let blob = records_at(&path)
        .into_iter()
        .find_map(|r| match r {
            SessionRecord::ToolResult { result_blob, .. } => Some(result_blob),
            _ => None,
        })
        .unwrap();
    let (head, rest) = blob.as_str().split_at(2);
    std::fs::remove_file(path.with_file_name("blobs").join(head).join(rest)).unwrap();
    let rebuilt = rebuild_at(&path, None);
    assert!(
        rebuilt.warnings.iter().any(|w| w.contains("uncompact")),
        "{:?}",
        rebuilt.warnings
    );
}

/// A manual compaction is a network call: Cancel stops it, and so does Shutdown.
#[tokio::test]
async fn a_manual_compaction_can_be_cancelled() {
    let fx = Fx::new();
    let mut script: Vec<MockTurn> = (1..=3).map(|i| text_step(&format!("answer {i}"))).collect();
    script.push(MockTurn::Stall);
    script.push(text_step("after"));
    script.push(MockTurn::Stall);
    let mock = MockProvider::new(script);
    let mut run = start(fx.builder(config(Mode::Ask), &mock));
    for i in 1..=3 {
        run.turn_with(&format!("message {i}")).await;
    }
    let compacting = |e: &Event| {
        matches!(e, Event::ContextStatus { warnings, .. }
            if warnings.iter().any(|w| w.contains("compacting")))
    };
    run.send(Command::Compact).await;
    run.until(compacting).await;
    run.send(Command::Cancel).await;
    let quick = Duration::from_secs(5);
    let events = tokio::time::timeout(
        quick,
        run.until(|e| matches!(e, Event::Warning { message } if message.contains("cancelled"))),
    )
    .await
    .expect("the compaction was not cancelled");
    assert!(
        !events.iter().any(|e| matches!(e, Event::Compacted { .. })),
        "{events:?}"
    );
    // The session goes on, uncompacted.
    run.turn_with("next").await;
    assert!(text_of(mock.requests().last().unwrap()).contains("answer 1"));

    run.send(Command::Compact).await;
    run.until(compacting).await;
    let path = run.engine.session_path().to_path_buf();
    tokio::time::timeout(quick, run.stop())
        .await
        .expect("shutdown waited for the compaction");
    assert!(
        !records_at(&path)
            .iter()
            .any(|r| matches!(r, SessionRecord::Compaction { .. }))
    );
}

/// A provider that panics on request `n` (0-based) and serves the mock otherwise: an
/// engine bug in a job.
struct PanicsOn {
    mock: MockProvider,
    n: usize,
    seen: std::sync::atomic::AtomicUsize,
}

impl tautly_provider::Provider for PanicsOn {
    fn name(&self) -> &str {
        "mock"
    }

    fn stream(
        &self,
        req: CanonicalRequest,
        cancel: tokio_util::sync::CancellationToken,
    ) -> futures::stream::BoxStream<'static, Result<ProviderEvent, ProviderError>> {
        let i = self.seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        assert_ne!(i, self.n, "probe: summary provider bug");
        self.mock.stream(req, cancel)
    }
}

/// Review A M3: the answer to a command the client asked for waits (a bounded time) for
/// a client that is behind, instead of being dropped from a full channel.
#[tokio::test]
async fn replies_to_commands_wait_for_a_client_that_is_behind() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![text_step("one")]);
    let mut run = start(fx.builder(config(Mode::Ask), &mock));
    run.turn_with("one").await;
    // Fill the event channel without reading it: each refused `SetBudget` is a warning.
    for _ in 0..300 {
        run.send(Command::SetBudget {
            scope: BudgetScope::Session,
            max_usd: Some(-1.0),
            max_tokens: None,
        })
        .await;
    }
    run.send(Command::Undo {
        turn: None,
        dry_run: true,
    })
    .await;
    run.send(Command::SetModel {
        model: ModelId::from(MODEL),
    })
    .await;
    run.send(Command::Compact).await;
    run.send(Command::Uncompact).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut events = Vec::new();
    let answered = |events: &[Event]| {
        let warned = |s: &str| {
            events
                .iter()
                .any(|e| matches!(e, Event::Warning { message } if message.contains(s)))
        };
        events.iter().any(|e| matches!(e, Event::Undone { .. }))
            && events
                .iter()
                .any(|e| matches!(e, Event::ModelChanged { .. }))
            && warned("nothing to compact")
            && warned("nothing to restore")
    };
    let end = Instant::now() + Duration::from_secs(5);
    while !answered(&events) && Instant::now() < end {
        if let Ok(Some(ev)) =
            tokio::time::timeout(Duration::from_millis(200), run.events.recv()).await
        {
            events.push(ev);
        }
    }
    let tail: Vec<&Event> = events
        .iter()
        .filter(|e| !matches!(e, Event::Warning { message } if message.contains("SetBudget")))
        .collect();
    assert!(answered(&events), "{tail:?}");
    run.stop().await;
}

/// Review A probe P7: a panic in a `Compact` job is no turn's end.
#[tokio::test]
async fn a_panic_in_a_compaction_job_logs_no_turn_end() {
    let fx = Fx::new();
    let mock = MockProvider::new((1..=3).map(|i| text_step(&format!("answer {i}"))).collect());
    let provider = Arc::new(PanicsOn {
        mock: mock.clone(),
        n: 3,
        seen: Default::default(),
    });
    let mut run = start(EngineBuilder {
        provider: Some(provider),
        ..fx.builder(config(Mode::Ask), &mock)
    });
    for i in 1..=3 {
        run.turn_with(&format!("message {i}")).await;
    }
    run.send(Command::Compact).await;
    let events = run
        .until(|e| matches!(e, Event::Error { message, .. } if message.contains("panicked")))
        .await;
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, Event::TurnFinished { .. })),
        "{events:?}"
    );
    let path = run.engine.session_path().to_path_buf();
    let rest = run.stop_events().await;
    assert!(
        !rest.iter().any(|e| matches!(e, Event::TurnFinished { .. })),
        "{rest:?}"
    );
    let recs = records_at(&path);
    let count = |f: fn(&SessionRecord) -> bool| recs.iter().filter(|r| f(r)).count();
    assert_eq!(
        count(|r| matches!(r, SessionRecord::TurnStart { .. })),
        count(|r| matches!(r, SessionRecord::TurnEnd { .. })),
        "{recs:?}"
    );
    assert_eq!(count(|r| matches!(r, SessionRecord::EngineError { .. })), 1);
}

// ------------------------------------------------------------------------------- undo

fn patch(lines: &[&str]) -> String {
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

/// Turn 1 creates `a.txt`; turn 2 changes it and deletes the executable `run.sh`.
async fn two_turns_of_edits(fx: &Fx) -> PathBuf {
    fx.write("run.sh", "#!/bin/sh\necho hi\n");
    std::fs::set_permissions(
        fx.dir.path().join("run.sh"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let mut cfg = config(Mode::AutoEdit);
    cfg.catalog_overrides = Some(catalog(200_000, 8_000, "patch"));
    let mock = MockProvider::new(vec![
        tools_step(vec![call(
            "w1",
            "write",
            json!({"path": "a.txt", "content": "v1\n"}),
        )]),
        text_step("created"),
        tools_step(vec![
            call("ra", "read", json!({"path": "a.txt"})),
            call("rr", "read", json!({"path": "run.sh"})),
        ]),
        tools_step(vec![call(
            "e1",
            "edit",
            json!({"patch": patch(&[
                "*** Begin Patch",
                "*** Update File: a.txt",
                "@@",
                "-v1",
                "+v2",
                "*** Add File: new/made.txt",
                "+made",
                "*** Delete File: run.sh",
                "*** End Patch",
            ])}),
        )]),
        text_step("changed"),
    ]);
    let mut run = start(fx.builder(cfg, &mock));
    run.turn_with("create").await;
    let events = run.turn_with("change").await;
    assert_eq!(finish_reason(&events), FinishReason::Completed);
    assert_eq!(fx.read("a.txt").as_deref(), Some("v2\n"));
    assert!(fx.read("run.sh").is_none());
    let path = run.engine.session_path().to_path_buf();
    run.stop().await;
    path
}

async fn undo(fx: &Fx, path: &Path, turn: Option<u32>) -> Vec<PathBuf> {
    let reader = SessionReader::<SessionRecord>::open(path).unwrap();
    let report = resume::undo(&reader, &fx.sandbox(), None, turn, false)
        .await
        .unwrap();
    let mut changed = report.restored;
    changed.extend(report.deleted);
    changed.sort();
    changed
}

#[tokio::test]
async fn undo_last_turn_restores_files() {
    let fx = Fx::new();
    let path = two_turns_of_edits(&fx).await;
    let restored = undo(&fx, &path, None).await;
    let root = fx.root();
    assert_eq!(
        restored,
        [
            root.join("a.txt"),
            root.join("new/made.txt"),
            root.join("run.sh")
        ]
    );
    assert_eq!(fx.read("a.txt").as_deref(), Some("v1\n"));
    assert_eq!(fx.read("run.sh").as_deref(), Some("#!/bin/sh\necho hi\n"));
    let mode = std::fs::metadata(root.join("run.sh"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o7777, 0o755);
}

#[tokio::test]
async fn undo_deletes_files_created_in_turn() {
    let fx = Fx::new();
    let path = two_turns_of_edits(&fx).await;
    undo(&fx, &path, None).await;
    assert!(fx.read("new/made.txt").is_none());
    // Turn 1 created a.txt: undoing it removes the file; run.sh stays as turn 2's undo left it.
    let restored = undo(&fx, &path, Some(1)).await;
    assert_eq!(restored, [fx.root().join("a.txt")]);
    assert!(fx.read("a.txt").is_none());
    assert!(fx.read("run.sh").is_some());
    // There is no turn 3.
    let reader = SessionReader::<SessionRecord>::open(&path).unwrap();
    assert!(
        resume::undo(&reader, &fx.sandbox(), None, Some(3), false)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn undo_appends_undone_record_with_from_and_to_blobs() {
    let fx = Fx::new();
    let path = two_turns_of_edits(&fx).await;
    undo(&fx, &path, None).await;
    let reader = SessionReader::<SessionRecord>::open(&path).unwrap();
    let recs: Vec<SessionRecord> = reader.records().map(|r| r.unwrap().1).collect();
    let Some(SessionRecord::Undone {
        turn,
        files,
        backup_ref,
    }) = recs.last()
    else {
        panic!("{:?}", recs.last())
    };
    assert_eq!(*turn, 2);
    assert_eq!(*backup_ref, None);
    let by_path = |p: &str| -> &UndoneFile {
        files
            .iter()
            .find(|f| f.path.as_str() == p)
            .unwrap_or_else(|| panic!("{p} not in {files:?}"))
    };
    let blob = |b: &Option<tautly_session::BlobRef>| {
        b.as_ref()
            .map(|b| String::from_utf8(reader.read_blob(b).unwrap()).unwrap())
    };
    let a = by_path("a.txt");
    assert_eq!(blob(&a.from_blob).as_deref(), Some("v2\n"));
    assert_eq!(blob(&a.to).as_deref(), Some("v1\n"));
    assert!(a.from_mode.is_some() && a.to_mode.is_some());
    let run_sh = by_path("run.sh");
    assert_eq!((blob(&run_sh.from_blob), run_sh.from_mode), (None, None));
    assert_eq!(blob(&run_sh.to).as_deref(), Some("#!/bin/sh\necho hi\n"));
    assert_eq!(run_sh.to_mode, Some(0o755));
    let made = by_path("new/made.txt");
    assert_eq!(blob(&made.from_blob).as_deref(), Some("made\n"));
    assert_eq!((made.to.clone(), made.to_mode), (None, None));

    // Rebuild replays it: the files it changed are no longer known as the model left them.
    let rebuilt = rebuild_at(&path, None);
    let note = &rebuilt.context.state.files[&fx.root().join("a.txt")];
    assert!(note.maybe_stale, "{note:?}");
}

#[tokio::test]
async fn undo_refuses_files_changed_since_the_turn() {
    let fx = Fx::new();
    let path = two_turns_of_edits(&fx).await;
    let records = records_at(&path).len();
    let refused = |fx: &Fx| {
        let reader = SessionReader::<SessionRecord>::open(&path).unwrap();
        let sandbox = fx.sandbox();
        async move { resume::undo(&reader, &sandbox, None, None, false).await }
    };
    // A human edit after the turn.
    fx.write("a.txt", "HUMAN EDIT\n");
    match refused(&fx).await {
        Err(UndoError::Changed { paths }) => assert_eq!(paths, [fx.root().join("a.txt")]),
        other => panic!("{other:?}"),
    }
    // Nothing was written: not even the files that did not change.
    assert_eq!(fx.read("a.txt").as_deref(), Some("HUMAN EDIT\n"));
    assert!(fx.read("run.sh").is_none());
    assert_eq!(fx.read("new/made.txt").as_deref(), Some("made\n"));
    assert_eq!(records_at(&path).len(), records);

    // A file the turn deleted was made again.
    fx.write("a.txt", "v2\n");
    fx.write("run.sh", "mine now\n");
    match refused(&fx).await {
        Err(UndoError::Changed { paths }) => assert_eq!(paths, [fx.root().join("run.sh")]),
        other => panic!("{other:?}"),
    }
    assert_eq!(fx.read("run.sh").as_deref(), Some("mine now\n"));
    assert_eq!(records_at(&path).len(), records);
}

#[tokio::test]
async fn undo_refuses_an_earlier_turn_whose_files_later_turns_changed() {
    let fx = Fx::new();
    let path = two_turns_of_edits(&fx).await;
    let reader = SessionReader::<SessionRecord>::open(&path).unwrap();
    match resume::undo(&reader, &fx.sandbox(), None, Some(1), false).await {
        Err(UndoError::LaterTurns { turns, paths }) => {
            assert_eq!(turns, [2]);
            assert_eq!(paths, [fx.root().join("a.txt")]);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(fx.read("a.txt").as_deref(), Some("v2\n"));
    // Once turn 2 is undone, turn 1 can be (undo_deletes_files_created_in_turn).
}

/// A human's mode-only change (the content is still what the turn left) is a change too.
#[tokio::test]
async fn undo_refuses_a_mode_only_change() {
    let fx = Fx::new();
    let path = two_turns_of_edits(&fx).await;
    let records = records_at(&path).len();
    let a = fx.dir.path().join("a.txt");
    std::fs::set_permissions(&a, std::fs::Permissions::from_mode(0o600)).unwrap();
    let reader = SessionReader::<SessionRecord>::open(&path).unwrap();
    match resume::undo(&reader, &fx.sandbox(), None, None, false).await {
        Err(UndoError::Changed { paths }) => assert_eq!(paths, [fx.root().join("a.txt")]),
        other => panic!("{other:?}"),
    }
    let mode = std::fs::metadata(&a).unwrap().permissions().mode();
    assert_eq!(mode & 0o7777, 0o600);
    assert_eq!(fx.read("a.txt").as_deref(), Some("v2\n"));
    assert!(fx.read("run.sh").is_none());
    assert_eq!(records_at(&path).len(), records);
}

/// A file already back at the turn's pre-image (reverted by hand) is still recorded as
/// undone, so the earlier turn that also changed it can be undone next.
#[tokio::test]
async fn files_already_back_at_their_pre_image_do_not_block_earlier_undos() {
    let fx = Fx::new();
    let path = two_turns_of_edits(&fx).await;
    fx.write("a.txt", "v1\n");
    let restored = undo(&fx, &path, Some(2)).await;
    let root = fx.root();
    assert_eq!(restored, [root.join("new/made.txt"), root.join("run.sh")]);
    assert_eq!(fx.read("a.txt").as_deref(), Some("v1\n"));
    // Turn 1 created a.txt; nothing later stands in the way now.
    let restored = undo(&fx, &path, Some(1)).await;
    assert_eq!(restored, [root.join("a.txt")]);
    assert!(fx.read("a.txt").is_none());
    // Both undone: nothing is left.
    assert_eq!(undo(&fx, &path, Some(2)).await, Vec::<PathBuf>::new());
}
