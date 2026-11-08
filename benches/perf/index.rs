//! Index gates (spec 7.1, 9.1): the first build of a 10,000-file repository, a single-file
//! reindex, and a reindex while the repo map's `top_referenced` query runs.
//!
//! `cargo test --release -p tautly-perf --test index -- --ignored --test-threads=1`

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tautly_index::Index;
use tautly_perf::gen_repo::{BIG_FILE, big_file, generate};
use tautly_perf::{median, ms, report};
use tautly_sandbox::LocalSandbox;
use tempfile::TempDir;

fn open(root: &Path, db: &Path) -> Arc<Index> {
    let sb = Arc::new(LocalSandbox::new(root.to_path_buf()).unwrap());
    Arc::new(Index::open(db, sb).unwrap())
}

/// A generated repository of `files` files and its built index.
async fn built(files: usize) -> (TempDir, TempDir, Arc<Index>) {
    let (root, db) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    generate(root.path(), files, 20);
    let idx = open(root.path(), &db.path().join("i.db"));
    idx.refresh().await.unwrap();
    (root, db, idx)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "perf gate"]
async fn first_build_of_10k_files_and_single_file_reindex() {
    let (root, db) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    generate(root.path(), 10_000, 20);
    let idx = open(root.path(), &db.path().join("i.db"));
    let t = Instant::now();
    idx.refresh().await.unwrap();
    let build = t.elapsed();
    report(&format!(
        "index: first refresh of 10,000 files {}",
        ms(build)
    ));
    assert!(build < Duration::from_secs(10), "{build:?}");

    let mut times = Vec::new();
    for n in 1..=20 {
        std::fs::write(root.path().join(BIG_FILE), big_file(n)).unwrap();
        let t = Instant::now();
        idx.reindex_file(Path::new(BIG_FILE)).await.unwrap();
        times.push(t.elapsed());
    }
    let reindex = median(times);
    report(&format!(
        "index: reindex of a 500-line file {} (median of 20)",
        ms(reindex)
    ));
    assert!(reindex < Duration::from_millis(50), "{reindex:?}");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "perf gate"]
async fn reindex_is_not_blocked_by_top_referenced() {
    let (root, _db, idx) = built(20_000).await;
    let t = Instant::now();
    idx.top_referenced(20).unwrap();
    report(&format!(
        "index: top_referenced(20) on 20,000 files {}",
        ms(t.elapsed())
    ));

    let mut times = Vec::new();
    let mut overlapped = 0;
    for n in 1..=20 {
        std::fs::write(root.path().join(BIG_FILE), big_file(n)).unwrap();
        let running = Arc::new(AtomicBool::new(true));
        let query = {
            let (idx, running) = (Arc::clone(&idx), Arc::clone(&running));
            tokio::task::spawn_blocking(move || {
                let r = idx.top_referenced(20);
                running.store(false, Ordering::SeqCst);
                r
            })
        };
        // Let the query get going first.
        tokio::time::sleep(Duration::from_millis(2)).await;
        let during = running.load(Ordering::SeqCst);
        let t = Instant::now();
        idx.reindex_file(Path::new(BIG_FILE)).await.unwrap();
        let took = t.elapsed();
        query.await.unwrap().unwrap();
        overlapped += usize::from(during);
        times.push(took);
    }
    // A query that ends before the reindex starts (overlapped < 20) cannot block it anyway.
    let reindex = median(times);
    report(&format!(
        "index: reindex started during top_referenced {} (median of 20, {overlapped} overlapped)",
        ms(reindex)
    ));
    assert!(reindex < Duration::from_millis(50), "{reindex:?}");
}
