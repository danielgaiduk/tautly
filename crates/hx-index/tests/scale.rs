//! Scale measurements, run in release mode:
//! `cargo test -p hx-index --release --test scale -- --ignored --nocapture`
//! `HX_SCALE_FILES` (default 10000) and `HX_SCALE_FNS` (default 120 functions per
//! file, about 5.5 KB) size the synthetic corpus.

mod common;

use std::path::Path;
use std::time::Instant;

use common::{open_at, synthetic_corpus};
use hx_index::Candidates;

fn env(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "scale measurement"]
async fn scale_build_phases_and_selective_search() {
    let files = env("HX_SCALE_FILES", 10_000);
    let root = tempfile::tempdir().unwrap();
    let db_dir = tempfile::tempdir().unwrap();
    synthetic_corpus(root.path(), files, env("HX_SCALE_FNS", 120));
    let idx = open_at(root.path(), &db_dir.path().join("i.db"));

    let t = Instant::now();
    idx.refresh().await.unwrap();
    println!(
        "phase 1, symbols and refs ({files} files): {:?}",
        t.elapsed()
    );
    assert!(idx.is_ready() && !idx.trigrams_ready());

    let t = Instant::now();
    idx.build_trigrams().await.unwrap();
    println!("phase 2, trigram postings: {:?}", t.elapsed());
    assert!(idx.trigrams_ready());

    let count = |c: &Candidates| match c {
        Candidates::Files(v) => v.len(),
        Candidates::FullScan => usize::MAX,
    };
    let t = Instant::now();
    let found = idx.grep_candidates("the_one_needle_marker");
    println!("selective search: {:?} -> {}", t.elapsed(), count(&found));
    assert_eq!(count(&found), 1);
    let t = Instant::now();
    let found = idx.grep_candidates("fn w1");
    println!("common search: {:?} -> {}", t.elapsed(), count(&found));
    let t = Instant::now();
    let n = idx.tree().glob("d3/**/*.rs", Path::new(".")).len();
    println!("glob: {:?} -> {n}", t.elapsed());
}
