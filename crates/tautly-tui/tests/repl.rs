//! The REPL (spec 7.11.8): scripted input, captured output, real engines on a scripted
//! provider and a temporary directory; and the rendering of single events.

use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use regex::Regex;
use serde_json::{Value, json};
use tautly_core::{
    Command, Config, DiffFile, DiffPreview, DiffStatus, EffectView, EngineBuilder, Event,
    FinishReason, SaveKind, Saved, SessionReader, SessionRecord, TestEvidence, ToolMetaView,
    Workspace,
};
use tautly_provider::{
    Catalog, MockProvider, MockTurn, ModelId, ProviderEvent, RawUsage, StopReason,
};
use tautly_sandbox::{LocalSandbox, Mode, Sandbox};
use tautly_tui::approve::exact_rule;
use tautly_tui::render::{self, RenderState};
use tautly_tui::{EngineFactory, Line, ReplOptions, ScriptedSource, run_repl};
use tempfile::TempDir;
use tokio::io::AsyncWrite;
use tokio::sync::mpsc;

const MODEL: &str = "mock/test";
const WAIT: Duration = Duration::from_secs(60);

// ---------------------------------------------------------------------------------------
// Fixtures

fn catalog(priced: bool) -> Catalog {
    let price = if priced {
        format!(
            "[models.\"{MODEL}\".price]\ninput = 3.0\noutput = 15.0\ncache_read = 0.3\n\
             cache_write = 3.75\n"
        )
    } else {
        String::new()
    };
    Catalog::from_toml(&format!(
        r#"[models."{MODEL}"]
context_window = 200000
max_output = 8000
parallel_tools = false
reasoning = "none"
cache = "none"
images = false
edit_format = "str_replace"
{price}"#
    ))
    .unwrap()
}

