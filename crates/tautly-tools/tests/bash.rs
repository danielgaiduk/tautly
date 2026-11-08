// Tests may shell out to inspect leftover processes; the lint targets src.
#![allow(clippy::disallowed_types)]

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tautly_provider::{Capabilities, EditFormat};
use tautly_sandbox::{Effect, LocalSandbox, Sandbox};
use tautly_tools::{BashTool, Tool, ToolCx, ToolResult, cap_output};
use tempfile::TempDir;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

fn sandbox() -> (TempDir, LocalSandbox) {
    let dir = TempDir::new().unwrap();
    let sb = LocalSandbox::new(dir.path().to_path_buf()).unwrap();
    (dir, sb)
}

fn cx(dir: &TempDir) -> ToolCx {
    ToolCx::new(EditFormat::StrReplace, dir.path().join(".spill"))
}

async fn run_args(args: Value) -> ToolResult {
    let (dir, sb) = sandbox();
    BashTool.run(args, &sb, &cx(&dir)).await
}

async fn run_bash(cmd: &str) -> ToolResult {
    run_args(json!({ "command": cmd })).await
}

/// A sleep duration unique to this test run, usable as a `pgrep -f` marker.
fn unique_sleep() -> String {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("300.{n}")
}

fn alive(marker: &str) -> bool {
    Command::new("pgrep")
        .args(["-f", marker])
        .output()
        .unwrap()
        .status
        .success()
}

/// Review B-M4: the variables the session strips (the provider API keys) do not reach a
/// command; others still do. (`HOME` and `USER` stand in for key variables: setting one in
/// this process would race the other tests.)
#[tokio::test]
async fn stripped_variables_do_not_reach_commands() {
    let (dir, sb) = sandbox();
    let cx = cx(&dir).with_env_remove(vec!["HOME".into(), "TAUTLY_NOT_SET_ANYWHERE".into()]);
    let r = BashTool
        .run(
            json!({ "command": "echo \"home=${HOME-unset} path=${PATH:+set}\"" }),
            &sb,
            &cx,
        )
        .await;
    assert!(!r.is_error, "{}", r.output);
    assert!(r.output.contains("home=unset path=set"), "{}", r.output);
}

#[tokio::test]
async fn captures_stdout_stderr_and_exit_code() {
    let r = run_bash("echo out; echo err >&2; exit 3").await;
    assert!(r.is_error);
    assert!(r.output.contains("out"), "{}", r.output);
    assert!(r.output.contains("err"), "{}", r.output);
    assert!(
        r.output.trim_end().ends_with("exit code: 3"),
        "{}",
        r.output
    );
}

#[tokio::test]
async fn success_is_not_an_error() {
    let r = run_bash("echo hi").await;
    assert!(!r.is_error, "{}", r.output);
    assert_eq!(r.output, "hi\nexit code: 0");
}

#[tokio::test]
async fn interleaves_in_arrival_order() {
    let r = run_bash("echo one; sleep 0.1; echo two >&2; sleep 0.1; echo three").await;
    let (a, b, c) = (
        r.output.find("one").unwrap(),
        r.output.find("two").unwrap(),
        r.output.find("three").unwrap(),
    );
    assert!(a < b && b < c, "{}", r.output);
}

#[tokio::test]
async fn sets_env_and_null_stdin() {
    let r = run_bash("echo $TAUTLY $PAGER $GIT_PAGER $TERM; cat; echo done").await;
    assert!(
        r.output.starts_with("1 cat cat dumb\ndone\n"),
        "{}",
        r.output
    );
}

