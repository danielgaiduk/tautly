//! The `tautly` binary's argument handling. These runs never reach a provider: they stop
//! at usage errors, before any session starts. Each runs in a scratch directory with its
//! own `TAUTLY_HOME`.

use std::process::{Command, Output};

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

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_tautly"))
            .args(args)
            .current_dir(self.work.path())
            .env("TAUTLY_HOME", self.home.path())
            .env("HOME", self.home.path())
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_DATA_HOME")
            .output()
            .unwrap()
    }
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[test]
fn help_lists_the_flags() {
    let s = Scratch::new();
    let out = s.run(&["--help"]);
    assert!(out.status.success());
    let help = stdout(&out);
    for flag in [
        "-p, --prompt",
        "--output",
        "--approve <MODE>",
        "--model",
        "--budget-usd",
        "--approve-memory",
        "--approve-mcp",
        "--continue",
        "--resume",
        "--rpc",
        "--no-checkpoints",
        "auto-edit",
        "yolo",
    ] {
        assert!(help.contains(flag), "{flag} missing from:\n{help}");
    }
    assert!(!help.contains("mock-script"), "the test flag is hidden");
    let out = s.run(&["--version"]);
    assert!(out.status.success());
    assert!(stdout(&out).starts_with("tautly "), "{}", stdout(&out));
}

#[test]
fn bad_flags_are_usage_errors() {
    let s = Scratch::new();
    for args in [
        &["--approve", "never", "--rpc"][..],
        &["-p", "hi", "--rpc"],
        &["-p", "hi", "--continue", "--resume", "01ABC"],
        &["-p", "hi", "--budget-usd", "-1"],
        &["-p", "hi", "--budget-usd", "lots"],
        &["--rpc", "--output", "json"],
        &["-p", "hi", "--output", "yaml"],
    ] {
        let out = s.run(args);
        assert_eq!(out.status.code(), Some(2), "{args:?}: {}", stderr(&out));
    }
}

#[test]
fn without_a_mode_it_says_what_to_use() {
    let s = Scratch::new();
    let out = s.run(&[]);
    assert_eq!(out.status.code(), Some(2));
    let err = stderr(&out);
    assert!(err.contains("-p") && err.contains("--rpc"), "{err}");
}

#[test]
fn an_unknown_provider_is_refused_before_anything_starts() {
    let s = Scratch::new();
    let out = s.run(&["-p", "hi", "--model", "acme/turbo"]);
    assert_eq!(out.status.code(), Some(1));
    let err = stderr(&out);
    assert!(
        err.contains("acme/turbo") && err.contains("anthropic"),
        "{err}"
    );
    assert!(stdout(&out).is_empty());
}

#[test]
fn continue_without_an_earlier_session_is_an_error() {
    let s = Scratch::new();
    let out = s.run(&["-p", "hi", "--continue", "--model", "openai/gpt-5"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("no earlier session"),
        "{}",
        stderr(&out)
    );
}
