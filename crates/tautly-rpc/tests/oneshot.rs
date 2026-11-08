//! `tautly -p`: one message, its events on the output, an exit code from how it ended.

mod common;

use common::*;
use serde_json::{Value, json};
use tautly_core::{CliOverrides, EngineBuilder, FinishReason, Mode};
use tautly_provider::{MockProvider, MockTurn, ProviderError};
use tautly_rpc::{OutputFormat, exit_code, run_oneshot, run_oneshot_to};

/// Run `prompt` through `run_oneshot`; the exit code and what it wrote.
async fn oneshot(fx: &Fx, b: EngineBuilder, prompt: &str, output: OutputFormat) -> (i32, String) {
    let (engine, events) = fx.build(b);
    let mut out = Vec::new();
    let code = tokio::time::timeout(
        WAIT,
        run_oneshot(engine, events, prompt.to_string(), output, &mut out),
    )
    .await
    .expect("the one-shot run did not end");
    (code, String::from_utf8(out).unwrap())
}

/// The events of a JSON run.
fn events(out: &str) -> Vec<Value> {
    out.lines()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("not JSON ({e}): {l}")))
        .collect()
}

fn tool_finished(events: &[Value], call_id: &str) -> Value {
    events
        .iter()
        .find(|e| e["type"] == "tool_finished" && e["call_id"] == call_id)
        .unwrap_or_else(|| panic!("no tool_finished for {call_id}: {events:?}"))
        .clone()
}

fn warnings(events: &[Value]) -> Vec<String> {
    events
        .iter()
        .filter(|e| e["type"] == "warning")
        .map(|e| e["message"].as_str().unwrap().to_string())
        .collect()
}

/// The event stream with what differs between runs replaced: session ids, the temporary
/// root, and the context numbers (which depend on whether the repo map was ready), the
/// estimated usage of a response that reported none included.
fn normalized(out: &str, fx: &Fx) -> String {
    let root = fx.root().to_string_lossy().into_owned();
    let mut lines = Vec::new();
    for mut ev in events(out) {
        if ev["type"] == "session_changed" {
            ev["session_id"] = json!("[session]");
        }
        if ev["type"] == "usage" && ev["estimated"] == true {
            for key in ["tokens_in", "tokens_out"] {
                ev[key] = json!("[n]");
            }
        }
        if ev["type"] == "turn_receipt" {
            ev["wall_ms"] = json!("[ms]");
            if ev["estimated"] == true {
                for key in ["tokens_in", "tokens_out"] {
                    ev[key] = json!("[n]");
                }
            }
        }
        if ev["type"] == "context_status" {
            for key in ["used", "budget", "breakdown"] {
                ev[key] = json!("[n]");
            }
        }
        lines.push(ev.to_string().replace(&root, "[root]"));
    }
    lines.join("\n")
}

