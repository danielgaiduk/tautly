//! The JSON-RPC server over an in-memory duplex, against `MockProvider` sessions.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use serde_json::{Value, json};
use tautly_core::{BudgetScope, Command, EngineBuilder, Mode, ModelId, Signal, SignalRouter};
use tautly_provider::{MockProvider, MockTurn, ProviderError};
use tautly_rpc::jsonrpc::{
    INVALID_PARAMS, INVALID_REQUEST, METHOD_NOT_FOUND, NOT_INITIALIZED, PARSE_ERROR,
    UNKNOWN_SESSION, UNSUPPORTED_PROTOCOL,
};
use tautly_rpc::{EngineFactory, RpcError, Server, command_for, serve};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, Lines};
use tokio::task::JoinHandle;

/// The client side of a server running on in-memory pipes.
struct Client {
    input: Option<DuplexStream>,
    output: Lines<BufReader<DuplexStream>>,
    server: JoinHandle<Result<(), RpcError>>,
    next_id: u64,
    /// Every line read, in order.
    raw: Vec<String>,
    /// Notifications read but not yet consumed: (session id, event).
    events: Vec<(String, Value)>,
}

impl Client {
    fn start(server: Server) -> Client {
        Client::spawn(move |input, output| tokio::spawn(server.serve(input, output)))
    }

    fn spawn(
        run: impl FnOnce(BufReader<DuplexStream>, DuplexStream) -> JoinHandle<Result<(), RpcError>>,
    ) -> Client {
        let (client_in, server_in) = tokio::io::duplex(1 << 20);
        let (server_out, client_out) = tokio::io::duplex(1 << 20);
        let server = run(BufReader::new(server_in), server_out);
        Client {
            input: Some(client_in),
            output: BufReader::new(client_out).lines(),
            server,
            next_id: 0,
            raw: Vec::new(),
            events: Vec::new(),
        }
    }

    async fn send_line(&mut self, line: &str) {
        let input = self.input.as_mut().expect("input closed");
        input.write_all(line.as_bytes()).await.unwrap();
        input.write_all(b"\n").await.unwrap();
    }

    async fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        self.send_line(&msg.to_string()).await;
        json!(id)
    }

    /// The next line from the server, parsed; None at the end of output.
    async fn read(&mut self) -> Option<Value> {
        let line = tokio::time::timeout(WAIT, self.output.next_line())
            .await
            .expect("timed out waiting for the server")
            .unwrap()?;
        let msg: Value =
            serde_json::from_str(&line).unwrap_or_else(|e| panic!("not JSON ({e}): {line}"));
        self.raw.push(line);
        Some(msg)
    }

    /// Read until the response to `id`, keeping notifications for later.
    async fn response(&mut self, id: &Value) -> Value {
        loop {
            let msg = self.read().await.expect("output ended before the response");
            if msg.get("method").is_some() {
                self.keep(msg);
            } else if &msg["id"] == id {
                return msg;
            } else {
                panic!("unexpected message {msg}");
            }
        }
    }

    async fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.request(method, params).await;
        self.response(&id).await
    }

    /// `call`, expecting success: the result.
    async fn ok(&mut self, method: &str, params: Value) -> Value {
        let resp = self.call(method, params).await;
        assert!(resp.get("error").is_none(), "{method} failed: {resp}");
        resp["result"].clone()
    }

    /// `call`, expecting an error: its code.
    async fn err(&mut self, method: &str, params: Value) -> i64 {
        let resp = self.call(method, params).await;
        resp["error"]["code"]
            .as_i64()
            .unwrap_or_else(|| panic!("{method} did not fail: {resp}"))
    }

    /// Keep a notification: an `event` as (session id, event), a `session/closed` as
    /// (session id, `{"type": "session/closed"}`).
    fn keep(&mut self, msg: Value) {
        let sid = msg["params"]["session_id"]
            .as_str()
            .unwrap_or_else(|| panic!("notification without session_id: {msg}"))
            .to_string();
        match msg["method"].as_str() {
            Some("event") => self.events.push((sid, msg["params"]["event"].clone())),
            Some("session/closed") => {
                assert_eq!(msg["params"].as_object().unwrap().len(), 1, "{msg}");
                self.events.push((sid, json!({"type": "session/closed"})));
            }
            _ => panic!("unexpected notification {msg}"),
        }
    }

    /// The first event (kept or new) that matches; it and the kept events before it are
    /// consumed and returned.
    async fn until(&mut self, pred: impl Fn(&str, &Value) -> bool) -> Vec<(String, Value)> {
        if let Some(i) = self.events.iter().position(|(s, e)| pred(s, e)) {
            return self.events.drain(..=i).collect();
        }
        loop {
            let msg = self.read().await.expect("output ended before the event");
            assert!(msg.get("method").is_some(), "unexpected response {msg}");
            self.keep(msg);
            let (s, e) = self.events.last().unwrap();
            if pred(s, e) {
                return std::mem::take(&mut self.events);
            }
        }
    }

    async fn until_type(&mut self, sid: &str, ty: &str) -> Vec<(String, Value)> {
        self.until(|s, e| s == sid && e["type"] == ty).await
    }

    /// Close the client's side of the input (a disconnect) and wait for the server to end.
    async fn disconnect(&mut self) {
        self.input = None;
        self.finish().await;
    }

    /// Wait for the server to end and read the rest of its output.
    async fn finish(&mut self) {
        while let Some(msg) = self.read().await {
            if msg.get("method").is_some() {
                self.keep(msg);
            }
        }
        let server = &mut self.server;
        tokio::time::timeout(WAIT, server)
            .await
            .expect("the server did not stop")
            .unwrap()
            .unwrap();
    }
}

fn events_of<'a>(events: &'a [(String, Value)], sid: &'a str) -> impl Iterator<Item = &'a Value> {
    events.iter().filter(move |(s, _)| s == sid).map(|(_, e)| e)
}

fn text_of(events: &[(String, Value)], sid: &str) -> String {
    events_of(events, sid)
        .filter(|e| e["type"] == "text_delta")
        .map(|e| e["text"].as_str().unwrap().to_string())
        .collect()
}

fn single(fx: &Fx, cfg: tautly_core::Config, mock: MockProvider) -> Client {
    let (f, _) = factory(fx, cfg, vec![mock], false);
    Client::start(Server::new(f, fx.workspaces(), fx.dir.path()))
}

#[tokio::test]
async fn initialize_negotiates_version() {
    let fx = Fx::new();
    let mut c = single(&fx, config(Mode::Ask), MockProvider::new(vec![]));
    let result = c
        .ok(
            "initialize",
            json!({"protocol": "1.4", "client": {"name": "test", "version": "0.1"}}),
        )
        .await;
    assert_eq!(result["protocol"], "1.0");
    let sid = result["session_id"]
        .as_str()
        .expect("a session id")
        .to_string();
    // The session's own announcement carries the same id.
    let seen = c.until_type(&sid, "session_changed").await;
    assert_eq!(seen.last().unwrap().1["session_id"], sid.as_str());
    // A second handshake is refused; the first stands.
    assert_ne!(c.err("initialize", json!({"protocol": "1.0"})).await, 0);
    c.ok("shutdown", json!({})).await;
    c.finish().await;
}

