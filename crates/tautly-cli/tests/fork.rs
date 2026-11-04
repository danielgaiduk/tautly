//! `tautly fork <session> --at N` (spec 7.12.4, Task 50) end to end on a scripted
//! provider (feature `mock`): `cargo test -p tautly --features mock --test fork`. Each run
//! is in a scratch git repository with its own `TAUTLY_HOME`.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::{Value, json};
use tempfile::TempDir;

struct Scratch {
    home: TempDir,
    work: TempDir,
}

impl Scratch {
    /// A git repository with `a.txt` committed.
    fn new() -> Scratch {
        let s = Scratch {
            home: tempfile::tempdir().unwrap(),
            work: tempfile::tempdir().unwrap(),
        };
        s.write("a.txt", "zero\n");
        s.git(&["init", "-q", "-b", "main"]);
        s.git(&["add", "-A"]);
        s.git(&["commit", "-q", "-m", "init"]);
        s
    }

    fn root(&self) -> PathBuf {
        self.work.path().canonicalize().unwrap()
    }

    fn write(&self, name: &str, text: &str) {
        std::fs::write(self.work.path().join(name), text).unwrap();
    }

    fn git(&self, args: &[&str]) -> String {
        git_in(self.work.path(), args)
    }

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
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            // The keychain is a file that does not exist: empty, never the real one.
            .env(
                "TAUTLY_TEST_SECRET_STORE",
                self.home.path().join("secrets.json"),
            )
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_DATA_HOME");
        for var in [
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
            "GEMINI_API_KEY",
            "OPENROUTER_API_KEY",
        ] {
            c.env_remove(var);
        }
        c
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    /// A one-turn session that writes `a.txt` (steps: 1 user, 2 write, 3 text); its id.
    fn parent(&self) -> String {
        let script = self.script(
            "parent.json",
            json!([write_call("w1", "a.txt", "one\n"), text("first")]),
        );
        let out = self.run(&[
            "-p",
            "one",
            "--approve",
            "auto-edit",
            "--output",
            "json",
            "--mock-script",
            &script,
        ]);
        assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
        lines(&out)[0]["session_id"].as_str().unwrap().to_string()
    }

    /// `tautly fork <id> --at <at>` without a prompt: the worktree and the fork's id, from
    /// its line.
    fn fork(&self, id: &str, at: &str) -> (PathBuf, String) {
        let out = self.run(&["fork", id, "--at", at]);
        assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
        parse_fork_line(&stdout(&out))
    }
}

fn git_in(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// `worktree: <path>  session: <id>  resume: cd <path> && tautly --resume <id>`.
fn parse_fork_line(text: &str) -> (PathBuf, String) {
    let line = text
        .lines()
        .find(|l| l.starts_with("worktree: "))
        .unwrap_or_else(|| panic!("no worktree line: {text}"));
    let rest = line.strip_prefix("worktree: ").unwrap();
    let (path, rest) = rest.split_once("  session: ").unwrap();
    let id = rest.split("  ").next().unwrap().to_string();
    (PathBuf::from(path), id)
}

fn text(t: &str) -> Value {
    json!([{"text_delta": t}, {"done": {"stop": "end_turn"}}])
}

fn write_call(id: &str, path: &str, content: &str) -> Value {
    json!([
        {"tool_call": {"id": id, "name": "write",
                       "input_json": json!({"path": path, "content": content}).to_string()}},
        {"done": {"stop": "tool_use"}}
    ])
}

fn bash_call(id: &str, command: &str) -> Value {
    json!([
        {"tool_call": {"id": id, "name": "bash",
                       "input_json": json!({"command": command}).to_string()}},
        {"done": {"stop": "tool_use"}}
    ])
}

fn lines(out: &Output) -> Vec<Value> {
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("not JSON ({e}): {l}")))
        .collect()
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
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

// --------------------------------------------------------------------------- tests

