//! When the index can be trusted (checkpoint 2: I1, I4, I6, I8): refreshes never undo
//! writes made during their scan and never overlap, the watcher's liveness is visible,
//! the post-bash freshness check is a cookie barrier while the watcher runs, and a
//! background refresh leaves no file without postings.

mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{Fixture, ProbeSandbox, eventually, paths, write_all};
use hx_index::{Candidates, Index};

fn has_candidate(idx: &Index, pattern: &str, rel: &str) -> bool {
    match idx.grep_candidates(pattern, false) {
        Candidates::Files(f) => paths(&f).contains(&rel),
        Candidates::FullScan => true,
    }
}

async fn built(fx: &Fixture) -> Arc<Index> {
    let idx = Arc::new(fx.open());
    idx.refresh().await.unwrap();
    idx.build_trigrams().await.unwrap();
    idx
}

fn sample() -> Fixture {
    Fixture::new(&[
        ("src/a.rs", "fn alpha() {}\n"),
        ("src/b.rs", "fn beta() {}\n"),
        ("docs/readme.md", "# docs\n"),
    ])
}

// ------------------------------------------------------------------ I1

/// The reviewer's [bash, write new file] race, made deterministic: hx writes and
/// re-indexes a new file after a refresh's scan passed its directory.
#[tokio::test(flavor = "multi_thread")]
async fn a_refresh_never_drops_a_file_written_during_its_scan() {
    let fx = sample();
    let idx = built(&fx).await;
    let mut hold = idx.hold_next_refresh_after_scan();
    let bg = tokio::spawn({
        let idx = Arc::clone(&idx);
        async move { idx.refresh().await }
    });
    hold.reached().await;
    write_all(
        fx.root.path(),
        &[("src/new.rs", "fn written_mid_scan() {}\n")],
    );
    idx.reindex_file(Path::new("src/new.rs")).await.unwrap();
    hold.release();
    bg.await.unwrap().unwrap();
    assert!(
        idx.tree().get(Path::new("src/new.rs")).is_some(),
        "new file lost from the tree"
    );
    assert!(has_candidate(&idx, "written_mid_scan", "src/new.rs"));
    assert_eq!(
        idx.outline(Path::new("src/new.rs")).unwrap()[0].name,
        "written_mid_scan"
    );
}

/// A file edited (and re-indexed) during a refresh's scan keeps the row the edit wrote,
/// not one built from the scan's older stat.
#[tokio::test(flavor = "multi_thread")]
async fn a_refresh_never_overwrites_a_row_written_during_its_scan() {
    let fx = sample();
    let idx = built(&fx).await;
    let mut hold = idx.hold_next_refresh_after_scan();
    let bg = tokio::spawn({
        let idx = Arc::clone(&idx);
        async move { idx.refresh().await }
    });
    hold.reached().await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    std::fs::write(fx.path("src/a.rs"), "fn alpha_edited() {}\n").unwrap();
    idx.reindex_file(Path::new("src/a.rs")).await.unwrap();
    hold.release();
    bg.await.unwrap().unwrap();
    let entry = idx.tree().get(Path::new("src/a.rs")).unwrap();
    let disk = std::fs::metadata(fx.path("src/a.rs")).unwrap();
    assert_eq!(entry.mtime, disk.modified().unwrap());
    assert_eq!(entry.size, disk.len());
}