#[tokio::test]
async fn rejects_major_version_mismatch() {
    let fx = Fx::new();
    let (f, made) = factory(&fx, config(Mode::Ask), vec![], false);
    let mut c = Client::start(Server::new(f, fx.workspaces(), fx.dir.path()));
    let resp = c.call("initialize", json!({"protocol": "2.0"})).await;
    assert_eq!(resp["error"]["code"], UNSUPPORTED_PROTOCOL, "{resp}");
    assert_eq!(resp["error"]["data"]["supported"], "1.0", "{resp}");
    assert!(
        made.lock().unwrap().specs.is_empty(),
        "no session for a refused client"
    );
    // Still not initialized.
    assert_eq!(
        c.err("send_message", json!({"text": "hi"})).await,
        NOT_INITIALIZED
    );
    assert_eq!(c.err("session/list", json!({})).await, NOT_INITIALIZED);
    // Garbage versions are refused too.
    for bad in ["one", "2.0.0", "1.0.0.0", "1..0", "1.0.x", ""] {
        assert_eq!(
            c.err("initialize", json!({"protocol": bad})).await,
            UNSUPPORTED_PROTOCOL,
            "{bad}"
        );
    }
    // `major.minor.patch` is a version too; the patch is ignored.
    let result = c.ok("initialize", json!({"protocol": "1.0.3"})).await;
    assert_eq!(result["protocol"], "1.0");
    c.disconnect().await;
}

#[test]
fn methods_map_to_commands() {
    let cases: Vec<(&str, Value, Command)> = vec![
        (
            "send_message",
            json!({"text": "hi", "session_id": "s1"}),
            Command::SendMessage { text: "hi".into() },
        ),
        (
            "approve",
            json!({"request_id": "approval-1"}),
            Command::Approve {
                request_id: "approval-1".into(),
                session_rule: None,
            },
        ),
        (
            "deny",
            json!({"request_id": "approval-2", "reason": "no"}),
            Command::Deny {
                request_id: "approval-2".into(),
                reason: Some("no".into()),
            },
        ),
        ("cancel", json!({}), Command::Cancel),
        (
            "set_budget",
            json!({"scope": "session", "max_usd": 2.5}),
            Command::SetBudget {
                scope: BudgetScope::Session,
                max_usd: Some(2.5),
                max_tokens: None,
            },
        ),
        ("compact", json!({}), Command::Compact { id: None }),
        ("uncompact", json!({}), Command::Uncompact),
        (
            "pin",
            json!({"text": "use pnpm"}),
            Command::Pin {
                text: "use pnpm".into(),
            },
        ),
        (
            "unpin",
            json!({"text": "use pnpm"}),
            Command::Unpin {
                text: "use pnpm".into(),
            },
        ),
        (
            "approve_memory",
            json!({"proposal_id": "p1"}),
            Command::ApproveMemory {
                proposal_id: "p1".into(),
            },
        ),
        (
            "reject_memory",
            json!({"proposal_id": "p1"}),
            Command::RejectMemory {
                proposal_id: "p1".into(),
            },
        ),
        (
            "consolidate_memory",
            json!({}),
            Command::ConsolidateMemory { id: None },
        ),
        (
            "fork",
            json!({"at": 3, "label": "try b"}),
            Command::Fork {
                id: None,
                at: Some(3),
                label: Some("try b".into()),
            },
        ),
        (
            "fork",
            json!({}),
            Command::Fork {
                id: None,
                at: None,
                label: None,
            },
        ),
        (
            "set_model",
            json!({"model": "openai/gpt-5"}),
            Command::SetModel {
                id: None,
                model: ModelId::from("openai/gpt-5"),
            },
        ),
        ("diff", json!({"turn": 2}), Command::Diff { turn: Some(2) }),
        ("diff", json!({}), Command::Diff { turn: None }),
        (
            "undo",
            json!({"dry_run": true}),
            Command::Undo {
                id: None,
                turn: None,
                dry_run: true,
            },
        ),
        (
            "undo",
            json!({"turn": 1}),
            Command::Undo {
                id: None,
                turn: Some(1),
                dry_run: false,
            },
        ),
        (
            "rewind",
            json!({"at": 3, "dry_run": true}),
            Command::Rewind {
                id: None,
                at: 3,
                label: None,
                dry_run: true,
                overwrite: vec![],
            },
        ),
        (
            "rewind",
            json!({"at": 2, "label": "b", "overwrite": ["a.txt"]}),
            Command::Rewind {
                id: None,
                at: 2,
                label: Some("b".into()),
                dry_run: false,
                overwrite: vec!["a.txt".into()],
            },
        ),
        (
            "switch",
            json!({"target": "01X", "session_id": "s1"}),
            Command::Switch {
                id: None,
                session_id: "01X".into(),
                dry_run: false,
                overwrite: vec![],
            },
        ),
        (
            "label",
            json!({"label": "main"}),
            Command::Label {
                label: "main".into(),
            },
        ),
        (
            "replay",
            json!({"target": "01X", "session_id": "s1", "speed": 0, "from_step": 3}),
            Command::Replay {
                id: None,
                session_id: Some("01X".into()),
                speed: 0.0,
                from_step: Some(3),
            },
        ),
        // Defaults: the session addressed, at real-time speed, from the start.
        (
            "replay",
            json!({"session_id": "s1"}),
            Command::Replay {
                id: None,
                session_id: None,
                speed: 1.0,
                from_step: None,
            },
        ),
        ("shutdown", json!({}), Command::Shutdown),
        // A `type` among the params cannot turn one command into another.
        (
            "send_message",
            json!({"type": "shutdown", "text": "x"}),
            Command::SendMessage { text: "x".into() },
        ),
    ];
    for (method, params, want) in cases {
        assert_eq!(
            command_for(method, &params).unwrap_or_else(|e| panic!("{method}: {e:?}")),
            want,
            "{method} {params}"
        );
    }
    let code = |method: &str, params: Value| command_for(method, &params).unwrap_err().code;
    assert_eq!(code("frobnicate", json!({})), METHOD_NOT_FOUND);
    // Wire names only: the serde tag of a command that is not a method is not one.
    assert_eq!(code("session/new", json!({})), METHOD_NOT_FOUND);
    assert_eq!(code("send_message", json!({})), INVALID_PARAMS);
    // A value this version does not know is refused rather than passed to the engine to
    // be ignored: the client learns it was not understood (spec 3.4).
    assert_eq!(
        code("set_budget", json!({"scope": "forever", "max_usd": 1})),
        INVALID_PARAMS
    );
    assert_eq!(
        code("set_budget", json!({"scope": "unknown", "max_usd": 1})),
        INVALID_PARAMS
    );
    assert!(command_for("set_budget", &json!({"scope": "session", "max_usd": 1})).is_ok());
    assert_eq!(code("approve", json!({"request_id": 5})), INVALID_PARAMS);
}

