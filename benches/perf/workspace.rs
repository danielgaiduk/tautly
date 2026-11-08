//! Workspace gates on a generated 100,000-file repository (spec 7.10, 9.1): a selective
//! `grep` through the trigram candidates, `glob` from the live tree, the memory of an
//! opened index and its tree, and a no-op post-bash freshness check with a live watcher.
//!
//! `cargo test --release -p tautly-perf --test workspace -- --ignored --test-threads=1`

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;
use tautly_index::Index;
use tautly_perf::gen_repo::{NEEDLE, generate};
use tautly_perf::{MB, median, ms, report, rss};
use tautly_provider::EditFormat;
use tautly_sandbox::LocalSandbox;
use tautly_tools::{GlobTool, GrepTool, Tool, ToolCx};
use tempfile::TempDir;

const FILES: usize = 100_000;
/// Set (to the database path, with the root in `TAUTLY_PERF_PROBE_ROOT`) when this binary
/// is re-run as the memory probe.
const PROBE_DB: &str = "TAUTLY_PERF_PROBE_DB";
const PROBE_ROOT: &str = "TAUTLY_PERF_PROBE_ROOT";

fn open(root: &Path, db: &Path) -> Index {
    let sb = Arc::new(LocalSandbox::new(root.to_path_buf()).unwrap());
    Index::open(db, sb).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "perf gate"]
async fn hundred_thousand_files() {
    let (root, db_dir) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    // Small files: the gates are about the file count.
    generate(root.path(), FILES, 2);
    let db = db_dir.path().join("i.db");
    let idx = Arc::new(open(root.path(), &db));
    let t = Instant::now();
    idx.refresh().await.unwrap();
    idx.build_trigrams().await.unwrap();
    report(&format!(
        "workspace: index build of {FILES} files {}",
        ms(t.elapsed())
    ));
    let _watch = Arc::clone(&idx).start_watcher().unwrap();
    let sb = LocalSandbox::new(root.path().to_path_buf()).unwrap();
    let cx = ToolCx::new(EditFormat::StrReplace, root.path().join(".spill"))
        .with_index(Arc::clone(&idx));

    let mut times = Vec::new();
    for _ in 0..20 {
        let t = Instant::now();
        let r = GrepTool.run(json!({ "pattern": NEEDLE }), &sb, &cx).await;
        times.push(t.elapsed());
        assert!(r.output.contains(NEEDLE), "{}", r.output);
    }
    let grep = median(times);
    report(&format!(
        "workspace: selective grep on {FILES} files {} (median of 20)",
        ms(grep)
    ));
    assert!(grep < Duration::from_millis(50), "{grep:?}");

    let mut times = Vec::new();
    for _ in 0..20 {
        let t = Instant::now();
        let r = GlobTool
            .run(json!({ "pattern": "pkg7/**/*.rs" }), &sb, &cx)
            .await;
        times.push(t.elapsed());
        assert!(!r.is_error && r.output.contains(".rs"), "{}", r.output);
    }
    let glob = median(times);
    report(&format!(
        "workspace: glob from the tree {} (median of 20)",
        ms(glob)
    ));
    assert!(glob < Duration::from_millis(5), "{glob:?}");

    // A no-op post-bash freshness check with the watcher running: a cookie, no refresh.
    let refreshes = idx.refresh_count();
    let mut times = Vec::new();
    for _ in 0..10 {
        idx.mark_stale();
        let t = Instant::now();
        idx.refresh_stale_in_background().unwrap().await.unwrap();
        times.push(t.elapsed());
        assert!(!idx.is_stale());
    }
    let check = median(times);
    report(&format!(
        "workspace: no-op freshness check {} (median of 10)",
        ms(check)
    ));
    assert_eq!(
        idx.refresh_count(),
        refreshes,
        "the check refreshed instead of a cookie"
    );
    assert!(check < Duration::from_millis(50), "{check:?}");

    // The memory an opened index and its tree take, measured in a fresh process.
    drop(_watch);
    drop(cx);
    drop(idx);
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["open_probe", "--exact", "--ignored", "--nocapture"])
        .env(PROBE_DB, &db)
        .env(PROBE_ROOT, root.path())
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{text}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let grown: u64 = text
        .lines()
        .find_map(|l| l.strip_prefix("open grew RSS by "))
        .and_then(|n| n.trim().parse().ok())
        .unwrap_or_else(|| panic!("no probe result in {text}"));
    report(&format!(
        "workspace: Index::open with the tree of {FILES} files grew RSS by {:.1} MB ({} B/file)",
        grown as f64 / MB as f64,
        grown / FILES as u64
    ));
    // Spec 7.10 budgets about 100 B per file; this leaves twice that.
    assert!(grown < 20 * MB, "{grown}");
}

/// Run as a child process by [`hundred_thousand_files`]: opens the index it names and
/// prints how much the open grew this process's RSS.
#[test]
#[ignore = "perf gate helper"]
fn open_probe() {
    let (Ok(db), Ok(root)) = (std::env::var(PROBE_DB), std::env::var(PROBE_ROOT)) else {
        return;
    };
    let before = rss();
    let idx = open(Path::new(&root), Path::new(&db));
    let after = rss();
    assert!(idx.tree().len() >= FILES);
    println!("open grew RSS by {}", after.saturating_sub(before));
}