fn config(mode: Mode, priced: bool) -> Config {
    let mut c = Config {
        model: ModelId::from(MODEL),
        catalog_overrides: Some(catalog(priced)),
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

fn bash(id: &str, command: &str) -> ProviderEvent {
    call(id, "bash", json!({ "command": command }))
}

fn usage() -> ProviderEvent {
    ProviderEvent::Usage(RawUsage {
        input: 1000,
        output: 100,
        cache_read: 0,
        cache_write: 0,
    })
}

fn step(events: Vec<ProviderEvent>) -> MockTurn {
    let mut evs = events;
    evs.push(usage());
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

fn text(s: &str) -> Line {
    Line::Text(s.into())
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

    fn opts(&self) -> ReplOptions {
        ReplOptions {
            color: false,
            footer: true,
            history: Some(self.home.path().join("repl_history")),
            root: self.root(),
            data_home: Some(self.home.path().to_path_buf()),
            model: Some(MODEL.into()),
            models: None,
        }
    }
}

/// What a factory did: the `resume` of every call, and every session log it opened.
#[derive(Clone, Default)]
struct Built {
    resumes: Arc<Mutex<Vec<Option<String>>>>,
    logs: Arc<Mutex<Vec<PathBuf>>>,
}

fn factory(
    fx: &Fx,
    cfg: Config,
    mock: &MockProvider,
    workspace: Option<Arc<Workspace>>,
    built: &Built,
) -> EngineFactory {
    let (root, home, mock, built) = (
        fx.root(),
        fx.home.path().to_path_buf(),
        mock.clone(),
        built.clone(),
    );
    Box::new(move |resume| {
        built.resumes.lock().unwrap().push(resume.clone());
        let (engine, events) = EngineBuilder {
            provider: Some(Arc::new(mock.clone())),
            data_home: Some(home.clone()),
            interactive: true,
            resume,
            workspace: workspace.clone(),
            ..EngineBuilder::new(cfg.clone(), root.clone())
        }
        .build()?;
        built
            .logs
            .lock()
            .unwrap()
            .push(engine.session_path().to_path_buf());
        Ok((engine, events))
    })
}

/// An `AsyncWrite` into a shared buffer.
#[derive(Clone, Default)]
struct Buf(Arc<Mutex<Vec<u8>>>);

impl Buf {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

impl AsyncWrite for Buf {
    fn poll_write(self: Pin<&mut Self>, _: &mut Context<'_>, b: &[u8]) -> Poll<io::Result<usize>> {
        self.0.lock().unwrap().extend_from_slice(b);
        Poll::Ready(Ok(b.len()))
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

/// The REPL's output and the prompts it showed, after it ran `lines`.
struct Ran {
    out: String,
    prompts: Vec<String>,
}

async fn run_with(lines: Vec<Line>, factory: EngineFactory, opts: ReplOptions) -> Ran {
    let source = ScriptedSource::new(lines);
    let prompts = source.prompts();
    let buf = Buf::default();
    let done = run_repl(
        factory,
        source,
        buf.clone(),
        futures::stream::pending(),
        opts,
    );
    tokio::time::timeout(WAIT, done)
        .await
        .expect("the REPL did not finish")
        .expect("the REPL failed");
    let prompts = prompts.lock().unwrap().clone();
    Ran {
        out: buf.text(),
        prompts,
    }
}

async fn run(fx: &Fx, lines: Vec<Line>, cfg: Config, mock: &MockProvider) -> Ran {
    let f = factory(fx, cfg, mock, None, &Built::default());
    run_with(lines, f, fx.opts()).await
}

/// The commands a session log recorded.
fn logged_commands(log: &Path) -> Vec<Command> {
    let reader = SessionReader::<SessionRecord>::open(log).unwrap();
    reader
        .records()
        .filter_map(|r| match r.ok()?.1 {
            SessionRecord::Command(c) => Some(c),
            _ => None,
        })
        .collect()
}

fn records(log: &Path) -> Vec<SessionRecord> {
    let reader = SessionReader::<SessionRecord>::open(log).unwrap();
    reader
        .records()
        .filter_map(|r| r.ok().map(|r| r.1))
        .collect()
}

fn count(hay: &str, needle: &str) -> usize {
    hay.matches(needle).count()
}

// ---------------------------------------------------------------------------------------
// Rendering

fn plain() -> ReplOptions {
    ReplOptions {
        footer: true,
        ..ReplOptions::default()
    }
}

fn render_all(events: &[Event], st: &mut RenderState, opts: &ReplOptions) -> String {
    events
        .iter()
        .filter_map(|e| render::event(e, st, opts))
        .collect()
}

fn finished(call_id: &str, tool: &str, output: &str, is_error: bool) -> Event {
    Event::ToolFinished {
        call_id: call_id.into(),
        tool: tool.into(),
        output: output.into(),
        is_error,
        blocked: false,
        effects: vec![],
        meta: ToolMetaView::default(),
        diff: None,
    }
}

fn proposed(call_id: &str, tool: &str, args: Value) -> Event {
    Event::ToolCallProposed {
        call_id: call_id.into(),
        tool: tool.into(),
        args,
    }
}

fn started(call_id: &str, tool: &str) -> Event {
    Event::ToolStarted {
        call_id: call_id.into(),
        tool: tool.into(),
    }
}

#[tokio::test]
async fn prints_streamed_text() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![MockTurn::Events(vec![
        ProviderEvent::TextDelta("Hel".into()),
        ProviderEvent::TextDelta("lo\nworld".into()),
        ProviderEvent::TextDelta(" \u{1b}[2Jbye".into()),
        ProviderEvent::Done {
            stop: StopReason::EndTurn,
        },
    ])]);
    let ran = run(&fx, vec![text("hi")], config(Mode::Ask, false), &mock).await;
    assert!(ran.out.contains("Hello\nworld"), "{}", ran.out);
    // The model's text never drives the terminal.
    assert!(!ran.out.contains('\u{1b}'), "{:?}", ran.out);
    assert!(ran.out.contains("\\u{1b}[2Jbye"), "{}", ran.out);
}

#[tokio::test]
async fn reasoning_shows_one_thinking_line_per_step() {
    let fx = Fx::new();
    std::fs::write(fx.dir.path().join("a.txt"), "alpha\n").unwrap();
    let mock = MockProvider::new(vec![
        step(vec![
            ProviderEvent::ReasoningDelta("secret plan ".into()),
            ProviderEvent::ReasoningDelta("more plan".into()),
            call("r1", "read", json!({"path": "a.txt"})),
        ]),
        MockTurn::Events(vec![
            ProviderEvent::ReasoningDelta("again ".into()),
            ProviderEvent::ReasoningDelta("and again".into()),
            ProviderEvent::TextDelta("done".into()),
            ProviderEvent::Done {
                stop: StopReason::EndTurn,
            },
        ]),
    ]);
    let ran = run(&fx, vec![text("go")], config(Mode::Ask, false), &mock).await;
    assert_eq!(count(&ran.out, "thinking…"), 2, "{}", ran.out);
    assert!(!ran.out.contains("secret plan"), "{}", ran.out);
    assert!(ran.out.contains("done"), "{}", ran.out);
}

#[test]
fn tool_row_format() {
    let t0 = Instant::now();
    let now = Arc::new(Mutex::new(t0));
    let mut st = RenderState::default();
    let clock = Arc::clone(&now);
    st.set_clock(move || *clock.lock().unwrap());
    let opts = plain();
    let mut row = |events: Vec<Event>, secs: f64| {
        *now.lock().unwrap() = t0;
        let mut out = String::new();
        for (i, e) in events.iter().enumerate() {
            if i == events.len() - 1 {
                *now.lock().unwrap() = t0 + Duration::from_secs_f64(secs);
            }
            out.extend(render::event(e, &mut st, &opts));
        }
        out
    };

    let mut ok = finished("c1", "bash", "test result: ok", false);
    if let Event::ToolFinished { meta, .. } = &mut ok {
        meta.exit_code = Some(0);
    }
    let out = row(
        vec![
            proposed("c1", "bash", json!({"command": "cargo  test -p x"})),
            started("c1", "bash"),
            ok,
        ],
        2.1,
    );
    assert!(
        out.contains("ok bash cargo test -p x 2.1s · exit 0"),
        "{out:?}"
    );

    // Targets are cut to 60 columns.
    let long = format!("echo {}", "x".repeat(100));
    let out = row(
        vec![
            proposed("c2", "bash", json!({ "command": long })),
            started("c2", "bash"),
            finished("c2", "bash", "", false),
        ],
        0.2,
    );
    let line = out.lines().find(|l| l.contains("echo xxx")).unwrap();
    let target = line
        .trim_start_matches("ok bash ")
        .split(" 0.2s")
        .next()
        .unwrap();
    assert_eq!(target.chars().count(), 60, "{line:?}");
    assert!(target.ends_with('…'), "{line:?}");

    let mut failed = finished("c3", "bash", "boom", true);
    if let Event::ToolFinished { meta, .. } = &mut failed {
        meta.exit_code = Some(1);
    }
    let mut blocked = finished("c6", "bash", "refused: same failure", true);
    if let Event::ToolFinished { blocked: b, .. } = &mut blocked {
        *b = true;
    }
    let cases = [
        (failed, "err bash false", "exit 1"),
        (
            finished("c4", "bash", "denied by user", true),
            "denied bash false",
            "",
        ),
        (
            finished(
                "c5",
                "bash",
                "cancelled: the turn was cancelled before this call ran",
                true,
            ),
            "cancelled bash false",
            "",
        ),
        (blocked, "blocked bash false", ""),
    ];
    for (fin, want, note) in cases {
        let Event::ToolFinished { call_id, .. } = &fin else {
            unreachable!()
        };
        let id = call_id.clone();
        let out = row(
            vec![proposed(&id, "bash", json!({"command": "false"})), fin],
            0.0,
        );
        assert!(out.contains(want), "{want}: {out:?}");
        assert!(out.contains(note), "{note}: {out:?}");
    }

    // Edits note their diff stat.
    let mut edit = finished("e1", "edit", "edited", false);
    if let Event::ToolFinished { diff, effects, .. } = &mut edit {
        *diff = Some(DiffPreview {
            unified: "--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1,2 +1,3 @@\n-a\n+b\n+c\n d\n".into(),
            lines: 7,
            truncated: false,
        });
        *effects = vec![EffectView::Edited {
            path: "src/a.rs".into(),
            created: false,
            post_hash: "0".repeat(64),
        }];
    }
    let out = row(
        vec![
            proposed(
                "e1",
                "edit",
                json!({"path": "src/a.rs", "old_text": "a", "new_text": "b\nc"}),
            ),
            started("e1", "edit"),
            edit,
        ],
        0.3,
    );
    assert!(out.contains("ok edit src/a.rs 0.3s · +2 -1"), "{out:?}");

    // A filtered command notes what the filter saved, as an estimate.
    let mut filtered = finished("f1", "bash", "12 passed", false);
    if let Event::ToolFinished { meta, .. } = &mut filtered {
        meta.exit_code = Some(0);
        meta.saved = Some(Saved {
            kind: SaveKind::Filter,
            bytes: 8000,
        });
    }
    let out = row(
        vec![
            proposed("f1", "bash", json!({"command": "cargo test"})),
            started("f1", "bash"),
            filtered,
        ],
        1.0,
    );
    assert!(
        out.contains("ok bash cargo test 1.0s · exit 0 · filtered, saved ~2.0k tok (est.)"),
        "{out:?}"
    );
}

#[test]
fn coloured_diff_only_with_color_option() {
    let ev = Event::SessionDiff {
        turn: None,
        files: vec![DiffFile {
            path: "a.rs".into(),
            status: DiffStatus::Modified,
            added: 1,
            removed: 1,
        }],
        unified: "--- a/a.rs\n+++ b/a.rs\n@@ -1,2 +1,2 @@\n ctx\n-old\n+new \u{1b}[2J\n".into(),
        truncated: false,
    };
    let coloured = render::event(
        &ev,
        &mut RenderState::default(),
        &ReplOptions {
            color: true,
            ..plain()
        },
    )
    .unwrap();
    assert!(coloured.contains("\u{1b}[32m+new"), "{coloured:?}");
    assert!(coloured.contains("\u{1b}[31m-old"), "{coloured:?}");
    assert!(coloured.contains("\u{1b}[2m ctx"), "{coloured:?}");
    let bare = render::event(&ev, &mut RenderState::default(), &plain()).unwrap();
    assert!(!bare.contains('\u{1b}'), "{bare:?}");
    assert!(
        bare.contains("+new \\u{1b}[2J") && bare.contains("-old"),
        "{bare:?}"
    );
    // Content never carries its own escape sequence through, in either mode.
    assert!(!coloured.contains("\u{1b}[2J"), "{coloured:?}");
}

const FOOTER: &str = r"turn \d+ · \d+ steps · [\d.]+s · ctx \d+/\d+ tok · cache \d+% · \$\d+\.\d{4} \(session \$\d+\.\d{4}\) · saved ~\S+ tok \(est\.\)";

#[tokio::test]
async fn footer_after_each_turn() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![text_step("one"), text_step("two")]);
    let ran = run(
        &fx,
        vec![text("first"), text("second")],
        config(Mode::Ask, true),
        &mock,
    )
    .await;
    let re = Regex::new(FOOTER).unwrap();
    let footers: Vec<&str> = re.find_iter(&ran.out).map(|m| m.as_str()).collect();
    assert_eq!(footers.len(), 2, "{}", ran.out);
    assert!(footers[0].starts_with("turn 1 · 1 steps"), "{footers:?}");
    assert!(footers[1].starts_with("turn 2 · "), "{footers:?}");
    // The session total adds up the turns.
    assert!(
        footers[1].contains("$0.0045 (session $0.0090)"),
        "{footers:?}"
    );
}

#[tokio::test]
async fn footer_hidden_when_option_off() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![text_step("one")]);
    let f = factory(&fx, config(Mode::Ask, true), &mock, None, &Built::default());
    let ran = run_with(
        vec![text("first")],
        f,
        ReplOptions {
            footer: false,
            ..fx.opts()
        },
    )
    .await;
    assert!(ran.out.contains("one"), "{}", ran.out);
    assert!(
        !Regex::new(FOOTER).unwrap().is_match(&ran.out),
        "{}",
        ran.out
    );
    assert!(!ran.out.contains("turn 1 ·"), "{}", ran.out);
}

