//! `tautly bisect <session> --test "<cmd>"` (spec 7.12.7, Task 53) end to end on a
//! scripted provider (feature `mock`): `cargo test -p tautly --features mock --test
//! bisect`. Each run is in a scratch git repository with its own `TAUTLY_HOME`.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::{Value, json};
use tempfile::TempDir;

struct Scratch {
    home: TempDir,
    work: TempDir,
}

impl Scratch {
    /// A git repository with `a.txt` (`good`) and an executable `check.sh` (passes while
    /// `a.txt` says good) committed.
    fn new() -> Scratch {
        let s = Scratch {
            home: tempfile::tempdir().unwrap(),
            work: tempfile::tempdir().unwrap(),
        };
        s.write("a.txt", "good\n");
        s.write("check.sh", "#!/bin/sh\ngrep -q good a.txt\n");
        std::fs::set_permissions(
            s.work.path().join("check.sh"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        s.git(&["init", "-q", "-b", "main"]);
        s.git(&["add", "-A"]);
        s.git(&["commit", "-q", "-m", "init"]);
        s
    }

    fn root(&self) -> PathBuf {
        self.work.path().canonicalize().unwrap()
    }

    fn write(&self, name: &str, text: &str) {
        let p = self.work.path().join(name);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
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
            .env(
                "TAUTLY_TEST_SECRET_STORE",
                self.home.path().join("secrets.json"),
            )
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_DATA_HOME")
            .stdin(Stdio::null());
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

    /// A one-turn session: 1 user, 2 write x.txt, 3 break a.txt, 4 text. Its id.
    fn session(&self, breaks: bool) -> String {
        let a = if breaks { "bad\n" } else { "good again\n" };
        let script = self.script(
            "session.json",
            json!([
                write_call("w1", "x.txt", "x\n"),
                write_call("w2", "a.txt", a),
                text("done")
            ]),
        );
        let out = self.run(&[
            "-p",
            "go",
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

    /// Whatever bisect worktrees exist (directories under `<data home>/worktrees` and
    /// worktrees git knows of besides the main one).
    fn worktrees(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Ok(projects) = std::fs::read_dir(self.home.path().join("worktrees")) {
            for p in projects {
                for e in std::fs::read_dir(p.unwrap().path()).unwrap() {
                    out.push(e.unwrap().path().display().to_string());
                }
            }
        }
        if let Ok(entries) = std::fs::read_dir(self.root().join(".git/worktrees")) {
            for e in entries {
                out.push(e.unwrap().path().display().to_string());
            }
        }
        out
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

// --------------------------------------------------------------------------- tests

#[test]
fn asks_unless_yes() {
    let s = Scratch::new();
    let id = s.session(true);
    // Not a terminal and no --yes: it says what it would run and runs nothing.
    let out = s.run(&["bisect", &id, "--test", "./check.sh"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    let said = stdout(&out);
    assert!(said.contains("./check.sh"), "{said}");
    assert!(said.contains("at most 5 runs"), "{said}");
    assert!(said.contains("--yes"), "{said}");
    // The default mode asks before commands: the summary says the policy would ask.
    assert!(
        said.contains("the policy asks before this command"),
        "{said}"
    );
    // It does not claim the check is confined.
    assert!(said.contains("the check is not confined"), "{said}");
    assert!(s.worktrees().is_empty(), "{:?}", s.worktrees());

    // With --json, stdout stays empty when nothing ran: the summary and the refusal go to
    // stderr.
    let out = s.run(&["bisect", &id, "--test", "./check.sh", "--json"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert_eq!(stdout(&out), "");
    assert!(stderr(&out).contains("at most 5 runs"), "{}", stderr(&out));
    assert!(stderr(&out).contains("--yes"), "{}", stderr(&out));

    // With --yes it runs, and names the step, its turn and its calls.
    let out = s.run(&["bisect", &id, "--test", "./check.sh", "--yes"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let said = stdout(&out);
    assert!(
        said.contains("first failing step: 3 (user turn 1)"),
        "{said}"
    );
    assert!(said.contains("write: a.txt"), "{said}");
    assert!(s.worktrees().is_empty(), "{:?}", s.worktrees());

    // --json: one object.
    let out = s.run(&["bisect", &id, "--test", "./check.sh", "--yes", "--json"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let v = &lines(&out)[0];
    assert_eq!(v["first_bad"], 3);
    assert_eq!(v["turn"], 1);
    assert_eq!(v["calls"], json!(["write: a.txt"]));
    assert_eq!(v["last_good"], 2);
    assert!(v["runs"].as_u64().unwrap() >= 3);
    // The user's tree is as the session left it.
    assert_eq!(
        std::fs::read_to_string(s.root().join("a.txt")).unwrap(),
        "bad\n"
    );
}

#[test]
fn endpoint_mismatch_exits_2() {
    let s = Scratch::new();
    // a.txt still passes at the tip: the bad endpoint does not fail.
    let id = s.session(false);
    let out = s.run(&["bisect", &id, "--test", "./check.sh", "--yes"]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    let said = stderr(&out);
    assert!(said.contains("step 4 (bad step, the default)"), "{said}");
    assert!(said.contains("--good and --bad"), "{said}");
    assert!(s.worktrees().is_empty(), "{:?}", s.worktrees());
    // The run's last lines are shown.
    let out = s.run(&[
        "bisect",
        &id,
        "--test",
        "echo checking; echo it broke >&2; exit 3",
        "--yes",
    ]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    let said = stderr(&out);
    assert!(said.contains("step 0 (good step, the default)"), "{said}");
    assert!(
        said.contains("the last lines of that run's output"),
        "{said}"
    );
    assert!(said.contains("it broke"), "{said}");
    assert!(said.contains("exit code: 3"), "{said}");
    // A range that does not exist is an error too (exit 1).
    let out = s.run(&[
        "bisect",
        &id,
        "--test",
        "./check.sh",
        "--good",
        "9",
        "--yes",
    ]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
}

#[test]
fn deny_rule_refuses() {
    let s = Scratch::new();
    let id = s.session(true);
    s.write(
        ".tautly/config.toml",
        "[permissions]\ndeny = [\"bash(./check.sh*)\"]\n",
    );
    let out = s.run(&["bisect", &id, "--test", "./check.sh", "--yes"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    let said = stderr(&out);
    assert!(said.contains("bash(./check.sh*)"), "{said}");
    assert!(s.worktrees().is_empty(), "{:?}", s.worktrees());
}

/// Whether process `pid` exists (a zombie counts).
fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

#[test]
fn signals_kill_the_check_and_clean_up() {
    for (sig, code) in [
        (libc::SIGHUP, 129),
        (libc::SIGTERM, 143),
        (libc::SIGINT, 130),
    ] {
        let s = Scratch::new();
        let id = s.session(true);
        let pid_file = s.home.path().join("check.pid");
        // The check records its pid, then waits far longer than the test.
        let test = format!("echo $$ > '{}'; exec sleep 40", pid_file.display());
        let mut child = s
            .command(&["bisect", &id, "--test", &test, "--yes"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let started = std::time::Instant::now();
        let pid = loop {
            if let Some(pid) = std::fs::read_to_string(&pid_file)
                .ok()
                .and_then(|t| t.trim().parse::<i32>().ok())
            {
                break pid;
            }
            assert!(
                started.elapsed() < std::time::Duration::from_secs(30),
                "the check never started"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        };
        assert!(alive(pid));
        assert!(!s.worktrees().is_empty(), "the check runs in its worktree");
        // SAFETY: a signal to the child this test started.
        unsafe { libc::kill(i32::try_from(child.id()).unwrap(), sig) };
        let waited = std::time::Instant::now();
        while child.try_wait().unwrap().is_none() {
            if waited.elapsed() > std::time::Duration::from_secs(20) {
                let _ = child.kill();
                panic!("tautly did not stop after signal {sig}");
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let out = child.wait_with_output().unwrap();
        assert_eq!(
            out.status.code(),
            Some(code),
            "signal {sig}: {}",
            stderr(&out)
        );
        assert!(stderr(&out).contains("cancelled"), "{}", stderr(&out));
        // The check is gone, and so is everything the bisect made.
        let gone = std::time::Instant::now();
        while alive(pid) && gone.elapsed() < std::time::Duration::from_secs(5) {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(!alive(pid), "signal {sig}: the check survived");
        assert!(
            s.worktrees().is_empty(),
            "signal {sig}: {:?}",
            s.worktrees()
        );
    }
}
