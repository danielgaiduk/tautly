mod common;

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

use common::{Fixture, open_at, paths, write_all};
use hx_index::{Candidates, Index};
use hx_sandbox::LocalSandbox;
use proptest::prelude::*;

fn files(c: Candidates) -> Vec<String> {
    match c {
        Candidates::Files(v) => v.iter().map(|p| p.to_str().unwrap().to_string()).collect(),
        Candidates::FullScan => panic!("expected narrowed candidates, got FullScan"),
    }
}

async fn ready(files: &[(&str, &str)]) -> (Fixture, Index) {
    let fx = Fixture::new(files);
    let idx = fx.open();
    idx.refresh().await.unwrap();
    (fx, idx)
}

#[tokio::test]
async fn grep_candidates_narrows_to_files_containing_literal() {
    let (_fx, idx) = ready(&[
        ("a.rs", "fn parse_config() {}\n"),
        ("b.rs", "fn other() { parse_config(); }\n"),
        ("c.txt", "nothing here\n"),
        ("d.md", "parse_conf only\n"),
    ])
    .await;
    assert_eq!(files(idx.grep_candidates("parse_config")), ["a.rs", "b.rs"]);
    assert_eq!(files(idx.grep_candidates("nothing")), ["c.txt"]);
    assert!(files(idx.grep_candidates("absent_literal")).is_empty());
    // Required literals are found inside a pattern, not only at its start.
    // `fn` is too short to look up; the literal after it is not.
    assert_eq!(
        files(idx.grep_candidates(r"fn\s+parse_config")),
        ["a.rs", "b.rs"]
    );
    assert_eq!(
        files(idx.grep_candidates(r"fn\s+parse_config\(\)")),
        ["a.rs", "b.rs"]
    );
    assert_eq!(files(idx.grep_candidates(r"nothing\s+here")), ["c.txt"]);
    assert_eq!(
        files(idx.grep_candidates(r".*parse_config")),
        ["a.rs", "b.rs"]
    );
}

#[tokio::test]
async fn grep_candidates_is_a_case_insensitive_superset() {
    let (_fx, idx) = ready(&[
        ("a.txt", "Hello World\n"),
        ("b.txt", "HELLO there\n"),
        ("c.txt", "bye\n"),
    ])
    .await;
    // Trigrams are folded: a case-sensitive pattern may over-select, never under-select.
    assert_eq!(files(idx.grep_candidates("Hello")), ["a.txt", "b.txt"]);
    assert_eq!(files(idx.grep_candidates("(?i)hello")), ["a.txt", "b.txt"]);
    assert_eq!(files(idx.grep_candidates("hello")), ["a.txt", "b.txt"]);
}

#[tokio::test]
async fn grep_candidates_alternation_unions_postings() {
    let (_fx, idx) = ready(&[
        ("a.rs", "foo\n"),
        ("b.rs", "barbaz\n"),
        ("c.rs", "foo and barbaz\n"),
        ("d.rs", "neither\n"),
        ("e.rs", "bar and baz apart\n"),
    ])
    .await;
    assert_eq!(
        files(idx.grep_candidates("foo|barbaz")),
        ["a.rs", "b.rs", "c.rs"]
    );
    assert_eq!(
        files(idx.grep_candidates("(foo|barbaz)")),
        ["a.rs", "b.rs", "c.rs"]
    );
    // A concatenation requires both parts.
    assert_eq!(files(idx.grep_candidates("foo.*barbaz")), ["c.rs"]);
}

#[tokio::test]
async fn short_literal_or_complex_pattern_falls_back_to_full_scan() {
    let (_fx, idx) = ready(&[("a.rs", "foo bar\n")]).await;
    for pat in [
        "ab", // shorter than a trigram
        "a",
        ".*",
        r"\w+",
        "foo|ab", // one alternative has no usable literal
        "[a-z]+",
        "(foo)?",
        "(unclosed", // parse error
        "",
    ] {
        assert_eq!(idx.grep_candidates(pat), Candidates::FullScan, "{pat:?}");
    }
    assert!(matches!(idx.grep_candidates("foo"), Candidates::Files(_)));
}