#[test]
fn fork_without_a_prompt_prints_the_worktree_and_the_resume_command() {
    let s = Scratch::new();
    let id = s.parent();
    let out = s.run(&["fork", &id, "--at", "1", "--label", "before"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let said = stdout(&out);
    let (wt, fork) = parse_fork_line(&said);
    assert_ne!(fork, id);
    assert!(
        said.contains(&format!(
            "resume: cd {} && tautly --resume {fork}",
            wt.display()
        )),
        "{said}"
    );
    // The default place: <data home>/worktrees/<project-id>/<session-id>.
    let home = s.home.path().canonicalize().unwrap();
    assert_eq!(
        wt.parent().unwrap().parent().unwrap(),
        home.join("worktrees")
    );
    assert_eq!(wt.file_name().unwrap().to_str().unwrap(), fork);
    // At step 1 (the user message) nothing was written yet.
    assert_eq!(std::fs::read_to_string(wt.join("a.txt")).unwrap(), "zero\n");
    assert_eq!(
        std::fs::read_to_string(s.root().join("a.txt")).unwrap(),
        "one\n"
    );
    // JSON, at a directory of one's choosing.
    let dir = s.home.path().join("mine");
    let out = s.run(&[
        "fork",
        &id,
        "--at",
        "3",
        "--dir",
        dir.to_str().unwrap(),
        "--output",
        "json",
    ]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let v: Value = serde_json::from_str(stdout(&out).trim()).unwrap();
    assert_eq!(v["at"], 3);
    assert_eq!(
        PathBuf::from(v["worktree"].as_str().unwrap()),
        dir.canonicalize().unwrap()
    );
    assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "one\n");
    // A step that does not exist changes nothing.
    let out = s.run(&["fork", &id, "--at", "9"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("no step 9"), "{}", stderr(&out));
}

#[test]
fn prompt_runs_headless_in_worktree_only() {
    let s = Scratch::new();
    let id = s.parent();
    let script = s.script(
        "fork.json",
        json!([write_call("w2", "new.txt", "fork\n"), text("done")]),
    );
    let out = s.run(&[
        "--mock-script",
        &script,
        "fork",
        &id,
        "--at",
        "3",
        "--prompt",
        "two",
        "--approve",
        "auto-edit",
        "--output",
        "json",
    ]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let (wt, fork) = parse_fork_line(&stderr(&out));
    let evs = lines(&out);
    assert_eq!(evs[0]["type"], "session_changed");
    assert_eq!(evs[0]["session_id"], fork.as_str());
    assert!(
        evs.iter()
            .any(|e| e["type"] == "turn_finished" && e["reason"] == "completed"),
        "{evs:?}"
    );
    assert_eq!(
        std::fs::read_to_string(wt.join("new.txt")).unwrap(),
        "fork\n"
    );
    assert!(
        !s.root().join("new.txt").exists(),
        "the original is untouched"
    );
    assert_eq!(std::fs::read_to_string(wt.join("a.txt")).unwrap(), "one\n");
    // The same exit codes as `tautly -p`: a spent budget is 2.
    let script = s.script("never.json", json!([text("never sent")]));
    let out = s.run(&[
        "--mock-script",
        &script,
        "fork",
        &id,
        "--at",
        "3",
        "--prompt",
        "three",
        "--budget-usd",
        "0",
    ]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
}

#[test]
fn logged_approvals_are_not_consent() {
    // Review Focus 13: the parent's user approved `bash` (interactively, over RPC); the
    // fork, headless without --approve, is asked the same call and denies it.
    let s = Scratch::new();
    let script = s.script(
        "rpc.json",
        json!([bash_call("b1", "printf x > x.txt"), text("ran it")]),
    );
    let mut child = s
        .command(&["--rpc", "--mock-script", &script])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut out = std::io::BufReader::new(child.stdout.take().unwrap());
    let mut send = |m: Value| writeln!(stdin, "{m}").unwrap();
    send(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": {"client": {"interactive": true}}}));
    send(json!({"jsonrpc": "2.0", "id": 2, "method": "send_message", "params": {"text": "run"}}));
    let seen = read_until(&mut out, |m| {
        m["params"]["event"]["type"] == "tool_approval_requested"
    });
    let ask = &seen.last().unwrap()["params"]["event"];
    let request = ask["request_id"].as_str().unwrap().to_string();
    send(json!({"jsonrpc": "2.0", "id": 3, "method": "approve",
                "params": {"request_id": request}}));
    let seen = read_until(&mut out, |m| {
        m["params"]["event"]["type"] == "turn_finished"
    });
    let id = seen
        .iter()
        .find(|m| m["id"] == 1)
        .map(|m| m["result"]["session_id"].as_str().unwrap().to_string())
        .unwrap_or_else(|| {
            seen.iter()
                .find_map(|m| m["params"]["session_id"].as_str().map(str::to_string))
                .unwrap()
        });
    drop(stdin);
    let end = child.wait_with_output().unwrap();
    assert_eq!(end.status.code(), Some(0), "{}", stderr(&end));
    assert!(s.root().join("x.txt").exists(), "the parent ran it");
    std::fs::remove_file(s.root().join("x.txt")).unwrap();

    // At step 1 (the message), the same call again: denied, not run.
    let script = s.script(
        "fork.json",
        json!([bash_call("b2", "printf x > x.txt"), text("denied")]),
    );
    let out = s.run(&[
        "--mock-script",
        &script,
        "fork",
        &id,
        "--at",
        "1",
        "--prompt",
        "run again",
        "--output",
        "json",
    ]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let (wt, _) = parse_fork_line(&stderr(&out));
    let evs = lines(&out);
    assert!(
        evs.iter()
            .any(|e| e["type"] == "tool_finished" && e["is_error"] == true),
        "{evs:?}"
    );
    assert!(
        !evs.iter().any(|e| e["type"] == "tool_started"),
        "the call ran: {evs:?}"
    );
    assert!(!wt.join("x.txt").exists());
    assert!(!s.root().join("x.txt").exists());
}

#[test]
fn sessions_delete_removes_clean_worktree_refuses_dirty_without_force() {
    let s = Scratch::new();
    let id = s.parent();
    let (wt, fork) = s.fork(&id, "3");
    assert!(
        s.git(&["worktree", "list"])
            .contains(&*wt.to_string_lossy())
    );
    let out = s.run(&["sessions", "delete", &fork]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(
        stdout(&out).contains("removed its worktree"),
        "{}",
        stdout(&out)
    );
    assert!(!wt.exists());
    assert!(
        !s.git(&["worktree", "list"])
            .contains(&*wt.to_string_lossy())
    );
    // The parent and its work tree are untouched.
    assert_eq!(
        std::fs::read_to_string(s.root().join("a.txt")).unwrap(),
        "one\n"
    );
    assert!(stdout(&s.run(&["sessions", "list"])).contains(&id));

    // A worktree with changes is refused, and nothing is deleted.
    let (wt, fork) = s.fork(&id, "3");
    std::fs::write(wt.join("mine.txt"), "keep me\n").unwrap();
    let out = s.run(&["sessions", "delete", &fork]);
    assert_eq!(out.status.code(), Some(1), "{}", stdout(&out));
    assert!(stderr(&out).contains("--force"), "{}", stderr(&out));
    assert!(wt.join("mine.txt").exists());
    let out = s.run(&["sessions", "show", &fork]);
    assert_eq!(out.status.code(), Some(0), "the log is still there");
    let out = s.run(&["sessions", "delete", &fork, "--force"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(!wt.exists());
}

#[test]
fn delete_from_inside_the_fork_is_refused_with_a_hint() {
    // Review I1.
    let s = Scratch::new();
    let id = s.parent();
    let (wt, fork) = s.fork(&id, "3");
    let out = s
        .command(&["sessions", "delete", &fork])
        .current_dir(&wt)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", stdout(&out));
    assert!(
        stderr(&out).contains("another worktree"),
        "{}",
        stderr(&out)
    );
    assert!(wt.exists());
    assert_eq!(s.run(&["sessions", "show", &fork]).status.code(), Some(0));
}

#[test]
fn a_model_without_a_key_is_warned_about_at_fork_time() {
    // Review M7: no key for its provider (no key variable, an empty keychain).
    let s = Scratch::new();
    let id = s.parent();
    let out = s.run(&["fork", &id, "--at", "3", "--model", "openai/gpt-5"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let said = stderr(&out);
    assert!(
        said.contains("warning") && said.contains("openai"),
        "{said}"
    );
}

#[test]
fn fork_refuses_outside_git() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let s = Scratch { home, work };
    s.write("a.txt", "zero\n");
    let id = s.parent();
    let out = s.run(&["fork", &id, "--at", "1"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("git"), "{}", stderr(&out));
}

#[test]
fn rpc_session_new_fork_worktree() {
    let s = Scratch::new();
    let id = s.parent();
    let script = s.script(
        "rpc.json",
        json!([write_call("w2", "rpc.txt", "over rpc\n"), text("done")]),
    );
    let mut child = s
        .command(&["--rpc", "--approve", "auto-edit", "--mock-script", &script])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut out = std::io::BufReader::new(child.stdout.take().unwrap());
    let mut send = |m: Value| writeln!(stdin, "{m}").unwrap();
    send(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": {"session": false}}));
    let root = s.root();
    send(
        json!({"jsonrpc": "2.0", "id": 2, "method": "session/new", "params": {
            "root": root,
            "fork": {"session_id": id, "at": 3, "label": "what-if", "worktree": true},
        }}),
    );
    let seen = read_until(&mut out, |m| m["id"] == 2);
    let reply = seen.last().unwrap();
    let fork = reply["result"]["session_id"]
        .as_str()
        .unwrap_or_else(|| panic!("{reply}"))
        .to_string();
    let wt = PathBuf::from(reply["result"]["worktree"].as_str().unwrap());
    assert_ne!(fork, id);
    assert_eq!(std::fs::read_to_string(wt.join("a.txt")).unwrap(), "one\n");
    send(json!({"jsonrpc": "2.0", "id": 3, "method": "send_message",
                "params": {"session_id": fork, "text": "write"}}));
    read_until(&mut out, |m| {
        m["params"]["event"]["type"] == "turn_finished"
    });
    assert_eq!(
        std::fs::read_to_string(wt.join("rpc.txt")).unwrap(),
        "over rpc\n"
    );
    assert!(!root.join("rpc.txt").exists());
    // A worktree fork with a model the server does not know is refused.
    send(
        json!({"jsonrpc": "2.0", "id": 4, "method": "session/new", "params": {
            "root": root,
            "fork": {"session_id": id, "at": 3, "worktree": true, "model": "nope"},
        }}),
    );
    let seen = read_until(&mut out, |m| m["id"] == 4);
    assert!(seen.last().unwrap()["error"].is_object(), "{seen:?}");
    drop(stdin);
    let end = child.wait_with_output().unwrap();
    assert_eq!(end.status.code(), Some(0), "{}", stderr(&end));
    // The fork's session runs in its worktree and is listed there.
    let out = Command::new(env!("CARGO_BIN_EXE_tautly"))
        .args(["sessions", "list"])
        .current_dir(&wt)
        .env("TAUTLY_HOME", s.home.path())
        .env("HOME", s.home.path())
        .output()
        .unwrap();
    assert!(stdout(&out).contains(&fork), "{}", stdout(&out));
}