#[tokio::test]
async fn oneshot_json_golden() {
    let fx = Fx::new();
    fx.write("a.txt", "hello\n");
    let mock = MockProvider::new(vec![
        tools_step(vec![call("c1", "read", json!({"path": "a.txt"}))]),
        text_step("The file says hello."),
    ]);
    // A ready workspace: the memory store is open, so the session-end episode is always
    // attempted (and, the script being used up, reported as not written).
    let b = EngineBuilder {
        workspace: Some(fx.ready_workspace().await),
        ..fx.builder(config(Mode::Ask), &mock)
    };
    let (code, out) = oneshot(&fx, b, "what does a.txt say?", OutputFormat::Json).await;
    assert_eq!(code, 0, "{out}");
    let evs = events(&out);
    assert_eq!(evs.first().unwrap()["type"], "session_changed");
    assert!(evs.iter().any(|e| e["type"] == "turn_finished"));
    // Warnings from the session's start-up tasks are left out: they run beside the turn,
    // so whether they come (the repo map not ready for the first request, a staleness
    // check that hit a busy index, a watcher that did not start) and where they land in
    // the stream depend on the machine's load.
    let all = normalized(&out, &fx);
    let startup = [
        "repo map",
        "memory index",
        "memory staleness",
        "code index",
        "file watcher",
        "project memory",
    ];
    let stable: Vec<&str> = all
        .lines()
        .filter(|l| !(l.contains(r#""type":"warning""#) && startup.iter().any(|s| l.contains(s))))
        .collect();
    insta::assert_snapshot!("oneshot_json", stable.join("\n"));
}

#[tokio::test]
async fn oneshot_text_prints_text_only() {
    let fx = Fx::new();
    fx.write("a.txt", "hello\n");
    let mock = MockProvider::new(vec![
        tools_step(vec![call("c1", "read", json!({"path": "a.txt"}))]),
        MockTurn::Events(vec![
            tautly_provider::ProviderEvent::TextDelta("Hello".into()),
            tautly_provider::ProviderEvent::TextDelta(", world.".into()),
            tautly_provider::ProviderEvent::Done {
                stop: tautly_provider::StopReason::EndTurn,
            },
        ]),
    ]);
    let (code, out) = oneshot(
        &fx,
        fx.builder(config(Mode::Ask), &mock),
        "greet",
        OutputFormat::Text,
    )
    .await;
    assert_eq!(code, 0);
    assert_eq!(out, "Hello, world.\n");
}

/// Run `prompt` through `run_oneshot_to`; the exit code, stdout and stderr.
async fn oneshot_split(
    fx: &Fx,
    b: EngineBuilder,
    prompt: &str,
    output: OutputFormat,
) -> (i32, String, String) {
    let (engine, events) = fx.build(b);
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = tokio::time::timeout(
        WAIT,
        run_oneshot_to(
            engine,
            events,
            prompt.to_string(),
            output,
            &mut out,
            &mut err,
        ),
    )
    .await
    .expect("the one-shot run did not end");
    (
        code,
        String::from_utf8(out).unwrap(),
        String::from_utf8(err).unwrap(),
    )
}

#[tokio::test]
async fn oneshot_text_reports_headless_refusals_on_stderr() {
    let script = || {
        MockProvider::new(vec![
            tools_step(vec![
                call("w1", "write", json!({"path": "out.txt", "content": "x"})),
                call(
                    "m1",
                    "remember",
                    json!({"title": "Use pnpm", "kind": "convention",
                           "body": "Run pnpm install, never npm."}),
                ),
            ]),
            text_step("could not write"),
        ])
    };
    let fixture = || {
        let fx = Fx::new();
        // A repository-declared MCP server nobody approved: skipped headless.
        fx.write(
            ".tautly/config.toml",
            "[mcp.servers.echo]\ncommand = \"/nonexistent/echo-server\"\n",
        );
        fx
    };
    let fx = fixture();
    let mock = script();
    let cfg = config_with(CliOverrides::default(), fx.dir.path());
    let b = EngineBuilder {
        workspace: Some(fx.ready_workspace().await),
        ..fx.builder(cfg, &mock)
    };
    let (code, out, err) = oneshot_split(&fx, b, "write", OutputFormat::Text).await;
    assert_eq!(code, 0, "{err}");
    // Stdout stays the model's text only.
    assert_eq!(out, "could not write\n", "{err}");
    let lines: Vec<&str> = err.lines().collect();
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("tautly: write denied: ") && l.contains("--approve auto-edit")),
        "{err}"
    );
    assert!(
        !lines.iter().any(|l| l.contains("denied: denied:")),
        "{err}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("tautly: warning: ") && l.contains("--approve-memory")),
        "{err}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("tautly: MCP server echo skipped: ")
                && l.contains("--approve-mcp")),
        "{err}"
    );
    assert!(!fx.exists("out.txt"));

    // As JSON, everything is in the event stream and stderr stays empty.
    let fx = fixture();
    let mock = script();
    let cfg = config_with(CliOverrides::default(), fx.dir.path());
    let b = EngineBuilder {
        workspace: Some(fx.ready_workspace().await),
        ..fx.builder(cfg, &mock)
    };
    let (code, out, err) = oneshot_split(&fx, b, "write", OutputFormat::Json).await;
    assert_eq!(code, 0, "{out}");
    assert_eq!(err, "");
    assert!(!warnings(&events(&out)).is_empty(), "{out}");
}

#[tokio::test]
async fn exit_codes() {
    for (reason, code) in [
        (FinishReason::Completed, 0),
        (FinishReason::Error, 1),
        (FinishReason::Budget, 2),
        (FinishReason::MaxTurns, 2),
        (FinishReason::Timeout, 2),
        (FinishReason::Cancelled, 130),
        (FinishReason::Unknown, 1),
    ] {
        assert_eq!(exit_code(reason), code, "{reason:?}");
    }

    // End to end: an error ends the turn with 1 ...
    let fx = Fx::new();
    let mock = MockProvider::new(vec![MockTurn::Error(ProviderError::Auth("bad key".into()))]);
    let (code, out) = oneshot(
        &fx,
        fx.builder(config(Mode::Ask), &mock),
        "hi",
        OutputFormat::Json,
    )
    .await;
    assert_eq!(code, 1, "{out}");
    assert!(events(&out).iter().any(|e| e["type"] == "error"));

    // ... the step cap with 2 ...
    let fx = Fx::new();
    fx.write("a.txt", "x\n");
    let mock = MockProvider::new(vec![
        tools_step(vec![call("c1", "read", json!({"path": "a.txt"}))]),
        tools_step(vec![call("c2", "read", json!({"path": "a.txt"}))]),
    ]);
    let mut cfg = config(Mode::Ask);
    cfg.max_turns = 1;
    let (code, out) = oneshot(&fx, fx.builder(cfg, &mock), "loop", OutputFormat::Json).await;
    assert_eq!(code, 2, "{out}");

    // ... and a session that cannot be resumed never starts (the CLI exits 1 on that).
    let fx = Fx::new();
    let mock = MockProvider::new(vec![]);
    let b = fx.builder(config(Mode::Ask), &mock);
    let made = EngineBuilder {
        resume: Some("01NOSUCHSESSION".into()),
        ..b
    }
    .build();
    assert!(
        made.is_err(),
        "resuming an unknown session must fail to build"
    );
}