#[tokio::test]
async fn not_ready_index_falls_back_to_full_scan() {
    let fx = Fixture::new(&[("a.rs", "foo bar\n")]);
    let idx = fx.open();
    assert_eq!(idx.grep_candidates("foo bar"), Candidates::FullScan);
    idx.refresh().await.unwrap();
    assert_eq!(files(idx.grep_candidates("foo bar")), ["a.rs"]);
    // A new handle on a populated database is not ready until it has refreshed.
    let again = fx.open();
    assert_eq!(again.grep_candidates("foo bar"), Candidates::FullScan);
}

#[tokio::test]
async fn trigram_rows_follow_edits_deletes_and_refresh() {
    let (fx, idx) = ready(&[("a.rs", "fn alpha() {}\n"), ("b.rs", "fn beta() {}\n")]).await;
    assert_eq!(files(idx.grep_candidates("alpha")), ["a.rs"]);

    std::fs::write(fx.path("a.rs"), "fn gamma() {}\n").unwrap();
    idx.reindex_file(Path::new("a.rs")).await.unwrap();
    assert!(files(idx.grep_candidates("alpha")).is_empty());
    assert_eq!(files(idx.grep_candidates("gamma")), ["a.rs"]);

    std::fs::remove_file(fx.path("b.rs")).unwrap();
    idx.reindex_file(Path::new("b.rs")).await.unwrap();
    assert!(files(idx.grep_candidates("beta")).is_empty());

    write_all(fx.root.path(), &[("c.txt", "delta text\n")]);
    std::fs::remove_file(fx.path("a.rs")).unwrap();
    idx.refresh().await.unwrap();
    assert_eq!(files(idx.grep_candidates("delta")), ["c.txt"]);
    assert!(files(idx.grep_candidates("gamma")).is_empty());
    // No orphaned postings are left behind by the deletes.
    let conn = rusqlite::Connection::open(fx.db()).unwrap();
    let orphans: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM trigrams WHERE file_id NOT IN (SELECT rowid FROM files)",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(orphans, 0);
}

fn trigram_count(db: &Path, rel: &str) -> i64 {
    let conn = rusqlite::Connection::open(db).unwrap();
    conn.query_row(
        "SELECT COUNT(*) FROM trigrams WHERE file_id = (SELECT rowid FROM files WHERE path = ?1)",
        [rel],
        |r| r.get(0),
    )
    .unwrap()
}

#[tokio::test]
async fn binary_and_oversize_files_have_no_trigrams() {
    let fx = Fixture::new(&[("ok.txt", "plenty of text here\n")]);
    std::fs::write(fx.path("bin.dat"), b"needle \xff\xfe needle needle").unwrap();
    std::fs::write(
        fx.path("big.txt"),
        format!("needle {}", "x".repeat(1024 * 1024)),
    )
    .unwrap();
    let idx = fx.open();
    idx.refresh().await.unwrap();
    assert!(trigram_count(&fx.db(), "ok.txt") > 0);
    assert_eq!(trigram_count(&fx.db(), "bin.dat"), 0);
    assert_eq!(trigram_count(&fx.db(), "big.txt"), 0);
    // They are still tracked, and a narrowed search never silently skips them: the
    // index cannot vouch for their content, so they stay candidates.
    assert_eq!(files(idx.grep_candidates("needle")), ["big.txt", "bin.dat"]);
    assert_eq!(
        files(idx.grep_candidates("plenty")),
        ["big.txt", "bin.dat", "ok.txt"]
    );
}

fn v1_database(db: &Path, files: &[(&str, &str)]) {
    let conn = rusqlite::Connection::open(db).unwrap();
    conn.execute_batch(
        "CREATE TABLE schema_version (version INTEGER NOT NULL);
         INSERT INTO schema_version VALUES (1);
         CREATE TABLE files (path TEXT PRIMARY KEY, hash TEXT NOT NULL, mtime INTEGER NOT NULL,
                             size INTEGER NOT NULL, lang TEXT NOT NULL);
         CREATE TABLE symbols (id INTEGER PRIMARY KEY, file TEXT NOT NULL REFERENCES files(path) ON DELETE CASCADE,
                               name TEXT NOT NULL, kind TEXT NOT NULL, parent TEXT, start_line INTEGER NOT NULL,
                               end_line INTEGER NOT NULL, signature TEXT NOT NULL, hash TEXT NOT NULL);
         CREATE INDEX symbols_file ON symbols(file);
         CREATE INDEX symbols_name ON symbols(name);
         CREATE TABLE refs (file TEXT NOT NULL REFERENCES files(path) ON DELETE CASCADE,
                            line INTEGER NOT NULL, name TEXT NOT NULL);
         CREATE INDEX refs_file ON refs(file);
         CREATE INDEX refs_name ON refs(name);
         CREATE TABLE ref_lines (file TEXT NOT NULL REFERENCES files(path) ON DELETE CASCADE,
                                 line INTEGER NOT NULL, text TEXT NOT NULL, PRIMARY KEY (file, line)) WITHOUT ROWID;",
    )
    .unwrap();
    // Rows as a v1 refresh left them: content hash and size, no trigrams.
    for (rel, content) in files {
        let hash = blake3::hash(content.as_bytes()).to_hex().to_string();
        conn.execute(
            "INSERT INTO files VALUES (?1, ?2, 0, ?3, 'rust')",
            rusqlite::params![rel, hash, content.len() as i64],
        )
        .unwrap();
    }
}