#[tokio::test]
async fn events_become_notifications_ndjson() {
    let fx = Fx::new();
    fx.write("a.txt", "hello\n");
    let mock = MockProvider::new(vec![
        tools_step(vec![call("c1", "read", json!({"path": "a.txt"}))]),
        text_step("it says hello"),
    ]);
    let mut c = single(&fx, config(Mode::Ask), mock);
    let sid = c.ok("initialize", json!({"protocol": "1.0"})).await["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        c.ok("send_message", json!({"text": "read a.txt"})).await,
        Value::Null
    );
    let events = c.until_type(&sid, "turn_finished").await;
    assert_eq!(events.last().unwrap().1["reason"], "completed");
    assert_eq!(text_of(&events, &sid), "it says hello");
    for kind in [
        "turn_started",
        "tool_call_proposed",
        "tool_finished",
        "usage",
    ] {
        assert!(
            events_of(&events, &sid).any(|e| e["type"] == kind),
            "no {kind} in {events:?}"
        );
    }
    // Every line is one JSON-RPC 2.0 message: a response with an id, or an `event`
    // notification with the session id and the event.
    for line in &c.raw {
        assert!(!line.contains('\n'));
        let msg: Value = serde_json::from_str(line).unwrap();
        assert_eq!(msg["jsonrpc"], "2.0", "{line}");
        if msg.get("method").is_some() {
            assert_eq!(msg["method"], "event", "{line}");
            assert!(msg.get("id").is_none(), "{line}");
            assert_eq!(msg["params"]["session_id"], sid.as_str(), "{line}");
            assert!(msg["params"]["event"]["type"].is_string(), "{line}");
        } else {
            assert!(msg.get("id").is_some(), "{line}");
            assert!(
                msg.get("result").is_some() ^ msg.get("error").is_some(),
                "{line}"
            );
        }
    }
    c.ok("shutdown", json!({})).await;
    c.finish().await;
}

