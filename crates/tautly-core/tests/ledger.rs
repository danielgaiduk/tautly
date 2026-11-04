//! The cost ledger (spec 4.6): a SQLite file shared by every session and process.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tautly_core::{Ledger, Totals};
use tautly_provider::{ModelId, RawUsage};

fn usage(input: u64, output: u64, cache_read: u64, cache_write: u64) -> RawUsage {
    RawUsage {
        input,
        output,
        cache_read,
        cache_write,
    }
}

#[test]
fn ledger_two_processes_concurrent_writes() {
    let dir = tempfile::tempdir().unwrap();
    let path = Arc::new(dir.path().join("nested").join("ledger.sqlite"));
    let model = ModelId::from("mock/test");
    let writers: Vec<_> = (0..2)
        .map(|w| {
            let path = Arc::clone(&path);
            let model = model.clone();
            std::thread::spawn(move || {
                // Its own handle (its own connection), as a second process would have.
                let ledger = Ledger::open(&path).unwrap();
                for i in 0..500u64 {
                    ledger
                        .record(&format!("s{w}"), &model, &usage(i, 1, 0, 0), Some(0.001))
                        .unwrap();
                }
            })
        })
        .collect();
    for w in writers {
        w.join().unwrap();
    }
    let t = Ledger::open(&path).unwrap().totals(UNIX_EPOCH).unwrap();
    assert_eq!(t.requests, 1000);
    assert_eq!(t.output, 1000);
    assert_eq!(t.input, 2 * (0..500u64).sum::<u64>());
    assert!((t.usd - 1.0).abs() < 1e-9, "{}", t.usd);
    assert_eq!(t.unpriced, 0);
    // WAL mode (kept in the file): readers never block the writers.
    let conn = rusqlite::Connection::open(path.as_path()).unwrap();
    let mode: String = conn
        .pragma_query_value(None, "journal_mode", |r| r.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
}

#[test]
fn totals_count_unpriced_rows_apart_and_filter_by_time() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite");
    let ledger = Ledger::open(&path).unwrap();
    let model = ModelId::from("mock/test");
    ledger
        .record("s1", &model, &usage(100, 10, 1000, 50), Some(0.25))
        .unwrap();
    ledger
        .record("s1", &model, &usage(7, 3, 0, 0), None)
        .unwrap();
    let all = ledger.totals(UNIX_EPOCH).unwrap();
    assert_eq!(
        all,
        Totals {
            requests: 2,
            input: 107,
            output: 13,
            cache_read: 1000,
            cache_write: 50,
            usd: 0.25,
            unpriced: 1,
            estimated: 0,
        }
    );
    assert_eq!(all.tokens(), 1170);
    let later = SystemTime::now() + Duration::from_secs(3600);
    assert_eq!(ledger.totals(later).unwrap(), Totals::default());

    // The rows outlive the handle.
    drop(ledger);
    let again = Ledger::open(&path).unwrap();
    assert_eq!(again.totals(UNIX_EPOCH).unwrap().requests, 2);
}

#[test]
fn opening_a_path_that_is_not_a_database_fails() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite");
    std::fs::write(
        &path,
        "not a database, just text that is long enough to matter",
    )
    .unwrap();
    assert!(Ledger::open(&path).is_err());
}

#[test]
fn estimated_rows_are_counted_apart() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(&dir.path().join("ledger.sqlite")).unwrap();
    let model = ModelId::from("mock/test");
    ledger
        .record_estimated("s1", &model, &usage(900, 40, 0, 0), Some(0.01))
        .unwrap();
    ledger
        .record("s1", &model, &usage(100, 10, 0, 0), Some(0.002))
        .unwrap();
    let t = ledger.totals(UNIX_EPOCH).unwrap();
    assert_eq!((t.requests, t.estimated, t.tokens()), (2, 1, 1050));
}

#[test]
fn session_totals_count_one_sessions_rows_estimated_ones_included() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(&dir.path().join("ledger.sqlite")).unwrap();
    let model = ModelId::from("mock/test");
    ledger
        .record("s1", &model, &usage(100, 10, 5, 0), Some(0.25))
        .unwrap();
    ledger
        .record_estimated("s1", &model, &usage(900, 40, 0, 0), None)
        .unwrap();
    ledger
        .record("s2", &model, &usage(7, 7, 7, 7), Some(1.0))
        .unwrap();
    let t = ledger.session_totals("s1").unwrap();
    assert_eq!(
        (t.requests, t.estimated, t.unpriced, t.tokens()),
        (2, 1, 1, 1055)
    );
    assert!((t.usd - 0.25).abs() < 1e-12);
    assert_eq!(ledger.session_totals("none").unwrap(), Totals::default());
}

#[test]
fn a_version_1_ledger_gains_the_estimated_column_and_keeps_its_rows() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE usage (id INTEGER PRIMARY KEY, at_ms INTEGER NOT NULL, \
             session TEXT NOT NULL, model TEXT NOT NULL, input INTEGER NOT NULL, \
             output INTEGER NOT NULL, cache_read INTEGER NOT NULL, \
             cache_write INTEGER NOT NULL, cost_usd REAL); \
             INSERT INTO usage VALUES (1, 1, 's', 'm', 10, 2, 0, 0, 0.5); \
             PRAGMA user_version = 1;",
        )
        .unwrap();
    }
    let ledger = Ledger::open(&path).unwrap();
    ledger
        .record_estimated("s", &ModelId::from("m"), &usage(5, 1, 0, 0), None)
        .unwrap();
    let t = ledger.totals(UNIX_EPOCH).unwrap();
    assert_eq!((t.requests, t.estimated, t.input), (2, 1, 15));
}