#[tokio::test]
async fn v1_database_migrates_and_is_fully_reparsed() {
    let root = tempfile::tempdir().unwrap();
    let dbs = tempfile::tempdir().unwrap();
    let content = [("a.rs", "fn needle_fn() {}\n"), ("b.rs", "fn other() {}\n")];
    write_all(root.path(), &content);
    let db = dbs.path().join("v1.db");
    // The v1 rows carry the files' real hashes: a refresh that trusts hashes would keep
    // them without trigrams, so the extractor-version change must force a reparse.
    v1_database(&db, &content);

    let sb = Arc::new(LocalSandbox::new(root.path().to_path_buf()).unwrap());
    let idx = Index::open(&db, sb).unwrap();
    // The v1 tree is available straight away.
    assert_eq!(idx.tree().len(), 2);

    let conn = rusqlite::Connection::open(&db).unwrap();
    let version: i64 = conn
        .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, 2);
    let tables: BTreeSet<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type IN ('table','index')")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    for name in ["trigrams", "meta", "refs_name_file_line"] {
        assert!(tables.contains(name), "{name} missing from {tables:?}");
    }
    assert_eq!(idx.grep_candidates("needle_fn"), Candidates::FullScan);

    let stats = idx.refresh().await.unwrap();
    assert_eq!(
        stats.parsed, 2,
        "every file is reparsed after the migration"
    );
    assert_eq!(files(idx.grep_candidates("needle_fn")), ["a.rs"]);
    assert_eq!(idx.find_symbol("needle_fn", None).unwrap().len(), 1);

    // The reparse is a one-time cost.
    let again = idx.refresh().await.unwrap();
    assert_eq!((again.parsed, again.unchanged), (0, 2));
}

#[tokio::test]
async fn changed_extractor_version_forces_a_full_reparse() {
    let (fx, idx) = ready(&[("a.rs", "fn a() {}\n"), ("b.rs", "fn b() {}\n")]).await;
    assert_eq!(idx.refresh().await.unwrap().parsed, 0);
    drop(idx);
    let conn = rusqlite::Connection::open(fx.db()).unwrap();
    let n = conn
        .execute(
            "UPDATE meta SET value = 'older' WHERE key = 'extractor_version'",
            [],
        )
        .unwrap();
    assert_eq!(n, 1, "the extractor version is recorded in meta");
    drop(conn);
    let idx = fx.open();
    assert_eq!(idx.refresh().await.unwrap().parsed, 2);
    assert_eq!(idx.refresh().await.unwrap().parsed, 0);
}

// ----------------------------------------------------------------- proptest

const WORDS: [&str; 7] = ["abc", "bca", "cab", "aab", "ABC", "bab", "éab"];

/// Files are made of a few words, so that random patterns built from the same words
/// regularly have true matches and the index narrows (rather than always falling back).
fn arb_content() -> impl Strategy<Value = String> {
    proptest::collection::vec(
        (
            proptest::sample::select(WORDS.to_vec()),
            prop_oneof![Just(" "), Just("\n"), Just(""), Just("_")],
        ),
        0..12,
    )
    .prop_map(|v| v.into_iter().map(|(w, sep)| format!("{w}{sep}")).collect())
}

