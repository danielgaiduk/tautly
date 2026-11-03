//! Checkpoint fix I1: bash output memory stays bounded and reader tasks/pipes are
//! released when the call returns. Its own test binary, so process-wide peak RSS
//! and fd counts are not disturbed by other tests; the tests here run one at a time.

use serde_json::json;
use tautly_provider::EditFormat;
use tautly_sandbox::LocalSandbox;
use tautly_tools::{BashTool, Tool, ToolCx, ToolResult};
use tempfile::TempDir;

static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn sandbox() -> (TempDir, LocalSandbox, ToolCx) {
    let dir = TempDir::new().unwrap();
    let sb = LocalSandbox::new(dir.path().to_path_buf()).unwrap();
    let cx = ToolCx::new(EditFormat::StrReplace, dir.path().join(".spill"));
    (dir, sb, cx)
}

async fn run(sb: &LocalSandbox, cx: &ToolCx, cmd: &str) -> ToolResult {
    BashTool.run(json!({ "command": cmd }), sb, cx).await
}

/// Peak resident set size of this process so far, in bytes.
fn peak_rss() -> u64 {
    // SAFETY: getrusage only writes the struct we pass.
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    assert_eq!(unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) }, 0);
    let max = u64::try_from(ru.ru_maxrss).unwrap();
    if cfg!(target_os = "macos") {
        max
    } else {
        max * 1024
    }
}

fn open_fds() -> usize {
    // SAFETY: F_GETFD on an arbitrary descriptor number has no side effects.
    (0..4096)
        .filter(|&fd| unsafe { libc::fcntl(fd, libc::F_GETFD) } != -1)
        .count()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn newline_free_flood_keeps_rss_bounded() {
    let _serial = SERIAL.lock().await;
    let (_dir, sb, cx) = sandbox();
    let before = peak_rss();
    let r = run(&sb, &cx, "head -c 300000000 /dev/zero | tr '\\0' a").await;
    let grown = peak_rss().saturating_sub(before);
    eprintln!("peak RSS growth: {} MB", grown / (1024 * 1024));
    assert!(
        !r.is_error,
        "{}",
        &r.output[r.output.len().saturating_sub(200)..]
    );
    assert!(
        r.output.contains("[full output saved to"),
        "spill note missing"
    );
    assert!(r.output.len() < 40 * 1024, "{}", r.output.len());
    assert!(
        grown < 50 * 1024 * 1024,
        "peak RSS grew by {} MB",
        grown / (1024 * 1024)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn readers_and_pipes_are_released_when_a_background_child_holds_them() {
    let _serial = SERIAL.lock().await;
    let (_dir, sb, cx) = sandbox();
    // Warm up anything lazily opened on first use (runtime, spill dir, ...).
    run(&sb, &cx, "true").await;
    let before = open_fds();
    let r = run(&sb, &cx, "sleep 2 & echo started").await;
    assert!(r.output.contains("started"), "{}", r.output);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let after = open_fds();
    assert!(after <= before, "fds before {before}, after {after}");
}