#[tokio::test]
async fn headless_ask_becomes_deny_without_approve_flag() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![
        tools_step(vec![call(
            "w1",
            "write",
            json!({"path": "out.txt", "content": "x"}),
        )]),
        text_step("could not write"),
    ]);
    let cfg = config_with(CliOverrides::default(), fx.dir.path());
    let (code, out) = oneshot(&fx, fx.builder(cfg, &mock), "write", OutputFormat::Json).await;
    assert_eq!(code, 0, "{out}");
    let evs = events(&out);
    assert!(
        !evs.iter().any(|e| e["type"] == "tool_approval_requested"),
        "headless never asks: {out}"
    );
    let finished = tool_finished(&evs, "w1");
    assert_eq!(finished["is_error"], true, "{finished}");
    assert!(!fx.exists("out.txt"));
}

#[tokio::test]
async fn headless_with_approve_auto_edit_allows_edits() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![
        tools_step(vec![call(
            "w1",
            "write",
            json!({"path": "out.txt", "content": "x"}),
        )]),
        text_step("written"),
    ]);
    let cfg = config_with(
        CliOverrides {
            mode: Some(Mode::AutoEdit),
            ..CliOverrides::default()
        },
        fx.dir.path(),
    );
    let (code, out) = oneshot(&fx, fx.builder(cfg, &mock), "write", OutputFormat::Json).await;
    assert_eq!(code, 0, "{out}");
    let finished = tool_finished(&events(&out), "w1");
    assert_eq!(finished["is_error"], false, "{finished}");
    assert_eq!(
        std::fs::read_to_string(fx.dir.path().join("out.txt")).unwrap(),
        "x"
    );
}

fn remember_then_done() -> MockProvider {
    MockProvider::new(vec![
        tools_step(vec![call(
            "m1",
            "remember",
            json!({"title": "Use pnpm", "kind": "convention",
                   "body": "Run pnpm install, never npm.", "tags": ["build"]}),
        )]),
        text_step("noted"),
    ])
}

fn memory_files(fx: &Fx) -> usize {
    std::fs::read_dir(fx.dir.path().join(".tautly/memory"))
        .map(|d| {
            d.filter(|e| {
                e.as_ref()
                    .unwrap()
                    .path()
                    .extension()
                    .is_some_and(|x| x == "md")
            })
            .count()
        })
        .unwrap_or(0)
}

#[tokio::test]
async fn headless_memory_proposal_rejected_without_flag_and_written_with_approve_memory() {
    // Without `--approve-memory`: rejected, and said.
    let fx = Fx::new();
    let mock = remember_then_done();
    let cfg = config_with(CliOverrides::default(), fx.dir.path());
    let b = EngineBuilder {
        workspace: Some(fx.ready_workspace().await),
        ..fx.builder(cfg, &mock)
    };
    let (code, out) = oneshot(&fx, b, "remember", OutputFormat::Json).await;
    assert_eq!(code, 0, "{out}");
    let evs = events(&out);
    assert!(!evs.iter().any(|e| e["type"] == "memory_proposed"), "{out}");
    assert!(
        warnings(&evs)
            .iter()
            .any(|w| w.contains("--approve-memory")),
        "{out}"
    );
    assert_eq!(memory_files(&fx), 0);

    // With it: written without asking.
    let fx = Fx::new();
    let mock = remember_then_done();
    let cfg = config_with(
        CliOverrides {
            approve_memory: Some(true),
            ..CliOverrides::default()
        },
        fx.dir.path(),
    );
    let b = EngineBuilder {
        workspace: Some(fx.ready_workspace().await),
        ..fx.builder(cfg, &mock)
    };
    let (code, out) = oneshot(&fx, b, "remember", OutputFormat::Json).await;
    assert_eq!(code, 0, "{out}");
    assert!(!events(&out).iter().any(|e| e["type"] == "memory_proposed"));
    assert_eq!(memory_files(&fx), 1, "{out}");
}
