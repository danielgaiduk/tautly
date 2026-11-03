//! The process-group registry (Task 25): `bash` registers each command's group, keeps a
//! group with a background child until it is terminated, releases one that emptied, and
//! kills the group when its call is dropped half way.

use std::time::{Duration, Instant};

use serde_json::json;
use tautly_provider::EditFormat;
use tautly_sandbox::LocalSandbox;
use tautly_tools::{BashTool, ProcessGroups, Tool, ToolCx, ToolResult, group_alive};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

struct Env {
    _dir: TempDir,
    sb: LocalSandbox,
    groups: ProcessGroups,
    spill: std::path::PathBuf,
}

fn env() -> Env {
    let dir = TempDir::new().unwrap();
    let sb = LocalSandbox::new(dir.path().to_path_buf()).unwrap();
    Env {
        spill: dir.path().join(".spill"),
        _dir: dir,
        sb,
        groups: ProcessGroups::default(),
    }
}

impl Env {
    fn cx(&self, cancel: CancellationToken) -> ToolCx {
        ToolCx::new(EditFormat::StrReplace, self.spill.clone())
            .with_process_groups(self.groups.clone())
            .with_cancel(cancel)
    }

    async fn run(&self, cmd: &str, cancel: CancellationToken) -> ToolResult {
        BashTool
            .run(json!({ "command": cmd }), &self.sb, &self.cx(cancel))
            .await
    }

    /// The one live group, once it shows up.
    async fn wait_for_group(&self) -> u32 {
        let end = Instant::now() + Duration::from_secs(10);
        loop {
            if let [pgid] = self.groups.live()[..] {
                return pgid;
            }
            assert!(Instant::now() < end, "no group registered");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

/// Wait (briefly) until group `pgid` is gone: a killed process may stay a zombie for a
/// moment, until its parent (the runtime, or init for an orphan) reaps it.
async fn gone(pgid: u32) {
    let end = Instant::now() + Duration::from_secs(1);
    while group_alive(pgid) {
        assert!(Instant::now() < end, "group {pgid} is still alive");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test]
async fn a_command_that_exits_leaves_no_group_registered() {
    let e = env();
    let r = e.run("echo hi", CancellationToken::new()).await;
    assert!(!r.is_error, "{}", r.output);
    assert!(e.groups.live().is_empty());
}

#[tokio::test]
async fn a_background_child_stays_registered_until_terminated() {
    let e = env();
    let r = e
        .run("sleep 305 >/dev/null 2>&1 &", CancellationToken::new())
        .await;
    assert!(!r.is_error, "{}", r.output);
    let pgid = e.wait_for_group().await;
    assert!(group_alive(pgid), "the background child should still run");
    assert_eq!(e.groups.terminate_all(Duration::from_millis(500)).await, 1);
    gone(pgid).await;
    assert!(e.groups.live().is_empty());
}

#[tokio::test]
async fn terminate_all_kills_a_group_that_ignores_sigterm() {
    let e = env();
    let r = e
        .run(
            "trap '' TERM; sleep 307 >/dev/null 2>&1 &",
            CancellationToken::new(),
        )
        .await;
    assert!(!r.is_error, "{}", r.output);
    let pgid = e.wait_for_group().await;
    let t0 = Instant::now();
    e.groups.terminate_all(Duration::from_millis(100)).await;
    assert!(t0.elapsed() < Duration::from_secs(2), "{:?}", t0.elapsed());
    gone(pgid).await;
}

#[tokio::test]
async fn dropping_a_running_call_kills_its_group() {
    let e = env();
    let cx = e.cx(CancellationToken::new());
    let call = BashTool.run(json!({ "command": "sleep 304" }), &e.sb, &cx);
    let pgid = tokio::select! {
        _ = call => panic!("sleep 304 returned"),
        pgid = e.wait_for_group() => pgid,
    };
    // The call future is gone: the group must not outlive it.
    gone(pgid).await;
    assert!(e.groups.live().is_empty());
}

#[tokio::test]
async fn cancel_kills_the_command_without_waiting_out_the_grace() {
    let e = env();
    let cancel = CancellationToken::new();
    let run = e.run("sleep 306", cancel.clone());
    tokio::pin!(run);
    let pgid = tokio::select! {
        _ = &mut run => panic!("sleep 306 returned"),
        pgid = e.wait_for_group() => pgid,
    };
    let t0 = Instant::now();
    cancel.cancel();
    let r = run.await;
    let took = t0.elapsed();
    assert!(r.output.contains("killed: cancelled"), "{}", r.output);
    assert!(!group_alive(pgid));
    // A command that dies on SIGTERM is not given the full 100 ms grace.
    assert!(took < Duration::from_millis(90), "{took:?}");
    assert!(e.groups.live().is_empty());
}