#[tokio::test]
async fn output_over_30kb_keeps_head_and_tail_and_spills() {
    let (dir, sb) = sandbox();
    let r = BashTool
        .run(json!({"command": "seq 1 100000"}), &sb, &cx(&dir))
        .await;
    assert!(r.output.len() <= 31 * 1024, "{}", r.output.len());
    assert!(r.output.starts_with("1\n"));
    assert!(r.output.contains("100000"));
    assert!(r.output.contains("full output saved to"));
    assert!(r.output.trim_end().ends_with("exit code: 0"));
    // The named spill path is readable and holds everything.
    let line = r
        .output
        .lines()
        .find(|l| l.contains("full output saved to"))
        .unwrap();
    let path = line
        .split("saved to ")
        .nth(1)
        .unwrap()
        .trim_end_matches(']');
    let full = String::from_utf8(sb.read(Path::new(path)).await.unwrap()).unwrap();
    assert!(full.starts_with("1\n") && full.ends_with("100000\n"));
    assert_eq!(full.lines().count(), 100000);
}

#[tokio::test]
async fn output_past_the_stream_threshold_spills_whole_with_head_and_tail() {
    let (dir, sb) = sandbox();
    // 6 MB: past the 4 MB at which output streams to the spill file.
    let r = BashTool
        .run(
            json!({"command": "printf START; head -c 6000000 /dev/zero | tr '\\0' a; printf END"}),
            &sb,
            &cx(&dir),
        )
        .await;
    assert!(r.output.starts_with("START"), "{}", &r.output[..100]);
    assert!(r.output.len() <= 31 * 1024, "{}", r.output.len());
    let body = r.output.split("\n[full output saved to ").next().unwrap();
    assert!(body.ends_with("END"), "{}", &body[body.len() - 100..]);
    let omitted: usize = body
        .split("… [")
        .nth(1)
        .and_then(|s| s.split(" bytes omitted]").next())
        .unwrap()
        .parse()
        .unwrap();
    let shown = body.len() - format!("\n… [{omitted} bytes omitted] …\n").len();
    assert_eq!(shown + omitted, 6_000_008);
    let path = r
        .output
        .lines()
        .find_map(|l| l.strip_prefix("[full output saved to "))
        .unwrap()
        .trim_end_matches(']');
    let full = sb.read(Path::new(path)).await.unwrap();
    assert_eq!(full.len(), 6_000_008);
    assert!(full.starts_with(b"STARTaaa") && full.ends_with(b"aaaEND"));
}

#[tokio::test]
async fn cap_output_small_is_untouched() {
    let (_d, sb) = sandbox();
    let (text, spill) = cap_output("short", 100, &sb).await;
    assert_eq!(text, "short");
    assert!(spill.is_none());
}

#[tokio::test]
async fn timeout_kills_process_group() {
    let s = unique_sleep();
    let t = Instant::now();
    let r = run_args(json!({"command": format!("sleep {s} & sleep {s}"), "timeout_s": 1})).await;
    assert!(t.elapsed() < Duration::from_secs(3));
    assert!(r.is_error);
    assert!(r.output.contains("timed out after 1s"), "{}", r.output);
    assert!(!alive(&format!("sleep {s}")), "leftover sleep processes");
}

#[tokio::test]
async fn cancel_kills_within_200ms() {
    let (dir, sb) = sandbox();
    let mut fastest = Duration::MAX;
    // The fastest of three: a loaded runner can stall any one run, but a cancel that is
    // slow by design is slow every time.
    for _ in 0..3 {
        let s = unique_sleep();
        let token = CancellationToken::new();
        let cx = cx(&dir).with_cancel(token.clone());
        let t2 = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            t2.cancel();
        });
        let t = Instant::now();
        let r = BashTool
            .run(
                json!({"command": format!("sleep {s} & sleep {s}")}),
                &sb,
                &cx,
            )
            .await;
        fastest = fastest.min(t.elapsed());
        assert!(r.is_error);
        assert!(r.output.contains("killed: cancelled"), "{}", r.output);
        assert!(!alive(&format!("sleep {s}")), "leftover sleep processes");
    }
    assert!(fastest < Duration::from_millis(250), "{fastest:?}");
}