fn evidence(stale: bool, edits_since: u32) -> TestEvidence {
    TestEvidence {
        command: "cargo test".into(),
        exit_code: Some(0),
        passed: Some(3),
        failed: Some(0),
        step: 2,
        write_gen: 1,
        stale,
        edits_since,
        execs_since: 0,
    }
}

#[test]
fn evidence_line_fresh_stale_and_none() {
    let opts = plain();
    let end = |evidence| Event::TurnFinished {
        reason: FinishReason::Completed,
        evidence,
    };
    let mut st = RenderState::default();
    let out = render_all(
        &[Event::TurnStarted, end(Some(evidence(false, 0)))],
        &mut st,
        &opts,
    );
    assert!(
        out.contains("last test: cargo test · exit 0 · fresh"),
        "{out:?}"
    );
    let out = render_all(
        &[Event::TurnStarted, end(Some(evidence(true, 2)))],
        &mut st,
        &opts,
    );
    assert!(
        out.contains("last test: cargo test · exit 0 · stale (2 edits since)"),
        "{out:?}"
    );
    // A turn that edited without testing says so; one that changed nothing says nothing.
    let mut edit = finished("e1", "write", "wrote", false);
    if let Event::ToolFinished { effects, .. } = &mut edit {
        *effects = vec![EffectView::Edited {
            path: "a.txt".into(),
            created: true,
            post_hash: "0".repeat(64),
        }];
    }
    let out = render_all(&[Event::TurnStarted, edit, end(None)], &mut st, &opts);
    assert!(out.contains("no test run this turn"), "{out:?}");
    let out = render_all(&[Event::TurnStarted, end(None)], &mut st, &opts);
    assert!(!out.contains("test"), "{out:?}");
}

