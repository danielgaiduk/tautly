//! The `tautly` binary end to end on a scripted provider (`--mock-script`, feature
//! `mock`): `cargo test -p tautly-cli --features mock`. Each run is in a scratch work tree
//! with its own `TAUTLY_HOME`, so no real configuration, key or session is touched.

use std::io::Write as _;
use std::process::{Command, Output, Stdio};

use serde_json::{Value, json};
use tempfile::TempDir;

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

    /// Write a mock script of `turns` and return its path.
    fn script(&self, name: &str, turns: Value) -> String {
        let path = self.home.path().join(name);
        std::fs::write(&path, turns.to_string()).unwrap();
        path.to_string_lossy().into_owned()
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_tautly"));
        c.args(args)
            .current_dir(self.work.path())
            .env("TAUTLY_HOME", self.home.path())
            .env("HOME", self.home.path())
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_DATA_HOME");
        c
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }
}

fn text(t: &str) -> Value {
    json!([{"text_delta": t}, {"done": {"stop": "end_turn"}}])
}

fn write_call(path: &str) -> Value {
    json!([
        {"tool_call": {"id": "w1", "name": "write",
                       "input_json": json!({"path": path, "content": "x"}).to_string()}},
        {"done": {"stop": "tool_use"}}
    ])
}

fn lines(out: &Output) -> Vec<Value> {
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("not JSON ({e}): {l}")))
        .collect()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn prompt_prints_text_and_exits_zero() {
    let s = Scratch::new();
    let script = s.script("s.json", json!([text("Hello from the mock.")]));
    let out = s.run(&["-p", "hi", "--mock-script", &script]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "Hello from the mock.\n"
    );
}

#[test]
fn json_output_streams_events_and_continue_resumes() {
    let s = Scratch::new();
    let script = s.script("s.json", json!([text("first")]));
    let out = s.run(&["-p", "hi", "--output", "json", "--mock-script", &script]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let evs = lines(&out);
    assert_eq!(evs[0]["type"], "session_changed");
    let first = evs[0]["session_id"].as_str().unwrap().to_string();
    assert!(
        evs.iter()
            .any(|e| e["type"] == "turn_finished" && e["reason"] == "completed")
    );

    let script = s.script("t.json", json!([text("second")]));
    let out = s.run(&[
        "-p",
        "again",
        "--continue",
        "--output",
        "json",
        "--mock-script",
        &script,
    ]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(lines(&out)[0]["session_id"], first.as_str());
}

#[test]
fn a_spent_budget_exits_two() {
    let s = Scratch::new();
    let script = s.script("s.json", json!([text("never sent")]));
    let out = s.run(&["-p", "hi", "--budget-usd", "0", "--mock-script", &script]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
}

#[test]
fn approve_auto_edit_lets_edits_through() {
    let s = Scratch::new();
    let script = s.script("s.json", json!([write_call("denied.txt"), text("ok")]));
    let out = s.run(&["-p", "write", "--mock-script", &script]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(!s.work.path().join("denied.txt").exists());

    let script = s.script("t.json", json!([write_call("allowed.txt"), text("ok")]));
    let out = s.run(&[
        "-p",
        "write",
        "--approve",
        "auto-edit",
        "--mock-script",
        &script,
    ]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(s.work.path().join("allowed.txt").exists());
}

#[test]
fn rpc_mode_serves_json_rpc_on_stdio() {
    let s = Scratch::new();
    let script = s.script("s.json", json!([text("over rpc")]));
    let mut child = s
        .command(&["--rpc", "--mock-script", &script])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = std::io::BufReader::new(child.stdout.take().unwrap());
    let mut send = |m: Value| writeln!(stdin, "{m}").unwrap();
    send(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocol": "1.0"}}));
    send(json!({"jsonrpc": "2.0", "id": 2, "method": "send_message", "params": {"text": "hi"}}));
    let mut msgs = Vec::new();
    loop {
        let mut line = String::new();
        let n = std::io::BufRead::read_line(&mut stdout, &mut line).unwrap();
        assert!(n > 0, "output ended early: {msgs:?}");
        let msg: Value = serde_json::from_str(&line).unwrap();
        let done = msg["params"]["event"]["type"] == "turn_finished";
        msgs.push(msg);
        if done {
            break;
        }
    }
    // A disconnect: the server shuts the session down, writes the rest and exits 0.
    drop(stdin);
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    msgs.extend(lines(&out));
    let init = msgs
        .iter()
        .find(|m| m["id"] == 1)
        .expect("initialize answered");
    let sid = init["result"]["session_id"].as_str().unwrap();
    assert!(msgs.iter().any(|m| m["id"] == 2 && m["result"].is_null()));
    assert!(msgs.iter().all(|m| m["jsonrpc"] == "2.0"));
    assert!(
        msgs.iter()
            .filter(|m| m["method"] == "event")
            .all(|m| m["params"]["session_id"] == sid)
    );
    let said: String = msgs
        .iter()
        .filter(|m| m["params"]["event"]["type"] == "text_delta")
        .map(|m| m["params"]["event"]["text"].as_str().unwrap())
        .collect();
    assert_eq!(said, "over rpc");
}