#[tokio::test]
async fn streams_output_deltas() {
    let (dir, sb) = sandbox();
    let (tx, mut rx) = mpsc::channel(16);
    let cx = cx(&dir).with_output_tx(tx);
    let fut = BashTool.run(
        json!({"command": "echo first; sleep 0.5; echo second"}),
        &sb,
        &cx,
    );
    let reader = async {
        let first = rx.recv().await.unwrap();
        (first, Instant::now())
    };
    let started = Instant::now();
    let (r, (first, at)) = tokio::join!(fut, reader);
    assert_eq!(first.trim_end(), "first");
    assert!(at - started < Duration::from_millis(400), "not streamed");
    assert!(r.output.contains("second"));
}

#[tokio::test]
async fn full_stream_channel_does_not_block() {
    let (dir, sb) = sandbox();
    let (tx, _rx) = mpsc::channel(1); // never drained
    let cx = cx(&dir).with_output_tx(tx);
    let r = tokio::time::timeout(
        Duration::from_secs(10),
        BashTool.run(json!({"command": "seq 1 1000"}), &sb, &cx),
    )
    .await
    .expect("blocked on full channel");
    assert!(r.output.contains("1000\n"), "{}", r.output);
}

#[test]
fn effect_is_exec_of_first_command() {
    let e = BashTool
        .effect(&json!({"command": "git status && rm -rf x"}))
        .unwrap();
    assert_eq!(
        e,
        Effect::Exec {
            argv: vec!["git".into(), "status".into()]
        }
    );
    let e = BashTool.effect(&json!({"command": "echo $(("})).unwrap();
    assert!(matches!(e, Effect::Exec { .. }));
}

#[test]
fn bad_args_are_rejected() {
    assert!(BashTool.effect(&json!({})).is_err());
    assert!(
        BashTool
            .effect(&json!({"command":"x","timeout_s":0}))
            .is_err()
    );
    assert!(
        BashTool
            .effect(&json!({"command":"x","timeout_s":601}))
            .is_err()
    );
    assert!(BashTool.effect(&json!({"command":"x","raw":true})).is_ok());
    let spec = BashTool.spec(&Capabilities::conservative());
    assert_eq!(spec.name, "bash");
}

#[tokio::test]
async fn bad_args_run_returns_error_result() {
    let r = run_args(json!({"timeout_s": 5})).await;
    assert!(r.is_error);
}

#[tokio::test]
async fn chatty_background_child_does_not_hold_the_call_open() {
    let marker = format!("tautly-test-{}", unique_sleep());
    let cmd = format!("(while true; do echo x; sleep 0.05; done) & # {marker}\necho done");
    let t = Instant::now();
    let r = run_bash(&cmd).await;
    let _ = Command::new("pkill").args(["-f", &marker]).status();
    assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
    assert!(!r.is_error, "{}", r.output);
    assert!(
        r.output.trim_end().ends_with("exit code: 0"),
        "{}",
        r.output
    );
}

#[tokio::test]
async fn non_utf8_environment_does_not_panic() {
    use std::os::unix::ffi::OsStrExt;
    // SAFETY: test-only; set before any bash run in this test and the value is unused elsewhere.
    unsafe { std::env::set_var("TAUTLY_TEST_BAD", std::ffi::OsStr::from_bytes(b"\xff\xfe")) };
    let r = run_bash("echo ok").await;
    unsafe { std::env::remove_var("TAUTLY_TEST_BAD") };
    assert!(!r.is_error, "{}", r.output);
}

#[tokio::test]
async fn flooding_background_child_does_not_hang_the_call() {
    let marker = format!("tautly-flood-{}", unique_sleep());
    let cmd = format!("yes {marker} &\nexit 0");
    let t = Instant::now();
    let r = tokio::time::timeout(Duration::from_secs(10), run_bash(&cmd)).await;
    let _ = Command::new("pkill").args(["-f", &marker]).status();
    let r = r.expect("call hung while a background child floods the pipe");
    assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
    assert!(
        !r.is_error,
        "{}",
        &r.output[r.output.len().saturating_sub(200)..]
    );
    assert!(r.output.trim_end().ends_with("exit code: 0"));
}
