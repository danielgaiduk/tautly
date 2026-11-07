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

    // JSON: the RPC stream's shape (CP5-A M8): each event wrapped as `replayed` (with its
    // offset and time), then `replay_finished`.
    let json_out = ok(&s.run(&["replay", &id, "--speed", "0", "--output", "json"]));
    let mut lines: Vec<Value> = json_out
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let end = lines.pop().unwrap();
    assert_eq!(end["type"], "replay_finished", "{json_out}");
    assert_eq!(end["session_id"], id.as_str());
    assert_eq!(end["events"], 6);
    assert_eq!(end["cancelled"], false);
    assert!(
        lines
            .iter()
            .all(|l| l["type"] == "replayed" && l["offset"].is_u64() && l["t_ms"].is_u64())
    );
    let events: Vec<Value> = lines.iter().map(|l| l["event"].clone()).collect();
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

#[test]
fn cli_replay_of_a_log_broken_mid_way_exits_1() {
    let s = Scratch::new();
    let id = s.session();
    let path = std::fs::read_dir(s.home.path().join("sessions"))
        .unwrap()
        .map(|p| p.unwrap().path().join(format!("{id}.jsonl")))
        .find(|p| p.exists())
        .unwrap();
    // The second record (the text) is broken; the header is line 1.
    let text = std::fs::read_to_string(&path).unwrap();
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    lines[2] = "{broken".into();
    std::fs::write(&path, lines.join("\n") + "\n").unwrap();
    let out = s.run(&["replay", &id, "--speed", "0", "--output", "json"]);
    assert_eq!(out.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&out.stdout);
    // What could be read was replayed, then the warning.
    assert!(stdout.contains("user_message"), "{stdout}");
    assert!(stdout.contains("cannot be read past"), "{stdout}");
    assert!(!stdout.contains("tool_finished"), "{stdout}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("could not be read to its end"));
}