// ---------------------------------------------------------------------------------------
// Approvals

fn write_call(id: &str, path: &str, content: &str) -> ProviderEvent {
    call(id, "write", json!({"path": path, "content": content}))
}

#[tokio::test]
async fn approval_prompt_shows_diff_and_y_approves_n_denies() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![
        step(vec![write_call("w1", "a.txt", "hello\n")]),
        step(vec![write_call("w2", "b.txt", "bye\n")]),
        text_step("done"),
    ]);
    let ran = run(
        &fx,
        vec![text("write them"), text("y"), text("n")],
        config(Mode::Ask, false),
        &mock,
    )
    .await;
    assert!(ran.out.contains("+hello"), "{}", ran.out);
    assert!(ran.out.contains("+bye"), "{}", ran.out);
    let asks: Vec<&String> = ran
        .prompts
        .iter()
        .filter(|p| p.contains("allow?"))
        .collect();
    assert_eq!(asks.len(), 2, "{:?}", ran.prompts);
    assert!(
        asks[0].contains("allow? [y]es [n]o [a]lways this session (write(a.txt))"),
        "{asks:?}"
    );
    assert!(fx.dir.path().join("a.txt").exists());
    assert!(!fx.dir.path().join("b.txt").exists());
    assert!(ran.out.contains("ok write a.txt"), "{}", ran.out);
    assert!(ran.out.contains("denied write b.txt"), "{}", ran.out);
}