#[tokio::test(flavor = "multi_thread")]
async fn refreshes_of_one_index_never_overlap() {
    let fx = sample();
    let idx = built(&fx).await;
    let mut hold = idx.hold_next_refresh_after_scan();
    let first = tokio::spawn({
        let idx = Arc::clone(&idx);
        async move { idx.refresh().await }
    });
    hold.reached().await;
    let second = tokio::spawn({
        let idx = Arc::clone(&idx);
        async move { idx.refresh().await }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !second.is_finished(),
        "a second refresh ran while the first was mid-way"
    );
    hold.release();
    first.await.unwrap().unwrap();
    second.await.unwrap().unwrap();
}

// ------------------------------------------------------------------ I4

#[tokio::test(flavor = "multi_thread")]
async fn watching_follows_the_watcher_lifecycle() {
    let fx = sample();
    let idx = built(&fx).await;
    assert!(!idx.watching());
    let handle = Arc::clone(&idx).start_watcher().unwrap();
    assert!(idx.watching());
    drop(handle);
    assert!(!idx.watching());
    // A second watcher's handle governs it alone.
    let a = Arc::clone(&idx).start_watcher().unwrap();
    let b = Arc::clone(&idx).start_watcher().unwrap();
    drop(a);
    assert!(idx.watching());
    drop(b);
    assert!(!idx.watching());
}

// ------------------------------------------------------------------ I6

#[tokio::test]
async fn a_refresh_takes_stat_data_from_the_walk() {
    let fx = sample();
    let sb = ProbeSandbox::new(fx.root.path());
    let idx = Index::open(&fx.db(), sb.clone()).unwrap();
    idx.refresh().await.unwrap();
    let before = sb.calls();
    idx.refresh().await.unwrap();
    assert_eq!(
        sb.calls(),
        before,
        "a no-op refresh stats nothing through the sandbox"
    );
    assert_eq!(idx.tree().len(), 3);
}

/// With the watcher alive, the post-bash check is a cookie barrier: staleness clears once
/// the watcher has caught up, without a full refresh, and what the command wrote is in.
#[tokio::test(flavor = "multi_thread")]
async fn a_live_watcher_clears_staleness_with_a_cookie_not_a_refresh() {
    let fx = sample();
    let idx = built(&fx).await;
    let _watch = Arc::clone(&idx).start_watcher().unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let refreshes = idx.refresh_count();
    // What a shell command did, then the bash tool's drop guard.
    write_all(
        fx.root.path(),
        &[("src/shell.rs", "fn from_the_shell() {}\n")],
    );
    idx.mark_stale();
    let started = Instant::now();
    let handle = idx.refresh_stale_in_background().expect("spawned");
    handle.await.unwrap();
    assert!(!idx.is_stale());
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(idx.refresh_count(), refreshes, "no full refresh");
    assert!(idx.tree().get(Path::new("src/shell.rs")).is_some());
    assert!(has_candidate(&idx, "from_the_shell", "src/shell.rs"));
    // The cookie is gone again.
    assert!(
        eventually(Duration::from_secs(2), || {
            std::fs::read_dir(fx.path(".hx/tmp"))
                .map(|d| {
                    d.flatten()
                        .all(|e| !e.file_name().to_string_lossy().starts_with("cookie-"))
                })
                .unwrap_or(true)
        })
        .await
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn without_a_watcher_the_post_bash_check_is_a_refresh() {
    let fx = sample();
    let idx = built(&fx).await;
    let refreshes = idx.refresh_count();
    idx.mark_stale();
    idx.refresh_stale_in_background()
        .expect("spawned")
        .await
        .unwrap();
    assert!(!idx.is_stale());
    assert_eq!(idx.refresh_count(), refreshes + 1);
}

/// Files a background refresh found changed get their postings built afterwards, so they
/// stop being candidates for every search.
#[tokio::test(flavor = "multi_thread")]
async fn a_background_refresh_builds_postings_for_what_it_changed() {
    let fx = sample();
    let idx = built(&fx).await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    std::fs::write(fx.path("src/b.rs"), "fn beta_changed() {}\n").unwrap();
    idx.mark_stale();
    idx.refresh_stale_in_background()
        .expect("spawned")
        .await
        .unwrap();
    assert!(!has_candidate(&idx, "unrelated_token", "src/b.rs"));
    assert!(has_candidate(&idx, "beta_changed", "src/b.rs"));
}

// ------------------------------------------------------------------ I8

#[tokio::test(flavor = "multi_thread")]
async fn wait_ready_waits_for_the_first_refresh() {
    let fx = sample();
    let idx = Arc::new(fx.open());
    assert!(!idx.wait_ready(Duration::from_millis(50)).await);
    let _bg = Arc::clone(&idx).spawn_background_refresh();
    assert!(idx.wait_ready(Duration::from_secs(10)).await);
    assert!(idx.is_ready());
    assert!(idx.wait_ready(Duration::ZERO).await);
}
