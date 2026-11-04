//! `peek`: a read-only look at an index database (for `tautly doctor`).

mod common;

use std::time::{Duration, Instant};

use common::Fixture;
use rusqlite::{Connection, TransactionBehavior};
use tautly_index::peek;

#[test]
fn a_missing_database_is_none_and_nothing_is_created() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("index").join("x.db");
    assert_eq!(peek(&db).unwrap(), None);
    assert!(!db.exists());
    assert!(!db.parent().unwrap().exists(), "no directory is created");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_built_index_reports_its_files_without_being_written() {
    let f = Fixture::new(&[
        ("a.rs", "fn a() {}\n"),
        ("b.rs", "fn b() {}\n"),
        ("c.py", "def c():\n    pass\n"),
    ]);
    let index = f.open();
    index.refresh().await.unwrap();
    drop(index);

    let db = f.db();
    let before = std::fs::read(&db).unwrap();
    let modified = std::fs::metadata(&db).unwrap().modified().unwrap();
    let found = peek(&db).unwrap().expect("present");
    assert_eq!(found.files, 3);
    assert!(found.is_current());
    assert_eq!(
        std::fs::read(&db).unwrap(),
        before,
        "the database is unchanged"
    );
    assert_eq!(
        std::fs::metadata(&db).unwrap().modified().unwrap(),
        modified
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn peek_does_not_wait_for_a_writer() {
    let f = Fixture::new(&[("a.rs", "fn a() {}\n")]);
    let index = f.open();
    index.refresh().await.unwrap();

    // A session in the middle of a write transaction (WAL: readers are not blocked).
    let mut writer = Connection::open(f.db()).unwrap();
    let tx = writer
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    tx.execute("DELETE FROM files", []).unwrap();
    let start = Instant::now();
    let found = peek(&f.db()).unwrap().unwrap();
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "{:?}",
        start.elapsed()
    );
    assert_eq!(found.files, 1, "the last committed state");
    tx.rollback().unwrap();
}

#[test]
fn an_old_schema_is_reported_not_migrated() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("old.db");
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch(
        "CREATE TABLE schema_version (version INTEGER NOT NULL);
         INSERT INTO schema_version (version) VALUES (1);
         CREATE TABLE files (path TEXT NOT NULL UNIQUE);
         INSERT INTO files (path) VALUES ('a.rs'), ('b.rs');",
    )
    .unwrap();
    drop(conn);
    let found = peek(&db).unwrap().unwrap();
    assert_eq!((found.schema_version, found.files), (1, 2));
    assert!(!found.is_current());
    // Still version 1: nothing was migrated.
    let conn = Connection::open(&db).unwrap();
    let v: i64 = conn
        .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(v, 1);
}