#[tokio::test]
async fn approval_a_sends_exact_session_rule() {
    assert_eq!(
        exact_rule("bash", &json!({"command": "cargo  test\t -p x"})).as_deref(),
        Some("bash(cargo test -p x)")
    );
    assert_eq!(
        exact_rule("bash", &json!({"command": "ls *.rs [a]?"})).as_deref(),
        Some("bash(ls \\*.rs \\[a\\]\\?)")
    );
    assert_eq!(
        exact_rule("bash", &json!({"command": "echo $(date)"})),
        None
    );
    assert_eq!(
        exact_rule(
            "edit",
            &json!({"path": "./src/a.rs", "old_text": "", "new_text": ""})
        )
        .as_deref(),
        Some("edit(src/a.rs)")
    );
    assert_eq!(
        exact_rule("write", &json!({"path": "b.txt", "content": ""})).as_deref(),
        Some("write(b.txt)")
    );
    assert_eq!(
        exact_rule("edit", &json!({"patch": "*** Begin Patch"})),
        None
    );
    assert_eq!(
        exact_rule("web_fetch", &json!({"url": "https://Docs.RS:443/x?y"})).as_deref(),
        Some("web_fetch(docs.rs)")
    );
    assert_eq!(
        exact_rule("mcp__gh__create_issue", &json!({})).as_deref(),
        Some("mcp__gh__create_issue")
    );
    assert_eq!(exact_rule("grep", &json!({"pattern": "x"})), None);

    let fx = Fx::new();
    let mock = MockProvider::new(vec![
        step(vec![bash("b1", "echo  hi")]),
        step(vec![bash("b2", "echo hi")]),
        text_step("done"),
    ]);
    let ran = run(
        &fx,
        vec![text("go"), text("a")],
        config(Mode::Ask, false),
        &mock,
    )
    .await;
    let asks: Vec<&String> = ran
        .prompts
        .iter()
        .filter(|p| p.contains("allow?"))
        .collect();
    assert_eq!(
        asks.len(),
        1,
        "the second call is not asked: {:?}",
        ran.prompts
    );
    assert!(asks[0].contains("(bash(echo hi))"), "{asks:?}");
    assert_eq!(count(&ran.out, "ok bash echo hi"), 2, "{}", ran.out);
    assert!(
        !ran.out.contains("session rule"),
        "the engine took the rule: {}",
        ran.out
    );
}

#[tokio::test]
async fn approval_a_with_pattern_sends_user_rule() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![
        step(vec![bash("b1", "echo hi")]),
        step(vec![bash("b2", "echo there")]),
        text_step("done"),
    ]);
    let ran = run(
        &fx,
        vec![text("go"), text("a bash(echo*)")],
        config(Mode::Ask, false),
        &mock,
    )
    .await;
    let asks = ran.prompts.iter().filter(|p| p.contains("allow?")).count();
    assert_eq!(asks, 1, "{:?}", ran.prompts);
    assert!(ran.out.contains("ok bash echo there"), "{}", ran.out);
    assert!(!ran.out.contains("session rule"), "{}", ran.out);
}

