//! 100 MB of tool output (spec 9.1): the RSS stays bounded and the whole output is
//! spilled to disk. Its own binary, so the process-wide peak RSS is this test's alone.
//!
//! `cargo test --release -p tautly-perf --test big_output -- --ignored`

use serde_json::json;
use tautly_perf::{MB, peak_rss, report};
use tautly_provider::EditFormat;
use tautly_sandbox::LocalSandbox;
use tautly_tools::{BashTool, Tool, ToolCx};
use tempfile::TempDir;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "perf gate"]
async fn hundred_megabytes_of_output_spill_with_bounded_rss() {
    let dir = TempDir::new().unwrap();
    let sb = LocalSandbox::new(dir.path().to_path_buf()).unwrap();
    let cx = ToolCx::new(EditFormat::StrReplace, dir.path().join(".spill"));
    // Warm up what is set up on first use (runtime, spill directory).
    BashTool
        .run(json!({"command": "seq 1 20000"}), &sb, &cx)
        .await;
    let before = peak_rss();
    let r = BashTool
        .run(
            json!({"command": "head -c 100000000 /dev/zero | tr '\\0' a"}),
            &sb,
            &cx,
        )
        .await;
    let grown = peak_rss().saturating_sub(before);
    assert!(
        !r.is_error,
        "{}",
        &r.output[r.output.len().saturating_sub(300)..]
    );
    let path = r
        .output
        .lines()
        .find_map(|l| l.strip_prefix("[full output saved to "))
        .map(|p| p.trim_end_matches(']').to_string())
        .unwrap_or_else(|| panic!("no spill note: {}", &r.output[r.output.len() - 300..]));
    let spilled = std::fs::metadata(dir.path().join(&path)).unwrap().len();
    report(&format!(
        "big output: 100 MB command, peak RSS growth {:.1} MB, spill file {spilled} bytes",
        grown as f64 / MB as f64
    ));
    assert_eq!(spilled, 100_000_000);
    assert!(grown < 20 * MB, "peak RSS grew by {} MB", grown / MB);
}
