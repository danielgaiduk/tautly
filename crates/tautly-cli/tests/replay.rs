//! `tautly replay` (spec 7.12.5): a session played back from its log, with no provider
//! built and no key needed, run in a scratch `TAUTLY_HOME` and work tree.

use std::path::PathBuf;
use std::process::{Command, Output};

use serde_json::{Value, json};
use tautly_core::{FinishReason, ProviderEvent, SessionRecord};
use tautly_session::{HeaderInit, SessionWriter};
use tempfile::TempDir;

const KEY_VARS: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "OPENAI_API_KEY",
    "GEMINI_API_KEY",
    "OPENROUTER_API_KEY",
];

struct Scratch {
    home: TempDir,
    work: TempDir,
}

impl Scratch {
    fn new() -> Scratch {
        Scratch {
            home: tempfile::tempdir().unwrap(),
            work: tempfile::tempdir().unwrap(),
        }
    }

    fn root(&self) -> PathBuf {
        self.work.path().canonicalize().unwrap()
    }

    fn run(&self, args: &[&str]) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_tautly"));
        cmd.args(args)
            .current_dir(self.work.path())
            .env("TAUTLY_HOME", self.home.path())
            .env("HOME", self.home.path())
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_DATA_HOME");
        for v in KEY_VARS {
            cmd.env_remove(v);
        }
        cmd.output().unwrap()
    }

    /// A session of this work tree whose model wrote terminal escapes and ran a command.
    fn session(&self) -> String {
        let mut w = SessionWriter::<SessionRecord>::create_in(
            self.home.path(),
            &self.root(),
            HeaderInit {
                model: Some("anthropic/claude-sonnet-5-5".into()),
                ..HeaderInit::default()
            },
        )
        .unwrap();
        let output = w.blob(b"hi\n\x1b[2J").unwrap();
        for r in [
            SessionRecord::TurnStart {
                turn: 1,
                text: "say hi".into(),
            },
            SessionRecord::Provider(ProviderEvent::TextDelta(
                "hello \x1b]0;pwned\x07world\n".into(),
            )),
            SessionRecord::ToolCall {
                id: "b1".into(),
                name: "bash".into(),
                args: json!({"command": "echo hi"}),
            },
            SessionRecord::ToolResult {
                id: "b1".into(),
                result_blob: output,
                is_error: false,
                effects: vec![],
                meta: Default::default(),
                ran: true,
                blocked: false,
            },
            SessionRecord::TurnEnd {
                reason: FinishReason::Completed,
                evidence: None,
            },
        ] {
            w.append(&r).unwrap();
        }
        w.checkpoint().unwrap();
        w.session_id().to_string()
    }
}

fn ok(out: &Output) -> String {
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout.clone()).unwrap()
}

#[test]
fn cli_replay_without_api_key() {
    let s = Scratch::new();
    let id = s.session();

    // JSON: the inner events, one per line, as logged.
    let json_out = ok(&s.run(&["replay", &id, "--speed", "0", "--output", "json"]));
    let events: Vec<Value> = json_out
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let types: Vec<&str> = events.iter().map(|e| e["type"].as_str().unwrap()).collect();
    assert_eq!(
        types,
        [
            "user_message",
            "turn_started",
            "text_delta",
            "tool_call_proposed",
            "tool_finished",
            "turn_finished"
        ],
        "{json_out}"
    );
    assert_eq!(events[2]["text"], "hello \u{1b}]0;pwned\u{7}world\n");
    assert_eq!(events[4]["output"], "hi\n\u{1b}[2J");

    // Text: the REPL's rendering, escaped (no control character reaches the terminal).
    let text = ok(&s.run(&["replay", &id[..10], "--speed", "0"]));
    assert!(!text.contains('\x1b') && !text.contains('\x07'), "{text:?}");
    assert!(text.contains("say hi"), "{text}");
    assert!(text.contains("hello"), "{text}");
    assert!(text.contains("ok bash echo hi"), "{text}");
}

#[test]
fn cli_replay_refuses_unknown_sessions_and_bad_steps() {
    let s = Scratch::new();
    let id = s.session();
    let out = s.run(&["replay", "nope", "--speed", "0"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("no session `nope`"));
    let out = s.run(&["replay", &id, "--from-step", "7"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("no step 7"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = s.run(&["replay", &id, "--speed", "-1"]);
    assert_eq!(out.status.code(), Some(2));
}