// ---------------------------------------------------------------------------------------
// Slash commands

#[tokio::test]
async fn slash_commands_map() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![
        step(vec![write_call("w1", "a.txt", "a\n")]),
        text_step("wrote a"),
        step(vec![write_call("w2", "b.txt", "b\n")]),
        text_step("wrote b"),
    ]);
    let built = Built::default();
    let f = factory(&fx, config(Mode::AutoEdit, false), &mock, None, &built);
    let ran = run_with(
        vec![
            text("make a"),
            text("/model compat:nowhere/x"),
            text("/diff 2"),
            text("/diff"),
            text("/undo"),
            text("y"),
            text("make b"),
            text("/undo"),
            text("n"),
            text("/compact"),
            text("/pin a"),
            text("/unpin a"),
            text("/memory consolidate"),
            text("/fork"),
        ],
        f,
        fx.opts(),
    )
    .await;
    let log = built.logs.lock().unwrap()[0].clone();
    let got: Vec<Command> = logged_commands(&log)
        .into_iter()
        .filter(|c| !matches!(c, Command::SendMessage { .. } | Command::Shutdown))
        .collect();
    let want = vec![
        Command::SetModel {
            model: ModelId::from("compat:nowhere/x"),
        },
        Command::Diff { turn: Some(2) },
        Command::Diff { turn: None },
        Command::Undo {
            turn: None,
            dry_run: true,
        },
        Command::Undo {
            turn: None,
            dry_run: false,
        },
        Command::Undo {
            turn: None,
            dry_run: true,
        },
        Command::Compact,
        Command::Pin { text: "a".into() },
        Command::Unpin { text: "a".into() },
        Command::ConsolidateMemory,
        Command::Fork {
            at: None,
            label: None,
        },
    ];
    assert_eq!(got, want, "{}", ran.out);
    assert!(
        ran.prompts.iter().filter(|p| p.contains("undo?")).count() == 2,
        "{:?}",
        ran.prompts
    );
    // The first undo ran (a.txt is gone), the second was declined (b.txt stays).
    assert!(!fx.dir.path().join("a.txt").exists());
    assert!(fx.dir.path().join("b.txt").exists());
}

#[tokio::test]
async fn model_without_arg_lists_current_and_models_with_keys() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![]);
    let f = factory(
        &fx,
        config(Mode::Ask, false),
        &mock,
        None,
        &Built::default(),
    );
    let ran = run_with(
        vec![text("/model")],
        f,
        ReplOptions {
            models: Some(Arc::new(|| {
                vec!["anthropic/claude-x".into(), "openai/gpt-y".into()]
            })),
            ..fx.opts()
        },
    )
    .await;
    assert!(ran.out.contains("* mock/test (current)"), "{}", ran.out);
    assert!(ran.out.contains("  anthropic/claude-x"), "{}", ran.out);
    assert!(ran.out.contains("  openai/gpt-y"), "{}", ran.out);
}

#[tokio::test]
async fn cost_prints_turn_and_session_receipt() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![text_step("one"), text_step("two")]);
    let ran = run(
        &fx,
        vec![text("first"), text("second"), text("/cost")],
        config(Mode::Ask, true),
        &mock,
    )
    .await;
    let cost = &ran.out[ran.out.rfind("last turn:").expect("a turn receipt")..];
    assert!(cost.starts_with("last turn: turn 2 · 1 steps"), "{cost}");
    assert!(cost.contains("$0.0045"), "{cost}");
    assert!(
        cost.contains("session: 2 turns · 2 steps · 0 tool calls"),
        "{cost}"
    );
    assert!(cost.contains("$0.0090"), "{cost}");
    assert!(cost.contains("saved ~"), "{cost}");
}