#[tokio::test]
async fn malformed_json_line_returns_parse_error_and_continues() {
    let fx = Fx::new();
    let mut c = single(&fx, config(Mode::Ask), MockProvider::new(vec![]));
    c.send_line("{this is not json").await;
    let resp = c.read().await.unwrap();
    assert_eq!(resp["id"], Value::Null, "{resp}");
    assert_eq!(resp["error"]["code"], PARSE_ERROR, "{resp}");
    // A notification is never answered, even when it fails.
    c.send_line(r#"{"jsonrpc":"2.0","method":"frobnicate"}"#)
        .await;
    c.send_line(r#"{"jsonrpc":"2.0","method":"cancel","params":[1]}"#)
        .await;
    c.send_line(r#"{"jsonrpc":"1.0","method":"cancel"}"#).await;
    // A line far over the limit is refused, and reading goes on.
    let huge = format!(
        r#"{{"jsonrpc":"2.0","id":99,"method":"x","params":{{"t":"{}"}}}}"#,
        "a".repeat(tautly_rpc::jsonrpc::MAX_LINE)
    );
    c.send_line(&huge).await;
    let resp = c.read().await.unwrap();
    assert_eq!(resp["id"], Value::Null, "{resp}");
    assert_eq!(resp["error"]["code"], INVALID_REQUEST, "{resp}");
    let result = c.ok("initialize", json!({"protocol": "1.0"})).await;
    assert_eq!(result["protocol"], "1.0");
    assert_eq!(c.err("frobnicate", json!({})).await, METHOD_NOT_FOUND);
    c.send_line("[]").await;
    let resp = c.response(&Value::Null).await;
    assert!(resp["error"]["code"].is_i64(), "{resp}");
    c.ok("shutdown", json!({})).await;
    c.finish().await;
}

/// A turn that wants to write `approved.txt` (which asks in `ask` mode), then ends.
fn write_then_done() -> MockProvider {
    MockProvider::new(vec![
        tools_step(vec![call(
            "w1",
            "write",
            json!({"path": "approved.txt", "content": "yes"}),
        )]),
        text_step("done"),
    ])
}

#[tokio::test]
async fn interactive_ask_waits_for_client_approval() {
    let fx = Fx::new();
    let mut c = single(&fx, config(Mode::Ask), write_then_done());
    let sid = c
        .ok(
            "initialize",
            json!({"protocol": "1.0", "client": {"interactive": true, "name": "app", "version": "1"}}),
        )
        .await["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    c.ok(
        "send_message",
        json!({"text": "write it", "session_id": sid}),
    )
    .await;
    let seen = c.until_type(&sid, "tool_approval_requested").await;
    let request = &seen.last().unwrap().1;
    assert_eq!(request["tool"], "write");
    let request_id = request["request_id"].as_str().unwrap().to_string();
    // Nothing runs while the client thinks.
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(!fx.exists("approved.txt"));
    c.ok(
        "approve",
        json!({"request_id": request_id, "session_id": sid}),
    )
    .await;
    let events = c.until_type(&sid, "turn_finished").await;
    assert_eq!(events.last().unwrap().1["reason"], "completed");
    let finished = events_of(&events, &sid)
        .find(|e| e["type"] == "tool_finished")
        .expect("the write ran");
    assert_eq!(finished["is_error"], false, "{finished}");
    assert!(fx.exists("approved.txt"));
    c.ok("shutdown", json!({})).await;
    c.finish().await;
}

/// `approve` passes its optional `session_rule` on: the second identical call is not asked.
#[tokio::test]
async fn approve_with_session_rule() {
    assert_eq!(
        command_for(
            "approve",
            &json!({"request_id": "approval-1", "session_rule": "bash(cargo test*)"}),
        )
        .unwrap(),
        Command::Approve {
            request_id: "approval-1".into(),
            session_rule: Some("bash(cargo test*)".into()),
        }
    );
    let fx = Fx::new();
    let echo = |id: &str| call(id, "bash", json!({"command": "echo hi"}));
    let mock = MockProvider::new(vec![
        tools_step(vec![echo("b1")]),
        tools_step(vec![echo("b2")]),
        text_step("done"),
    ]);
    let mut c = single(&fx, config(Mode::Ask), mock);
    let sid = c
        .ok(
            "initialize",
            json!({"protocol": "1.0", "client": {"interactive": true}}),
        )
        .await["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    c.ok("send_message", json!({"text": "go"})).await;
    let seen = c.until_type(&sid, "tool_approval_requested").await;
    let request_id = seen.last().unwrap().1["request_id"]
        .as_str()
        .unwrap()
        .to_string();
    c.ok(
        "approve",
        json!({"request_id": request_id, "session_rule": "bash(echo hi)"}),
    )
    .await;
    let events = c.until_type(&sid, "turn_finished").await;
    assert_eq!(events.last().unwrap().1["reason"], "completed");
    let asked = events_of(&events, &sid)
        .filter(|e| e["type"] == "tool_approval_requested")
        .count();
    assert_eq!(asked, 0, "the second call ran without asking: {events:?}");
    let ran = events_of(&events, &sid)
        .filter(|e| e["type"] == "tool_finished" && e["is_error"] == false)
        .count();
    assert_eq!(ran, 2, "{events:?}");
    c.ok("shutdown", json!({})).await;
    c.finish().await;
}

/// A `session_rule` on an MCP server approval is refused with `-32602`; the request stays
/// pending and the server does not start.
#[tokio::test]
async fn approve_with_session_rule_on_mcp_server_is_refused() {
    let fx = Fx::new();
    fx.write(
        ".tautly/config.toml",
        "[mcp.servers.srv]\ncommand = \"/nonexistent/tautly-test-server\"\n",
    );
    let cfg = config_with(tautly_core::CliOverrides::default(), &fx.root());
    let mut c = single(&fx, cfg, MockProvider::new(vec![]));
    let sid = c
        .ok(
            "initialize",
            json!({"protocol": "1.0", "client": {"interactive": true}}),
        )
        .await["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let seen = c.until_type(&sid, "mcp_server_approval_requested").await;
    let request_id = seen.last().unwrap().1["request_id"]
        .as_str()
        .unwrap()
        .to_string();
    let code = c
        .err(
            "approve",
            json!({"request_id": request_id, "session_rule": "mcp__srv__*"}),
        )
        .await;
    assert_eq!(code, INVALID_PARAMS);
    // Still pending: a plain deny is taken, and the server never started.
    c.ok("deny", json!({"request_id": request_id})).await;
    let events = c
        .until(|s, e| s == sid && e["type"] == "mcp_server_status" && e["state"] == "skipped")
        .await;
    assert!(
        !events_of(&events, &sid).any(|e| e["type"] == "mcp_server_status"
            && (e["state"] == "ready" || e["state"] == "starting")),
        "{events:?}"
    );
    c.ok("shutdown", json!({})).await;
    c.finish().await;
}

#[tokio::test]
async fn pending_approval_denied_on_disconnect() {
    let fx = Fx::new();
    let mut c = single(&fx, config(Mode::Ask), write_then_done());
    let sid = c
        .ok(
            "initialize",
            json!({"protocol": "1.0", "client": {"interactive": true}}),
        )
        .await["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    c.ok("send_message", json!({"text": "write it"})).await;
    c.until_type(&sid, "tool_approval_requested").await;
    // The client goes away with the request unanswered: nothing hangs, nothing runs.
    c.disconnect().await;
    let events = std::mem::take(&mut c.events);
    let finished = events_of(&events, &sid)
        .find(|e| e["type"] == "tool_finished")
        .unwrap_or_else(|| panic!("the call was never answered: {events:?}"));
    assert_eq!(finished["is_error"], true, "{finished}");
    assert!(
        events_of(&events, &sid).any(|e| e["type"] == "turn_finished"),
        "{events:?}"
    );
    assert!(!fx.exists("approved.txt"));
}

#[tokio::test]
async fn interactive_memory_proposal_waits_for_approve_memory() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![
        tools_step(vec![call(
            "m1",
            "remember",
            json!({"title": "Use pnpm", "kind": "convention",
                   "body": "Run pnpm install, never npm."}),
        )]),
        text_step("noted"),
    ]);
    let (f, made) = factory(&fx, config(Mode::Ask), vec![mock], false);
    let mut c = Client::start(Server::new(f, fx.workspaces(), fx.dir.path()));
    let sid = c
        .ok(
            "initialize",
            json!({"protocol": "1.0", "client": {"interactive": true}}),
        )
        .await["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    // The memory store opens with the index, after session start.
    let ws = made.lock().unwrap().workspaces[0].upgrade().unwrap();
    assert!(ws.wait_ready(WAIT).await);
    while ws.memory().is_none() {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    drop(ws);
    c.ok("send_message", json!({"text": "remember pnpm"})).await;
    let seen = c.until_type(&sid, "memory_proposed").await;
    let proposal = seen.last().unwrap().1["proposal_id"]
        .as_str()
        .unwrap()
        .to_string();
    let memory_dir = fx.dir.path().join(".tautly/memory");
    let count = || {
        std::fs::read_dir(&memory_dir)
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
    };
    assert_eq!(count(), 0, "written before the client answered");
    c.ok("approve_memory", json!({"proposal_id": proposal}))
        .await;
    c.until_type(&sid, "turn_finished").await;
    // Applied at the next step start, or right after the turn.
    let deadline = tokio::time::Instant::now() + WAIT;
    while count() == 0 {
        assert!(tokio::time::Instant::now() < deadline, "never written");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    c.ok("shutdown", json!({})).await;
    c.finish().await;
}

#[tokio::test]
async fn headless_client_gets_the_headless_rules() {
    let fx = Fx::new();
    let mut c = single(&fx, config(Mode::Ask), write_then_done());
    let sid = c
        .ok(
            "initialize",
            json!({"protocol": "1.0", "client": {"interactive": false}}),
        )
        .await["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    c.ok("send_message", json!({"text": "write it"})).await;
    let events = c.until_type(&sid, "turn_finished").await;
    assert!(
        !events_of(&events, &sid).any(|e| e["type"] == "tool_approval_requested"),
        "{events:?}"
    );
    let finished = events_of(&events, &sid)
        .find(|e| e["type"] == "tool_finished")
        .unwrap();
    assert_eq!(finished["is_error"], true, "{finished}");
    assert!(!fx.exists("approved.txt"));
    c.ok("shutdown", json!({})).await;
    c.finish().await;
}

#[tokio::test]
async fn two_sessions_share_one_index_and_route_events_by_session_id() {
    let fx = Fx::new();
    fx.write("src/lib.rs", "pub fn alpha() {}\n");
    let (f, made) = factory(
        &fx,
        config(Mode::Ask),
        vec![
            MockProvider::new(vec![text_step("from-a")]),
            MockProvider::new(vec![text_step("from-b")]),
        ],
        false,
    );
    let mut c = Client::start(Server::new(f, fx.workspaces(), fx.dir.path()));
    // No default session: this client manages its own.
    let init = c
        .ok(
            "initialize",
            json!({"protocol": "1.0", "client": {"interactive": true}, "session": false}),
        )
        .await;
    assert_eq!(init["session_id"], Value::Null);
    let root = fx.root().to_string_lossy().into_owned();
    let a = c.ok("session/new", json!({"root": root})).await["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let b = c.ok("session/new", json!({"root": root})).await["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(a, b);
    {
        let m = made.lock().unwrap();
        assert_eq!(m.specs.len(), 2);
        assert!(m.specs.iter().all(|(_, _, interactive)| *interactive));
        let (wa, wb) = (
            m.workspaces[0].upgrade().unwrap(),
            m.workspaces[1].upgrade().unwrap(),
        );
        assert!(Arc::ptr_eq(&wa, &wb), "two workspaces for one root");
        assert_eq!(wa.root(), fx.root());
    }
    let ws = made.lock().unwrap().workspaces[0].upgrade().unwrap();
    assert!(
        ws.wait_ready(WAIT).await,
        "the shared index never became ready"
    );
    let index = ws.index().unwrap();
    drop(ws);

    let mut listed: Vec<String> = c.ok("session/list", json!({})).await["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            assert_eq!(s["root"], root.as_str(), "{s}");
            s["session_id"].as_str().unwrap().to_string()
        })
        .collect();
    listed.sort();
    let mut want = vec![a.clone(), b.clone()];
    want.sort();
    assert_eq!(listed, want);

    // With several sessions, a session-scoped method must name one.
    assert_eq!(
        c.err("send_message", json!({"text": "hi"})).await,
        UNKNOWN_SESSION
    );
    assert_eq!(
        c.err("send_message", json!({"text": "hi", "session_id": "nope"}))
            .await,
        UNKNOWN_SESSION
    );
    c.ok("send_message", json!({"text": "hi a", "session_id": a}))
        .await;
    c.ok("send_message", json!({"text": "hi b", "session_id": b}))
        .await;
    // The two turns run at once and may finish in either order.
    let finished = |evs: &[(String, Value)], sid: &str| {
        evs.iter()
            .any(|(s, e)| s == sid && e["type"] == "turn_finished")
    };
    let mut events = Vec::new();
    while !(finished(&events, &a) && finished(&events, &b)) {
        events.extend(c.until(|_, e| e["type"] == "turn_finished").await);
    }
    assert_eq!(text_of(&events, &a), "from-a");
    assert_eq!(text_of(&events, &b), "from-b");

    // Closing one session leaves the other, and the shared workspace, running.
    assert_eq!(
        c.ok("session/close", json!({"session_id": a})).await,
        Value::Null
    );
    let listed = c.ok("session/list", json!({})).await;
    assert_eq!(listed["sessions"].as_array().unwrap().len(), 1, "{listed}");
    assert_eq!(listed["sessions"][0]["session_id"], b.as_str());
    let ws = made.lock().unwrap().workspaces[0].upgrade();
    assert!(ws.is_some(), "closed with a session still on it");
    assert!(Arc::ptr_eq(&ws.unwrap().index().unwrap(), &index));
    assert_eq!(
        c.err("send_message", json!({"text": "again", "session_id": a}))
            .await,
        UNKNOWN_SESSION
    );
    // A new session on the root now gets the same workspace again.
    let d = c.ok("session/new", json!({"root": root})).await["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    {
        let m = made.lock().unwrap();
        let (wb, wd) = (
            m.workspaces[1].upgrade().unwrap(),
            m.workspaces[2].upgrade().unwrap(),
        );
        assert!(Arc::ptr_eq(&wb, &wd));
    }
    drop(index);
    c.ok("session/close", json!({"session_id": b})).await;
    c.ok("session/close", json!({"session_id": d})).await;
    // The last session closed the workspace (index, watcher, memory store).
    let weak = made.lock().unwrap().workspaces[0].clone();
    let deadline = tokio::time::Instant::now() + WAIT;
    while weak.upgrade().is_some() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the workspace outlived its last session"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        c.ok("session/list", json!({})).await["sessions"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    c.ok("shutdown", json!({})).await;
    c.finish().await;
}

#[tokio::test]
async fn single_session_form_still_works() {
    let fx = Fx::new();
    let mut c = single(
        &fx,
        config(Mode::Ask),
        MockProvider::new(vec![text_step("one"), text_step("two")]),
    );
    let sid = c.ok("initialize", json!({})).await["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    for (text, want) in [("first", "one"), ("second", "two")] {
        c.ok("send_message", json!({"text": text})).await;
        let events = c.until_type(&sid, "turn_finished").await;
        assert_eq!(events.last().unwrap().1["reason"], "completed");
        assert_eq!(text_of(&events, &sid), want);
        assert!(events.iter().all(|(s, _)| s == &sid));
    }
    // Commands answered by events work the same way.
    c.ok("diff", json!({})).await;
    c.until_type(&sid, "session_diff").await;
    assert_eq!(c.ok("shutdown", json!({})).await, Value::Null);
    c.finish().await;
    // The session ended with the server.
    assert!(
        c.events.is_empty() || c.events.iter().all(|(s, _)| s == &sid),
        "{:?}",
        c.events
    );
}

#[tokio::test]
async fn prebuilt_engine_serves_the_single_session_form() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![text_step("hi there")]);
    let (engine, events) = fx.build(fx.builder(config(Mode::Ask), &mock));
    let id = engine.session_id().to_string();
    let mut c =
        Client::spawn(move |input, output| tokio::spawn(serve(engine, events, input, output)));
    // Commands before the handshake are refused.
    assert_eq!(
        c.err("send_message", json!({"text": "hi"})).await,
        NOT_INITIALIZED
    );
    let result = c.ok("initialize", json!({"protocol": "1.0"})).await;
    assert_eq!(result["session_id"], id.as_str());
    // This server cannot open sessions of its own.
    assert_ne!(c.err("session/new", json!({"root": fx.root()})).await, 0);
    c.ok("send_message", json!({"text": "hello"})).await;
    let events = c.until_type(&id, "turn_finished").await;
    assert_eq!(text_of(&events, &id), "hi there");
    c.disconnect().await;
}

#[tokio::test]
async fn a_failed_session_is_an_error_not_a_crash() {
    let fx = Fx::new();
    let (f, _) = factory(&fx, config(Mode::Ask), vec![], false);
    let mut c = Client::start(Server::new(f, fx.workspaces(), fx.dir.path()));
    c.ok("initialize", json!({"session": false})).await;
    let missing = fx.root().join("no-such-dir");
    let resp = c.call("session/new", json!({"root": missing})).await;
    assert!(resp["error"]["message"].is_string(), "{resp}");
    assert_eq!(
        c.err("session/new", json!({"root": "relative/path"})).await,
        INVALID_PARAMS
    );
    assert_eq!(c.err("session/new", json!({})).await, INVALID_PARAMS);
    let resp = c
        .call(
            "session/new",
            json!({"root": fx.root(), "resume": "01NOSUCHSESSION"}),
        )
        .await;
    assert!(resp["error"]["message"].is_string(), "{resp}");
    assert_eq!(
        c.err("session/close", json!({"session_id": "nope"})).await,
        UNKNOWN_SESSION
    );
    c.ok("shutdown", json!({})).await;
    c.finish().await;
}

#[tokio::test]
async fn session_new_resumes_and_forks_a_closed_session() {
    let fx = Fx::new();
    let (f, made) = factory(
        &fx,
        config(Mode::Ask),
        vec![
            MockProvider::new(vec![text_step("first answer")]),
            MockProvider::new(vec![text_step("resumed answer")]),
            MockProvider::new(vec![text_step("forked answer")]),
        ],
        false,
    );
    let mut c = Client::start(Server::new(f, fx.workspaces(), fx.dir.path()));
    let parent = c.ok("initialize", json!({})).await["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    c.ok("send_message", json!({"text": "hello"})).await;
    c.until_type(&parent, "turn_finished").await;
    c.ok("session/close", json!({"session_id": parent})).await;

    let root = fx.root();
    let resumed = c
        .ok("session/new", json!({"root": root, "resume": parent}))
        .await["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(resumed, parent);
    assert_eq!(
        made.lock().unwrap().specs[1].1.as_deref(),
        Some(parent.as_str())
    );
    // An open session cannot be forked into a second process-local copy.
    assert_ne!(
        c.err(
            "session/new",
            json!({"root": root, "fork": {"session_id": parent}})
        )
        .await,
        0
    );
    c.ok("session/close", json!({"session_id": parent})).await;

    let branch = c
        .ok(
            "session/new",
            json!({"root": root, "fork": {"session_id": parent, "label": "b"}}),
        )
        .await["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(branch, parent);
    let listed = c.ok("session/list", json!({})).await;
    assert_eq!(listed["sessions"].as_array().unwrap().len(), 1, "{listed}");
    assert_eq!(listed["sessions"][0]["session_id"], branch.as_str());
    c.ok(
        "send_message",
        json!({"text": "go on", "session_id": branch}),
    )
    .await;
    let events = c.until_type(&branch, "turn_finished").await;
    assert_eq!(text_of(&events, &branch), "forked answer");
    // A worktree fork (spec 7.12.4) is refused outside git.
    c.ok("session/close", json!({"session_id": branch})).await;
    assert_ne!(
        c.err(
            "session/new",
            json!({"root": root, "fork": {"session_id": parent, "worktree": true}})
        )
        .await,
        0
    );
    c.ok("shutdown", json!({})).await;
    c.finish().await;
}

#[tokio::test]
async fn a_fork_moves_the_session_id() {
    let fx = Fx::new();
    let mut c = single(
        &fx,
        config(Mode::Ask),
        MockProvider::new(vec![text_step("one"), text_step("two")]),
    );
    let parent = c.ok("initialize", json!({})).await["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    c.ok("send_message", json!({"text": "hello"})).await;
    c.until_type(&parent, "turn_finished").await;
    c.ok("fork", json!({"label": "b"})).await;
    // The old id still reaches the session, before and after the move is announced.
    c.ok("cancel", json!({"session_id": parent})).await;
    // Announced under the old id, naming the new one.
    let seen = c
        .until(|s, e| {
            s == parent && e["type"] == "session_changed" && e["session_id"] != parent.as_str()
        })
        .await;
    let branch = seen.last().unwrap().1["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    // From now on the session goes by the branch's id (and so does the default).
    c.ok(
        "send_message",
        json!({"text": "again", "session_id": branch}),
    )
    .await;
    let events = c.until_type(&branch, "turn_finished").await;
    assert_eq!(text_of(&events, &branch), "two");
    let listed = c.ok("session/list", json!({})).await;
    assert_eq!(listed["sessions"].as_array().unwrap().len(), 1, "{listed}");
    assert_eq!(listed["sessions"][0]["session_id"], branch.as_str());
    // The old id stays an alias of the session until it closes.
    c.ok("cancel", json!({"session_id": parent})).await;
    c.ok("cancel", json!({})).await;
    c.ok("session/close", json!({"session_id": parent})).await;
    assert_eq!(
        c.err("cancel", json!({"session_id": branch})).await,
        UNKNOWN_SESSION
    );
    assert_eq!(
        c.err("cancel", json!({"session_id": parent})).await,
        UNKNOWN_SESSION
    );
    c.ok("shutdown", json!({})).await;
    c.finish().await;
}

#[tokio::test]
async fn start_commands_reach_every_new_session_first() {
    let fx = Fx::new();
    // A budget of zero: the turn ends at once with `budget`.
    let (f, _) = factory(
        &fx,
        config(Mode::Ask),
        vec![MockProvider::new(vec![MockTurn::Error(
            ProviderError::Other("not reached".into()),
        )])],
        false,
    );
    let server =
        Server::new(f, fx.workspaces(), fx.dir.path()).start_commands(vec![Command::SetBudget {
            scope: BudgetScope::Session,
            max_usd: Some(0.0),
            max_tokens: Some(0),
        }]);
    let mut c = Client::start(server);
    let sid = c.ok("initialize", json!({})).await["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    c.ok("send_message", json!({"text": "hello"})).await;
    let events = c.until_type(&sid, "turn_finished").await;
    assert_eq!(events.last().unwrap().1["reason"], "budget", "{events:?}");
    c.ok("shutdown", json!({})).await;
    c.finish().await;
}

#[tokio::test]
async fn a_session_that_ends_by_itself_is_removed_and_announced() {
    let fx = Fx::new();
    let (f, _) = factory(
        &fx,
        config(Mode::Ask),
        vec![MockProvider::new(vec![text_step("one")])],
        false,
    );
    // Signals reach the engines as they do in `tautly --rpc`, not through the server.
    let router = SignalRouter::new(|sig| panic!("escalated on {sig:?}"));
    let routed = Arc::clone(&router);
    let f: EngineFactory = Arc::new(move |spec| {
        let (engine, events) = f(spec)?;
        routed.add(&engine);
        Ok((engine, events))
    });
    let mut c = Client::start(Server::new(f, fx.workspaces(), fx.dir.path()));
    let sid = c.ok("initialize", json!({})).await["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    c.ok("send_message", json!({"text": "hello"})).await;
    c.until_type(&sid, "turn_finished").await;

    router.deliver(Signal::Terminate).await;
    let seen = c.until_type(&sid, "session/closed").await;
    assert!(
        seen.iter().all(|(s, _)| s == &sid),
        "another session's events: {seen:?}"
    );
    // Gone from the server: not listed, not the default any more.
    let listed = c.ok("session/list", json!({})).await;
    assert!(
        listed["sessions"].as_array().unwrap().is_empty(),
        "{listed}"
    );
    assert_eq!(
        c.err("send_message", json!({"text": "again"})).await,
        UNKNOWN_SESSION
    );
    assert_eq!(
        c.err("session/close", json!({"session_id": sid})).await,
        UNKNOWN_SESSION
    );
    // The server goes on: a new session opens, and closing it is not announced again.
    let root = fx.root().to_string_lossy().into_owned();
    let other = c.ok("session/new", json!({"root": root})).await["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    c.ok("session/close", json!({"session_id": other})).await;
    c.ok("shutdown", json!({})).await;
    c.finish().await;
    assert!(
        !c.events.iter().any(|(_, e)| e["type"] == "session/closed"),
        "{:?}",
        c.events
    );
    assert!(
        c.raw.iter().filter(|l| l.contains("session/closed")).all(
            |l| serde_json::from_str::<Value>(l).unwrap()
                == json!({"jsonrpc": "2.0", "method": "session/closed",
                          "params": {"session_id": sid}})
        ),
        "{:?}",
        c.raw
    );
}

#[tokio::test]
async fn the_stop_future_ends_the_server_like_a_disconnect() {
    let fx = Fx::new();
    let (f, _) = factory(
        &fx,
        config(Mode::Ask),
        vec![write_then_done(), MockProvider::new(vec![])],
        false,
    );
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = Server::new(f, fx.workspaces(), fx.dir.path()).stop_on(async move {
        let _ = stopped.await;
    });
    let mut c = Client::start(server);
    let sid = c
        .ok(
            "initialize",
            json!({"protocol": "1.0", "client": {"interactive": true}}),
        )
        .await["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let root = fx.root().to_string_lossy().into_owned();
    let other = c.ok("session/new", json!({"root": root})).await["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    c.ok("send_message", json!({"text": "write it"})).await;
    c.until_type(&sid, "tool_approval_requested").await;
    // SIGTERM, say: the pending request is denied, the session drained, the server ends
    // with the client's input still open.
    stop.send(()).unwrap();
    c.finish().await;
    let events = std::mem::take(&mut c.events);
    // Every open session is announced as closed, after its last event.
    for id in [&sid, &other] {
        let kinds: Vec<&str> = events_of(&events, id)
            .map(|e| e["type"].as_str().unwrap())
            .collect();
        assert_eq!(
            kinds.iter().filter(|k| **k == "session/closed").count(),
            1,
            "{id}: {kinds:?}"
        );
        assert_eq!(kinds.last(), Some(&"session/closed"), "{id}: {kinds:?}");
    }
    let finished = events_of(&events, &sid)
        .find(|e| e["type"] == "tool_finished")
        .unwrap_or_else(|| panic!("the call was never answered: {events:?}"));
    assert_eq!(finished["is_error"], true, "{finished}");
    assert!(!fx.exists("approved.txt"));
    assert!(c.input.is_some());
}

#[tokio::test]
async fn shutdown_does_not_wait_for_a_session_still_being_built() {
    let fx = Fx::new();
    let (f, _) = factory(&fx, config(Mode::Ask), vec![], false);
    // A build that waits, as on a keychain dialog, until released (or the test ends).
    let (release, gate) = std::sync::mpsc::channel::<()>();
    let gate = Arc::new(std::sync::Mutex::new(gate));
    let (entered_tx, entered) = std::sync::mpsc::channel::<()>();
    let entered_tx = std::sync::Mutex::new(entered_tx);
    let blocking: EngineFactory = Arc::new(move |spec| {
        let _ = entered_tx.lock().unwrap().send(());
        let _ = gate.lock().unwrap().recv();
        f(spec)
    });
    let mut c = Client::start(Server::new(blocking, fx.workspaces(), fx.dir.path()));
    c.ok("initialize", json!({"session": false})).await;
    let root = fx.root().to_string_lossy().into_owned();
    let new_id = c.request("session/new", json!({"root": root})).await;
    let deadline = tokio::time::Instant::now() + WAIT;
    while entered.try_recv().is_err() {
        assert!(tokio::time::Instant::now() < deadline, "never built");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let shutdown_id = c.request("shutdown", json!({})).await;
    let quick = Duration::from_secs(5);
    let mut answers = Vec::new();
    tokio::time::timeout(quick, async {
        while let Some(msg) = c.read().await {
            answers.push(msg);
        }
    })
    .await
    .expect("shutdown waited for the session being built");
    tokio::time::timeout(quick, &mut c.server)
        .await
        .expect("the server did not stop")
        .unwrap()
        .unwrap();
    let answer = |id: &Value| answers.iter().find(|m| &m["id"] == id).cloned();
    assert_eq!(
        answer(&shutdown_id).expect("shutdown unanswered")["result"],
        Value::Null
    );
    if let Some(resp) = answer(&new_id) {
        assert!(resp["error"]["code"].is_i64(), "{resp}");
    }
    // The build finishes later; its session is shut down on its own.
    release.send(()).unwrap();
}

/// A factory whose builds wait, as on a keychain dialog, until released (or the test ends):
/// the factory, a signal that a build has started, and the release.
fn gated(
    fx: &Fx,
    mocks: Vec<MockProvider>,
) -> (
    EngineFactory,
    std::sync::mpsc::Receiver<()>,
    std::sync::mpsc::Sender<()>,
) {
    let (f, _) = factory(fx, config(Mode::Ask), mocks, false);
    let (release, gate) = std::sync::mpsc::channel::<()>();
    let gate = Arc::new(std::sync::Mutex::new(gate));
    let (entered_tx, entered) = std::sync::mpsc::channel::<()>();
    let entered_tx = std::sync::Mutex::new(entered_tx);
    let blocking: EngineFactory = Arc::new(move |spec| {
        let _ = entered_tx.lock().unwrap().send(());
        let _ = gate.lock().unwrap().recv();
        f(spec)
    });
    (blocking, entered, release)
}

async fn wait_entered(entered: &std::sync::mpsc::Receiver<()>) {
    let deadline = tokio::time::Instant::now() + WAIT;
    while entered.try_recv().is_err() {
        assert!(tokio::time::Instant::now() < deadline, "never built");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn a_blocked_initialize_build_does_not_stop_the_reader_or_the_stop_future() {
    let fx = Fx::new();
    let (blocking, entered, release) = gated(&fx, vec![]);
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = Server::new(blocking, fx.workspaces(), fx.dir.path()).stop_on(async move {
        let _ = stopped.await;
    });
    let mut c = Client::start(server);
    let init_id = c.request("initialize", json!({})).await;
    wait_entered(&entered).await;
    // Input is still read while the build waits: requests behind `initialize` are held,
    // up to a limit, and the ones over it are refused at once.
    for _ in 0..1000 {
        c.request("session/list", json!({})).await;
    }
    let quick = Duration::from_secs(5);
    let refused = tokio::time::timeout(quick, c.read())
        .await
        .expect("the reader is stuck in the build")
        .expect("output ended");
    assert_eq!(refused["error"]["code"], NOT_INITIALIZED, "{refused}");
    // The stop future is heard too: the server ends without waiting for the build.
    stop.send(()).unwrap();
    let mut answers = Vec::new();
    tokio::time::timeout(quick, async {
        while let Some(msg) = c.read().await {
            answers.push(msg);
        }
    })
    .await
    .expect("the stop waited for the build");
    tokio::time::timeout(quick, &mut c.server)
        .await
        .expect("the server did not stop")
        .unwrap()
        .unwrap();
    if let Some(resp) = answers.iter().find(|m| m["id"] == init_id) {
        assert!(resp["error"]["code"].is_i64(), "{resp}");
    }
    // The build finishes later; its session is shut down on its own.
    release.send(()).unwrap();
}

#[tokio::test]
async fn requests_behind_a_slow_initialize_run_after_it_in_order() {
    let fx = Fx::new();
    let (blocking, entered, release) = gated(&fx, vec![MockProvider::new(vec![text_step("hi")])]);
    let mut c = Client::start(Server::new(blocking, fx.workspaces(), fx.dir.path()));
    let init_id = c.request("initialize", json!({})).await;
    let list_id = c.request("session/list", json!({})).await;
    let send_id = c.request("send_message", json!({"text": "hello"})).await;
    // A script: the input ends right behind the requests; they are still answered (the
    // disconnect then shuts the session down).
    c.input = None;
    wait_entered(&entered).await;
    release.send(()).unwrap();
    c.finish().await;
    let responses: Vec<Value> = c
        .raw
        .iter()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .filter(|m| m.get("id").is_some())
        .collect();
    let order: Vec<Value> = responses.iter().map(|m| m["id"].clone()).collect();
    assert_eq!(
        order,
        vec![init_id.clone(), list_id.clone(), send_id.clone()]
    );
    let sid = responses[0]["result"]["session_id"].as_str().unwrap();
    assert_eq!(
        responses[1]["result"]["sessions"][0]["session_id"], sid,
        "{}",
        responses[1]
    );
    assert_eq!(responses[2]["result"], Value::Null, "{}", responses[2]);
}

#[tokio::test]
async fn prebuilt_engine_refuses_no_default_session() {
    // `session: false` would leave the prebuilt session unserved: refused.
    let fx = Fx::new();
    let mock = MockProvider::new(vec![]);
    let (engine, events) = fx.build(fx.builder(config(Mode::Ask), &mock));
    let mut c =
        Client::spawn(move |input, output| tokio::spawn(serve(engine, events, input, output)));
    assert_eq!(
        c.err("initialize", json!({"session": false})).await,
        INVALID_PARAMS
    );
    c.ok("initialize", json!({})).await;
    c.disconnect().await;
}

#[tokio::test]
async fn prebuilt_engine_is_shut_down_without_initialize() {
    // A client that leaves before `initialize`: the session is still shut down, and done
    // with by the time `serve` returns (its log is free to resume at once).
    let fx = Fx::new();
    let mock = MockProvider::new(vec![]);
    let (engine, events) = fx.build(fx.builder(config(Mode::Ask), &mock));
    let id = engine.session_id().to_string();
    let mut c =
        Client::spawn(move |input, output| tokio::spawn(serve(engine, events, input, output)));
    c.disconnect().await;
    let resumed = EngineBuilder {
        resume: Some(id.clone()),
        ..fx.builder(config(Mode::Ask), &mock)
    }
    .build();
    let (engine, mut events) = resumed.unwrap_or_else(|e| panic!("still in use: {e}"));
    engine.send(Command::Shutdown).await.unwrap();
    drop(engine);
    while events.recv().await.is_some() {}
}

#[tokio::test]
async fn semver_style_protocol_versions_are_accepted() {
    let fx = Fx::new();
    let mut c = single(&fx, config(Mode::Ask), MockProvider::new(vec![]));
    let result = c.ok("initialize", json!({"protocol": "1.0.0"})).await;
    assert_eq!(result["protocol"], "1.0");
    c.ok("shutdown", json!({})).await;
    c.finish().await;
}

#[tokio::test]
async fn rpc_session_tree_and_rewind_methods() {
    let fx = Fx::new();
    fx.write("a.txt", "zero\n");
    let mock = MockProvider::new(vec![
        tools_step(vec![call(
            "w1",
            "write",
            json!({"path": "a.txt", "content": "one\n"}),
        )]),
        text_step("first"),
        tools_step(vec![call(
            "w2",
            "write",
            json!({"path": "a.txt", "content": "two\n"}),
        )]),
        text_step("second"),
    ]);
    let mut c = single(&fx, config(Mode::Yolo), mock);
    let a = c.ok("initialize", json!({})).await["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    for text in ["one", "two"] {
        c.ok("send_message", json!({ "text": text })).await;
        c.until_type(&a, "turn_finished").await;
    }
    // The dry run changes nothing.
    c.ok("rewind", json!({"at": 3, "dry_run": true})).await;
    let seen = c.until_type(&a, "rewound").await;
    let dry = &seen.last().unwrap().1;
    assert_eq!(dry["report"]["dry_run"], true, "{dry}");
    assert_eq!(dry["report"]["applied"], false);
    assert_eq!(dry["report"]["restored"], json!(["a.txt"]));
    assert_eq!(
        std::fs::read_to_string(fx.root().join("a.txt")).unwrap(),
        "two\n"
    );

    c.ok("rewind", json!({"at": 3, "label": "try-b"})).await;
    let seen = c.until_type(&a, "rewound").await;
    let done = &seen.last().unwrap().1;
    assert_eq!(done["report"]["applied"], true, "{done}");
    let branch = done["branch"].as_str().unwrap().to_string();
    c.until(|s, e| s == a && e["type"] == "session_changed")
        .await;
    assert_eq!(
        std::fs::read_to_string(fx.root().join("a.txt")).unwrap(),
        "one\n"
    );
    c.ok("label", json!({"label": "renamed"})).await;

    let root = fx.root().to_string_lossy().into_owned();
    let tree = c.ok("session/tree", json!({ "root": root })).await;
    let nodes = tree["nodes"].as_array().unwrap();
    assert_eq!(nodes.len(), 2, "{tree}");
    assert_eq!(nodes[0]["session_id"], a.as_str());
    assert_eq!(nodes[0]["origin"], "user");
    assert_eq!(nodes[1]["session_id"], branch.as_str());
    assert_eq!(nodes[1]["parent"], json!([a, 3]));
    assert_eq!(nodes[1]["origin"], "rewind");
    assert_eq!(nodes[1]["current"], true);
    assert_eq!(nodes[1]["live"], true);
    // The label lands in the log once the session flushes; switching back flushes it.
    c.ok("switch", json!({ "target": a })).await;
    let seen = c.until_type(&branch, "switched").await;
    assert_eq!(seen.last().unwrap().1["report"]["applied"], true);
    assert_eq!(
        std::fs::read_to_string(fx.root().join("a.txt")).unwrap(),
        "two\n"
    );
    let tree = c.ok("session/tree", json!({ "root": root })).await;
    let nodes = tree["nodes"].as_array().unwrap();
    assert_eq!(nodes[1]["label"], "renamed", "{tree}");
    assert_eq!(nodes[0]["current"], true, "{tree}");
    assert_eq!(
        c.err("session/tree", json!({ "root": "relative" })).await,
        INVALID_PARAMS
    );
    c.ok("shutdown", json!({})).await;
    c.finish().await;
}