/// Regex fragments over the same vocabulary: literals, classes, optional and repeated
/// pieces, groups and alternations, optionally case-insensitive.
fn arb_pattern() -> impl Strategy<Value = String> {
    let atom = prop_oneof![
        proptest::sample::select(WORDS.to_vec()).prop_map(str::to_string),
        proptest::string::string_regex("[abcAB]{1,5}").unwrap(),
        Just("[ab]bc".to_string()),
        Just("[^c]".to_string()),
        Just(".".to_string()),
        Just("\\w+".to_string()),
        Just("a?".to_string()),
        Just("b*".to_string()),
        Just("(abc|bca)".to_string()),
        Just("(ab|ba)".to_string()),
        Just("(cab)+".to_string()),
        Just("\\s".to_string()),
        Just("_".to_string()),
        Just("^".to_string()),
    ];
    let seq = proptest::collection::vec(atom, 1..4).prop_map(|v| v.concat());
    let alt = proptest::collection::vec(seq, 1..4).prop_map(|v| v.join("|"));
    (
        alt,
        prop_oneof![Just(""), Just("(?i)"), Just("(?s)"), Just("(?m)")],
    )
        .prop_map(|(p, flags)| format!("{flags}{p}"))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    #[test]
    fn candidates_never_miss_a_true_match(
        contents in proptest::collection::vec(arb_content(), 1..8),
        patterns in proptest::collection::vec(arb_pattern(), 1..8),
    ) {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let root = tempfile::tempdir().unwrap();
        let db_dir = tempfile::tempdir().unwrap();
        let mut named = Vec::new();
        for (i, c) in contents.iter().enumerate() {
            let rel = format!("dir{}/f{i}.txt", i % 3);
            let p = root.path().join(&rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, c).unwrap();
            named.push((rel, c.clone()));
        }
        let idx = open_at(root.path(), &db_dir.path().join("i.db"));
        rt.block_on(idx.refresh()).unwrap();
        for pat in &patterns {
            let re = regex::Regex::new(pat).unwrap();
            // The full scan is the oracle.
            let truth: BTreeSet<&str> = named
                .iter()
                .filter(|(_, c)| re.is_match(c))
                .map(|(r, _)| r.as_str())
                .collect();
            match idx.grep_candidates(pat) {
                Candidates::FullScan => {}
                Candidates::Files(got) => {
                    let got: BTreeSet<&str> = paths(&got).into_iter().collect();
                    prop_assert!(
                        truth.is_subset(&got),
                        "pattern {pat:?}: missed {:?}", truth.difference(&got).collect::<Vec<_>>()
                    );
                }
            }
        }
    }
}

/// Scale smoke test, run with `cargo test -p hx-index --release -- --ignored --nocapture`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "scale measurement"]
async fn scale_10k_files_build_and_selective_search() {
    let files: usize = std::env::var("HX_SCALE_FILES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10_000);
    let root = tempfile::tempdir().unwrap();
    let db_dir = tempfile::tempdir().unwrap();
    let mut seed = 0x2545_f491_4f6c_dd1du64;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let vocab: Vec<String> = (0..3000)
        .map(|i| format!("w{:x}_{}", next() % 100_000, i % 97))
        .collect();
    for i in 0..files {
        let dir = root.path().join(format!("d{}/e{}", i % 50, i % 7));
        std::fs::create_dir_all(&dir).unwrap();
        let mut body = String::new();
        for _ in 0..120 {
            let w = &vocab[(next() % vocab.len() as u64) as usize];
            body.push_str(&format!("fn {w}() {{ call_{w}(); }}\n"));
        }
        if i == files / 2 {
            body.push_str("// the_one_needle_marker\n");
        }
        std::fs::write(dir.join(format!("f{i}.rs")), body).unwrap();
    }
    let idx = open_at(root.path(), &db_dir.path().join("i.db"));
    let t = std::time::Instant::now();
    idx.refresh().await.unwrap();
    println!("refresh of {files} files: {:?}", t.elapsed());
    let t = std::time::Instant::now();
    let found = idx.grep_candidates("the_one_needle_marker");
    println!(
        "selective search: {:?} -> {}",
        t.elapsed(),
        files_len(&found)
    );
    let t = std::time::Instant::now();
    let found = idx.grep_candidates("fn w1");
    println!("common search: {:?} -> {}", t.elapsed(), files_len(&found));
    let t = std::time::Instant::now();
    let tree = idx.tree();
    let n = tree.glob("d3/**/*.rs", Path::new(".")).len();
    println!("glob: {:?} -> {n}", t.elapsed());
}

fn files_len(c: &Candidates) -> usize {
    match c {
        Candidates::Files(v) => v.len(),
        Candidates::FullScan => usize::MAX,
    }
}
