//! The `tautly` binary end to end on a scripted provider (`--mock-script`, feature
//! `mock`): `cargo test -p tautly-cli --features mock`. Each run is in a scratch work tree
//! with its own `TAUTLY_HOME`, so no real configuration, key or session is touched.

use std::io::{BufRead as _, Write as _};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::time::{Duration, Instant};

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

/// Send `signal` (`TERM`, `INT`) to the child.
fn kill(child: &Child, signal: &str) {
    let status = Command::new("kill")
        .args([&format!("-{signal}"), &child.id().to_string()])
        .status()
        .unwrap();
    assert!(status.success());
}

/// Wait for the child to exit, killing it if it takes longer than 20 s.
fn wait_exit(child: &mut Child) -> ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("tautly did not exit");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Read JSON lines from `out` until one matches `pred`.
fn read_until(out: &mut impl std::io::BufRead, pred: impl Fn(&Value) -> bool) -> Vec<Value> {
    let mut seen = Vec::new();
    loop {
        let mut line = String::new();
        let n = out.read_line(&mut line).unwrap();
        assert!(n > 0, "output ended early: {seen:?}");
        let msg: Value = serde_json::from_str(&line).unwrap();
        let done = pred(&msg);
        seen.push(msg);
        if done {
            return seen;
        }
    }
}

#[test]
fn a_signal_during_a_prompt_exits_with_its_code() {
    // SIGTERM is 143 (128 + 15); Ctrl-C (SIGINT) cancels the turn: 130.
    for (signal, code) in [("TERM", 143), ("INT", 130)] {
        let s = Scratch::new();
        let script = s.script(
            "s.json",
            json!([[
                {"tool_call": {"id": "b1", "name": "bash",
                               "input_json": json!({"command": "sleep 30"}).to_string()}},
                {"done": {"stop": "tool_use"}}
            ], text("never")]),
        );
        let mut child = s
            .command(&[
                "-p",
                "wait",
                "--output",
                "json",
                "--approve",
                "yolo",
                "--mock-script",
                &script,
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdout = std::io::BufReader::new(child.stdout.take().unwrap());
        read_until(&mut stdout, |e| e["type"] == "tool_started");
        kill(&child, signal);
        let status = wait_exit(&mut child);
        assert_eq!(status.code(), Some(code), "SIG{signal}: {status:?}");
    }
}

#[test]
fn rpc_mode_ends_on_sigterm() {
    let s = Scratch::new();
    let script = s.script("s.json", json!([text("unused")]));
    let mut child = s
        .command(&["--rpc", "--mock-script", &script])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = std::io::BufReader::new(child.stdout.take().unwrap());
    let init = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}});
    writeln!(stdin, "{init}").unwrap();
    let seen = read_until(&mut stdout, |m| m["id"] == 1);
    let sid = seen.last().unwrap()["result"]["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    // The client keeps its end open: the signal alone ends the server, after the session.
    kill(&child, "TERM");
    let status = wait_exit(&mut child);
    assert_eq!(status.code(), Some(143), "{status:?}");
    let rest: Vec<Value> = stdout
        .lines()
        .map(|l| serde_json::from_str(&l.unwrap()).unwrap())
        .collect();
    assert!(
        rest.iter()
            .all(|m| m["params"]["session_id"] == sid.as_str()),
        "{rest:?}"
    );
    // The session is announced as closed before the process goes.
    assert_eq!(
        rest.last().map(|m| m["method"].clone()),
        Some(json!("session/closed")),
        "{rest:?}"
    );
    drop(stdin);
}

// ---------------------------------------------------------------- utility commands

impl Scratch {
    fn write(&self, name: &str, text: &str) {
        let path = self.work.path().join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.work.path().join(name)).unwrap()
    }

    fn exists(&self, name: &str) -> bool {
        self.work.path().join(name).exists()
    }

    /// Run a prompt on a script of `turns` (edits allowed, JSON output); the session id.
    fn prompt(&self, prompt: &str, turns: Value, extra: &[&str]) -> String {
        let script = self.script(&format!("script-{}.json", prompt.len()), turns);
        let mut args = vec![
            "-p",
            prompt,
            "--approve",
            "auto-edit",
            "--output",
            "json",
            "--mock-script",
            &script,
        ];
        args.extend_from_slice(extra);
        let out = self.run(&args);
        assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
        lines(&out)[0]["session_id"].as_str().unwrap().to_string()
    }
}