/// Whether the ledger at `path` has an index on the session column (review C M10:
/// `session_totals` runs at every resume and per row of a session list).
fn has_session_index(path: &std::path::Path) -> bool {
    let conn = rusqlite::Connection::open(path).unwrap();
    let mut stmt = conn
        .prepare("SELECT sql FROM sqlite_master WHERE type = 'index' AND tbl_name = 'usage'")
        .unwrap();
    stmt.query_map([], |r| r.get::<_, Option<String>>(0))
        .unwrap()
        .filter_map(Result::unwrap)
        .any(|sql| sql.contains("(session"))
}

#[test]
fn session_totals_use_an_index_new_ledgers_and_migrated_ones_alike() {
    let dir = tempfile::tempdir().unwrap();
    let fresh = dir.path().join("fresh.sqlite");
    Ledger::open(&fresh).unwrap();
    assert!(has_session_index(&fresh));

    let old = dir.path().join("old.sqlite");
    {
        let conn = rusqlite::Connection::open(&old).unwrap();
        conn.execute_batch(
            "CREATE TABLE usage (id INTEGER PRIMARY KEY, at_ms INTEGER NOT NULL, \
             session TEXT NOT NULL, model TEXT NOT NULL, input INTEGER NOT NULL, \
             output INTEGER NOT NULL, cache_read INTEGER NOT NULL, \
             cache_write INTEGER NOT NULL, cost_usd REAL, \
             estimated INTEGER NOT NULL DEFAULT 0); \
             CREATE INDEX usage_at ON usage(at_ms); \
             INSERT INTO usage VALUES (1, 1, 's', 'm', 10, 2, 0, 0, 0.5, 0); \
             PRAGMA user_version = 2;",
        )
        .unwrap();
    }
    let ledger = Ledger::open(&old).unwrap();
    assert_eq!(ledger.session_totals("s").unwrap().input, 10);
    assert!(has_session_index(&old));
}

#[test]
fn a_v3_ledger_migrates_to_receipts_and_reopens() {
    use tautly_core::{Event, Savings};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v3.sqlite");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE usage (id INTEGER PRIMARY KEY, at_ms INTEGER NOT NULL, \
             session TEXT NOT NULL, model TEXT NOT NULL, input INTEGER NOT NULL, \
             output INTEGER NOT NULL, cache_read INTEGER NOT NULL, \
             cache_write INTEGER NOT NULL, cost_usd REAL, \
             estimated INTEGER NOT NULL DEFAULT 0); \
             CREATE INDEX usage_at ON usage(at_ms); \
             CREATE INDEX usage_session ON usage(session); \
             INSERT INTO usage VALUES (1, 1, 's', 'm', 10, 2, 0, 0, 0.5, 0); \
             PRAGMA user_version = 3;",
        )
        .unwrap();
    }
    let receipt = Event::TurnReceipt {
        turn: 1,
        model: ModelId::from("m"),
        tokens_in: 10,
        tokens_out: 2,
        cache_read: 0,
        cache_write: 0,
        side_tokens: 0,
        cost_usd: Some(0.5),
        wall_ms: 7,
        cache_hit_ratio: None,
        steps: 1,
        tool_calls: 0,
        saved: Savings::default(),
        estimated: false,
    };
    {
        let ledger = Ledger::open(&path).unwrap();
        assert_eq!(
            ledger.session_totals("s").unwrap().input,
            10,
            "old rows survive"
        );
        ledger
            .record_receipt("s", "p", dir.path(), &receipt)
            .unwrap();
    }
    let version: i64 = rusqlite::Connection::open(&path)
        .unwrap()
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_eq!(version, 4);
    // Reopening migrates nothing again and keeps both tables' rows.
    let ledger = Ledger::open(&path).unwrap();
    let rows = ledger.session_receipts("s").unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        (rows[0].turn, rows[0].wall_ms, rows[0].project.as_str()),
        (1, 7, "p")
    );
    assert_eq!(ledger.session_totals("s").unwrap().requests, 1);
}

#[test]
fn the_ledger_file_is_private() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite");
    let ledger = Ledger::open(&path).unwrap();
    ledger
        .record("s", &ModelId::from("m"), &usage(1, 1, 0, 0), None)
        .unwrap();
    for f in ["ledger.sqlite", "ledger.sqlite-wal"] {
        let mode = std::fs::metadata(dir.path().join(f))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "{f}: {mode:o}");
    }
}

#[test]
fn a_corrupt_ledger_is_moved_aside_and_recreated() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite");
    let junk = "this is not a database, just text that is long enough to matter";
    std::fs::write(&path, junk).unwrap();
    let (ledger, notes) = Ledger::open_recovering(&path).unwrap();
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(notes[0].contains("corrupt"), "{notes:?}");
    ledger
        .record("s", &ModelId::from("m"), &usage(1, 1, 0, 0), None)
        .unwrap();
    assert_eq!(ledger.totals(UNIX_EPOCH).unwrap().requests, 1);
    let moved: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n.starts_with("ledger.sqlite.corrupt-"))
        .collect();
    assert_eq!(moved.len(), 1, "{moved:?}");
    assert_eq!(
        std::fs::read_to_string(dir.path().join(&moved[0])).unwrap(),
        junk
    );
    // A healthy ledger opens with nothing to say.
    drop(ledger);
    assert!(Ledger::open_recovering(&path).unwrap().1.is_empty());
}

#[test]
fn a_ledger_from_a_newer_version_is_left_alone() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite");
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch("PRAGMA user_version = 99;")
        .unwrap();
    let err = Ledger::open_recovering(&path).err().expect("refused");
    assert!(err.to_string().contains("newer"), "{err}");
    // Not moved or changed.
    let v: i64 = rusqlite::Connection::open(&path)
        .unwrap()
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_eq!(v, 99);
}