#[tokio::test]
async fn resume_without_arg_lists_last_10_sessions_and_with_arg_rebuilds_engine() {
    let fx = Fx::new();
    let mut ids = Vec::new();
    for i in 0..12 {
        let mock = MockProvider::new(vec![text_step("ok")]);
        let (engine, mut events) = EngineBuilder {
            provider: Some(Arc::new(mock)),
            data_home: Some(fx.home.path().to_path_buf()),
            ..EngineBuilder::new(config(Mode::Ask, false), fx.root())
        }
        .build()
        .unwrap();
        ids.push(engine.session_id().to_string());
        engine
            .send(Command::SendMessage {
                text: format!("task-{i:02}\nmore"),
            })
            .await
            .unwrap();
        while let Some(e) = events.recv().await {
            if matches!(e, Event::TurnFinished { .. }) {
                break;
            }
        }
        engine.send(Command::Shutdown).await.unwrap();
        while events.recv().await.is_some() {}
        // Distinct start times, newest last.
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let mock = MockProvider::new(vec![]);
    let built = Built::default();
    let f = factory(&fx, config(Mode::Ask, false), &mock, None, &built);
    let ran = run_with(
        vec![
            text("/resume"),
            text(&format!("/resume {}", ids[5])),
            text("/resume 1"),
        ],
        f,
        fx.opts(),
    )
    .await;
    let listing = &ran.out[..ran
        .out
        .find(&format!("resumed {}", ids[5]))
        .unwrap_or(ran.out.len())];
    for (i, id) in ids.iter().enumerate().skip(2) {
        assert!(listing.contains(&format!("task-{i:02}")), "{i}: {listing}");
        assert!(listing.contains(id.as_str()), "{i}: {listing}");
    }
    for i in 0..2 {
        assert!(!listing.contains(&format!("task-{i:02}")), "{i}: {listing}");
    }
    assert!(!listing.contains("more"), "only the first line: {listing}");
    let first = listing.find("task-11").unwrap();
    assert!(
        first < listing.find("task-02").unwrap(),
        "newest first: {listing}"
    );
    assert_eq!(
        *built.resumes.lock().unwrap(),
        vec![None, Some(ids[5].clone()), Some(ids[11].clone())]
    );
}

#[tokio::test]
async fn memory_proposal_and_mcp_server_approval_use_y_n() {
    let fx = Fx::new();
    std::fs::create_dir_all(fx.dir.path().join(".tautly")).unwrap();
    std::fs::write(
        fx.dir.path().join(".tautly/config.toml"),
        "[mcp.servers.srv]\ncommand = \"/nonexistent/tautly-test-server\"\n",
    )
    .unwrap();
    let loaded = Config::load_from(None, &fx.root(), Default::default()).unwrap();
    let mut cfg = loaded.config;
    cfg.model = ModelId::from(MODEL);
    cfg.catalog_overrides = Some(catalog(false));
    cfg.permissions.mode = Mode::Ask;
    let sb: Arc<dyn Sandbox> = Arc::new(LocalSandbox::new(fx.root()).unwrap());
    let ws = Workspace::new(sb, fx.home.path());
    ws.start();
    assert!(ws.wait_ready(WAIT).await, "index never became ready");
    for _ in 0..300 {
        if ws.memory().is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let mock = MockProvider::new(vec![
        step(vec![call(
            "m1",
            "remember",
            json!({"title": "Use pnpm", "kind": "convention", "body": "Run pnpm, never npm."}),
        )]),
        text_step("noted"),
    ]);
    let built = Built::default();
    let f = factory(&fx, cfg, &mock, Some(ws), &built);
    let ran = run_with(
        vec![text("remember pnpm"), text("n"), text("y")],
        f,
        fx.opts(),
    )
    .await;
    let questions: Vec<&String> = ran
        .prompts
        .iter()
        .filter(|p| p.contains("[y]es [n]o"))
        .collect();
    assert_eq!(questions.len(), 2, "{:?}\n{}", ran.prompts, ran.out);
    assert!(questions[0].contains("start this server?"), "{questions:?}");
    assert!(!questions[0].contains("[a]lways"), "{questions:?}");
    assert!(questions[1].contains("save to memory?"), "{questions:?}");
    assert!(ran.out.contains("srv"), "{}", ran.out);
    assert!(ran.out.contains("Use pnpm"), "{}", ran.out);
    let log = built.logs.lock().unwrap()[0].clone();
    let recs = records(&log);
    assert!(
        recs.iter()
            .any(|r| matches!(r, SessionRecord::MemoryDecision { approved: true, .. })),
        "{recs:?}"
    );
    assert!(
        recs.iter()
            .any(|r| matches!(r, SessionRecord::Command(Command::Deny { .. }))),
        "{recs:?}"
    );
}

// ---------------------------------------------------------------------------------------
// Keys

/// A REPL fed from a channel, with its SIGINTs from another.
struct Live {
    lines: mpsc::UnboundedSender<Line>,
    sigint: mpsc::UnboundedSender<()>,
    out: Buf,
    done: tokio::task::JoinHandle<Result<(), tautly_tui::ReplError>>,
}

fn live(fx: &Fx, mock: &MockProvider) -> Live {
    let (lines, source) = ScriptedSource::channel();
    let (sigint, signals) = mpsc::unbounded_channel();
    let out = Buf::default();
    let f = factory(fx, config(Mode::Ask, false), mock, None, &Built::default());
    let done = tokio::spawn(run_repl(
        f,
        source,
        out.clone(),
        tokio_stream(signals),
        fx.opts(),
    ));
    Live {
        lines,
        sigint,
        out,
        done,
    }
}

fn tokio_stream(mut rx: mpsc::UnboundedReceiver<()>) -> impl futures::Stream<Item = ()> {
    futures::stream::poll_fn(move |cx| rx.poll_recv(cx))
}

async fn wait_for(out: &Buf, what: &str, times: usize) {
    let deadline = Instant::now() + WAIT;
    while count(&out.text(), what) < times {
        assert!(
            Instant::now() < deadline,
            "never saw {what:?}: {}",
            out.text()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Wait until the provider has been asked `n` times (the turn is under way).
async fn wait_requests(mock: &MockProvider, n: usize) {
    let deadline = Instant::now() + WAIT;
    while mock.requests().len() < n {
        assert!(Instant::now() < deadline, "the provider was never asked");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn ctrl_c_once_cancels_twice_within_2s_exits() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![MockTurn::Stall]);
    let l = live(&fx, &mock);
    l.lines.send(text("go")).unwrap();
    wait_requests(&mock, 1).await;
    l.sigint.send(()).unwrap();
    wait_for(&l.out, "turn cancelled", 1).await;
    // The second Ctrl+C, within 2 s, at the prompt: the REPL exits.
    l.lines.send(Line::CtrlC).unwrap();
    tokio::time::timeout(WAIT, l.done)
        .await
        .expect("the REPL did not exit")
        .unwrap()
        .unwrap();

    // A third one more than 2 s after the last cancels again instead of exiting.
    let fx = Fx::new();
    let mock = MockProvider::new(vec![MockTurn::Stall, MockTurn::Stall]);
    let l = live(&fx, &mock);
    l.lines.send(text("go")).unwrap();
    wait_requests(&mock, 1).await;
    l.sigint.send(()).unwrap();
    wait_for(&l.out, "turn cancelled", 1).await;
    tokio::time::sleep(Duration::from_millis(2100)).await;
    l.lines.send(text("again")).unwrap();
    wait_requests(&mock, 2).await;
    l.sigint.send(()).unwrap();
    wait_for(&l.out, "turn cancelled", 2).await;
    assert!(!l.done.is_finished(), "{}", l.out.text());
    drop(l.lines);
    tokio::time::timeout(WAIT, l.done)
        .await
        .expect("Ctrl+D did not exit")
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn ctrl_c_at_prompt_clears_line_ctrl_d_exits() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![text_step("still here")]);
    let ran = run(
        &fx,
        vec![Line::CtrlC, text("hi"), Line::CtrlD, text("never read")],
        config(Mode::Ask, false),
        &mock,
    )
    .await;
    assert!(ran.out.contains("still here"), "{}", ran.out);
    // One turn only (the Ctrl+C line was not sent).
    assert!(
        ran.out.contains("turn 1 ·") && !ran.out.contains("turn 2 ·"),
        "{}",
        ran.out
    );
    assert_eq!(ran.prompts.len(), 3, "{:?}", ran.prompts);
}

#[tokio::test]
async fn history_written_without_approval_answers() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![step(vec![bash("b1", "echo hi")]), text_step("done")]);
    run(
        &fx,
        vec![
            text("first message"),
            text("y"),
            text("/diff"),
            text("two\nlines"),
        ],
        config(Mode::Ask, false),
        &mock,
    )
    .await;
    let hist = std::fs::read_to_string(fx.home.path().join("repl_history")).unwrap();
    let entries: Vec<&str> = hist.lines().collect();
    assert_eq!(
        entries,
        ["first message", "/diff", "two\\nlines"],
        "{hist:?}"
    );
}

#[tokio::test]
async fn unknown_slash_command_prints_help() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![]);
    let ran = run(&fx, vec![text("/nope")], config(Mode::Ask, false), &mock).await;
    assert!(ran.out.contains("unknown command: /nope"), "{}", ran.out);
    assert!(ran.out.contains("commands:"), "{}", ran.out);
    assert!(ran.out.contains("/memory consolidate"), "{}", ran.out);
    assert_eq!(mock.requests().len(), 0);
}