fn out_text(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn write_content(path: &str, content: &str) -> Value {
    json!([
        {"tool_call": {"id": "w1", "name": "write",
                       "input_json": json!({"path": path, "content": content}).to_string()}},
        {"done": {"stop": "tool_use"}}
    ])
}

#[test]
fn prompt_show_full_prints_last_request() {
    let s = Scratch::new();
    s.prompt("remember the milk", json!([text("done")]), &[]);
    let out = s.run(&["prompt", "show", "--full"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let request: Value = serde_json::from_slice(&out.stdout).expect("JSON");
    assert!(request.is_object());
    assert!(
        out_text(&out).contains("remember the milk"),
        "{}",
        out_text(&out)
    );
    // Pretty printed: more than one line.
    assert!(out_text(&out).lines().count() > 3);
}

#[test]
fn sessions_list_shows_ids() {
    let s = Scratch::new();
    let id = s.prompt("fix the parser", json!([text("done")]), &[]);
    let out = s.run(&["sessions", "list"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let list = out_text(&out);
    let row = list.lines().find(|l| l.contains(&id)).expect(&list);
    assert!(row.contains("fix the parser"), "{row}");
    assert!(
        row.contains("UTC") || row.contains(':'),
        "a start time: {row}"
    );
}

#[test]
fn sessions_show_lists_the_user_turns() {
    let s = Scratch::new();
    s.write("a.txt", "old\n");
    let id = s.prompt(
        "first job",
        json!([write_content("a.txt", "one\n"), text("ok")]),
        &[],
    );
    s.prompt("second job", json!([text("fine")]), &["--continue"]);
    let out = s.run(&["sessions", "show", &id[..12]]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let shown = out_text(&out);
    assert!(shown.contains(&id), "{shown}");
    let t1 = shown
        .lines()
        .find(|l| l.contains("first job"))
        .expect(&shown);
    let t2 = shown
        .lines()
        .find(|l| l.contains("second job"))
        .expect(&shown);
    assert!(
        t1.trim_start().starts_with('1') && t1.contains("completed"),
        "{t1}"
    );
    assert!(t1.contains("1 file"), "{t1}");
    assert!(t2.trim_start().starts_with('2'), "{t2}");
    assert!(t1.contains("steps"), "step numbers next to the turn: {t1}");
}

#[test]
fn sessions_export_md() {
    let s = Scratch::new();
    s.write("a.txt", "old\n");
    let id = s.prompt(
        "change a.txt please",
        json!([write_content("a.txt", "one\n"), text("All done here.")]),
        &[],
    );
    let out = s.run(&["sessions", "export", &id, "--format", "md"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let md = out_text(&out);
    assert!(md.starts_with("# Session "), "{md}");
    for part in [
        "change a.txt please",
        "All done here.",
        "write",
        "a.txt",
        "## Turn 1",
    ] {
        assert!(md.contains(part), "{part} missing from:\n{md}");
    }
    // The default is markdown.
    let default = s.run(&["sessions", "export", &id]);
    assert_eq!(out_text(&default), md);

    let out = s.run(&["sessions", "export", &id, "--format", "jsonl"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let rows: Vec<Value> = out_text(&out)
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(rows[0]["type"], "header");
    assert_eq!(rows[0]["session_id"], id.as_str());
    assert!(rows.iter().any(|r| r["type"] == "record"));
}

#[test]
fn sessions_delete_removes_the_log() {
    let s = Scratch::new();
    let id = s.prompt("hello", json!([text("hi")]), &[]);
    let other = s.prompt("hello again", json!([text("hi")]), &[]);
    // Only the full id deletes.
    let out = s.run(&["sessions", "delete", &id[..10]]);
    assert_eq!(out.status.code(), Some(1));
    let out = s.run(&["sessions", "delete", &id]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(out_text(&out).contains(&id), "{}", out_text(&out));
    let list = out_text(&s.run(&["sessions", "list"]));
    assert!(!list.contains(&id) && list.contains(&other), "{list}");
    let out = s.run(&["sessions", "delete", &id]);
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn undo_restores_file() {
    let s = Scratch::new();
    s.write("a.txt", "old\n");
    let id = s.prompt(
        "edit",
        json!([write_content("a.txt", "new\n"), text("ok")]),
        &[],
    );
    assert_eq!(s.read("a.txt"), "new\n");
    let out = s.run(&["undo"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(s.read("a.txt"), "old\n");
    let said = out_text(&out);
    assert!(
        said.contains("undid turn 1") && said.contains("restored a.txt"),
        "{said}"
    );
    // The undo is in the session's log: `show` marks the turn.
    let shown = out_text(&s.run(&["sessions", "show", &id]));
    assert!(shown.contains("undone"), "{shown}");
}

#[test]
fn undo_deletes_a_file_the_turn_created_and_takes_a_turn_number() {
    let s = Scratch::new();
    s.prompt(
        "make b",
        json!([write_content("b.txt", "b\n"), text("ok")]),
        &[],
    );
    s.prompt(
        "make c",
        json!([write_content("c.txt", "c\n"), text("ok")]),
        &["--continue"],
    );
    // Turn 2 is the last; ask for turn 2 explicitly, then turn 1.
    let out = s.run(&["undo", "--turn", "2"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(
        out_text(&out).contains("deleted c.txt"),
        "{}",
        out_text(&out)
    );
    assert!(!s.exists("c.txt") && s.exists("b.txt"));
    let out = s.run(&["undo", "--turn", "1"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(!s.exists("b.txt"));
    let out = s.run(&["undo", "--turn", "9"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("no turn 9"), "{}", stderr(&out));
}

#[test]
fn undo_dry_run_changes_nothing() {
    let s = Scratch::new();
    s.write("a.txt", "old\n");
    let id = s.prompt(
        "edit",
        json!([write_content("a.txt", "new\n"), text("ok")]),
        &[],
    );
    let log = std::fs::read(
        std::fs::read_dir(s.home.path().join("sessions"))
            .unwrap()
            .flatten()
            .flat_map(|d| std::fs::read_dir(d.path()).unwrap().flatten())
            .find(|f| f.file_name().to_string_lossy().starts_with(&id))
            .unwrap()
            .path(),
    )
    .unwrap();
    let out = s.run(&["undo", "--dry-run"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let said = out_text(&out);
    assert!(
        said.contains("dry run") && said.contains("restore a.txt"),
        "{said}"
    );
    assert!(said.contains("nothing was changed"), "{said}");
    assert_eq!(s.read("a.txt"), "new\n");
    let shown = out_text(&s.run(&["sessions", "show", &id]));
    assert!(!shown.contains("undone"), "{shown}");
    // The log is byte for byte what it was.
    let after = std::fs::read(
        std::fs::read_dir(s.home.path().join("sessions"))
            .unwrap()
            .flatten()
            .flat_map(|d| std::fs::read_dir(d.path()).unwrap().flatten())
            .find(|f| f.file_name().to_string_lossy().starts_with(&id))
            .unwrap()
            .path(),
    )
    .unwrap();
    assert_eq!(log, after);
}

#[test]
fn undo_refuses_a_file_changed_since_the_turn() {
    let s = Scratch::new();
    s.write("a.txt", "old\n");
    s.write("b.txt", "old\n");
    s.prompt(
        "edit",
        json!([
            write_content("a.txt", "new\n"),
            write_content("b.txt", "new\n"),
            text("ok")
        ]),
        &[],
    );
    s.write("a.txt", "edited by hand\n");
    let out = s.run(&["undo"]);
    assert_eq!(out.status.code(), Some(1));
    let err = stderr(&out);
    assert!(
        err.contains("changed since the turn") && err.contains("a.txt"),
        "{err}"
    );
    assert!(err.contains("nothing was written"), "{err}");
    assert_eq!(s.read("a.txt"), "edited by hand\n");
    assert_eq!(s.read("b.txt"), "new\n", "nothing was written");
    // A dry run refuses the same way.
    let out = s.run(&["undo", "--dry-run"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("a.txt"), "{}", stderr(&out));
}

#[test]
fn undo_refuses_when_a_later_turn_changed_the_file() {
    let s = Scratch::new();
    s.write("a.txt", "old\n");
    s.prompt(
        "one",
        json!([write_content("a.txt", "one\n"), text("ok")]),
        &[],
    );
    s.prompt(
        "two",
        json!([write_content("a.txt", "two\n"), text("ok")]),
        &["--continue"],
    );
    let out = s.run(&["undo", "--turn", "1"]);
    assert_eq!(out.status.code(), Some(1));
    let err = stderr(&out);
    assert!(
        err.contains("a.txt") && err.contains("later turn") && err.contains('2'),
        "{err}"
    );
    assert!(err.contains("tautly undo --turn 2"), "the way out: {err}");
    assert_eq!(s.read("a.txt"), "two\n");
    // Newest first works, then the older one.
    assert_eq!(s.run(&["undo", "--turn", "2"]).status.code(), Some(0));
    assert_eq!(s.read("a.txt"), "one\n");
    assert_eq!(s.run(&["undo", "--turn", "1"]).status.code(), Some(0));
    assert_eq!(s.read("a.txt"), "old\n");
}

fn git(s: &Scratch, args: &[&str]) {
    let status = Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(args)
        .current_dir(s.work.path())
        .env("HOME", s.home.path())
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

#[test]
fn undo_in_a_git_work_tree_reverts_command_effects() {
    let s = Scratch::new();
    s.write("a.txt", "old\n");
    git(&s, &["init", "-q"]);
    git(&s, &["add", "."]);
    git(&s, &["commit", "-q", "-m", "init"]);
    let script = s.script(
        "s.json",
        json!([[
            {"tool_call": {"id": "b1", "name": "bash",
                           "input_json": json!({"command": "echo changed > a.txt && echo hi > new.txt"}).to_string()}},
            {"done": {"stop": "tool_use"}}
        ], text("ok")]),
    );
    let out = s.run(&[
        "-p",
        "run it",
        "--approve",
        "yolo",
        "--mock-script",
        &script,
    ]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(s.read("a.txt"), "changed\n");
    assert!(s.exists("new.txt"));
    let dry = s.run(&["undo", "--dry-run"]);
    assert!(
        out_text(&dry).contains("restore a.txt") && out_text(&dry).contains("delete new.txt"),
        "{}",
        out_text(&dry)
    );
    assert!(s.exists("new.txt"), "a dry run writes nothing");
    let out = s.run(&["undo"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(s.read("a.txt"), "old\n");
    assert!(!s.exists("new.txt"));
    let said = out_text(&out);
    assert!(said.contains("backup") && said.contains("undo-"), "{said}");
    // Deleting the session removes its checkpoint refs.
    let list = out_text(&s.run(&["sessions", "list"]));
    let id = list
        .split_whitespace()
        .find(|w| w.len() == 26)
        .unwrap()
        .to_string();
    let refs = |s: &Scratch| {
        let out = Command::new("git")
            .args(["for-each-ref", "refs/tautly"])
            .current_dir(s.work.path())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    assert!(refs(&s).contains(&id), "{}", refs(&s));
    let out = s.run(&["sessions", "delete", &id]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(
        out_text(&out).contains("checkpoint ref"),
        "{}",
        out_text(&out)
    );
    assert!(!refs(&s).contains(&id), "{}", refs(&s));
}

const MEM_A: &str = "---\nid: note-a\ntitle: \"Note A\"\nkind: fact\nanchors: []\ncreated: 2026-01-01T00:00:00\n---\nA.\n";
const MEM_B: &str = "---\nid: note-b\ntitle: \"Note B\"\nkind: fact\nanchors: []\ncreated: 2026-01-02T00:00:00\n---\nB, again.\n";

/// `memory consolidate` with the side model's reply `reply`, answering the prompts with
/// `answers` on stdin.
fn consolidate(s: &Scratch, reply: &str, answers: &str) -> Output {
    let script = s.script("c.json", json!([text(reply)]));
    let mut child = s
        .command(&["--mock-script", &script, "memory", "consolidate"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(answers.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn memory_consolidate_applies_what_you_approve() {
    let s = Scratch::new();
    s.write(".tautly/memory/note-a.md", MEM_A);
    s.write(".tautly/memory/note-b.md", MEM_B);
    let reply = r#"[{"delete": "note-a"}, {"delete": "note-b"}]"#;
    // Approve the first proposal, refuse the second.
    let out = consolidate(&s, reply, "y\nn\n");
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let said = out_text(&out);
    assert!(said.contains("Note A") && said.contains("Note B"), "{said}");
    assert!(
        said.contains("1 applied") && said.contains("1 rejected"),
        "{said}"
    );
    assert!(!s.exists(".tautly/memory/note-a.md"), "approved: deleted");
    assert!(s.exists(".tautly/memory/note-b.md"), "refused: kept");
}

#[test]
fn memory_consolidate_rejects_everything_without_answers() {
    let s = Scratch::new();
    s.write(".tautly/memory/note-a.md", MEM_A);
    let out = consolidate(&s, r#"[{"delete": "note-a"}]"#, "");
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(s.exists(".tautly/memory/note-a.md"));
    assert!(out_text(&out).contains("1 rejected"), "{}", out_text(&out));
}

#[test]
fn undo_session_flag_names_the_session() {
    let s = Scratch::new();
    let first = s.prompt(
        "make a",
        json!([write_content("a.txt", "a\n"), text("ok")]),
        &[],
    );
    // A second session (no --continue), the newest.
    s.prompt(
        "make b",
        json!([write_content("b.txt", "b\n"), text("ok")]),
        &[],
    );
    let out = s.run(&["undo", "--session", &first[..14]]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(!s.exists("a.txt") && s.exists("b.txt"));
    assert!(out_text(&out).contains(&first), "{}", out_text(&out));
    // Without the flag it is the newest session's turn.
    let out = s.run(&["undo"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(!s.exists("b.txt"));
}

#[test]
fn memory_consolidate_with_nothing_to_do_says_so() {
    let s = Scratch::new();
    let out = consolidate(&s, "[]", "");
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(
        out_text(&out).contains("no changes") || stderr(&out).contains("no memory"),
        "{} / {}",
        out_text(&out),
        stderr(&out)
    );
}
