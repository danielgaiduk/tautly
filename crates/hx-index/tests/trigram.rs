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
    idx.build_trigrams().await.unwrap();
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
    assert_eq!(
        files(idx.grep_candidates("parse_config", false)),
        ["a.rs", "b.rs"]
    );
    assert_eq!(files(idx.grep_candidates("nothing", false)), ["c.txt"]);
    assert!(files(idx.grep_candidates("absent_literal", false)).is_empty());
    // Required literals are found inside a pattern, not only at its start.
    // `fn` is too short to look up; the literal after it is not.
    assert_eq!(
        files(idx.grep_candidates(r"fn\s+parse_config", false)),
        ["a.rs", "b.rs"]
    );
    assert_eq!(
        files(idx.grep_candidates(r"fn\s+parse_config\(\)", false)),
        ["a.rs", "b.rs"]
    );
    assert_eq!(
        files(idx.grep_candidates(r"nothing\s+here", false)),
        ["c.txt"]
    );
    assert_eq!(
        files(idx.grep_candidates(r".*parse_config", false)),
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
    assert_eq!(
        files(idx.grep_candidates("Hello", false)),
        ["a.txt", "b.txt"]
    );
    assert_eq!(
        files(idx.grep_candidates("(?i)hello", false)),
        ["a.txt", "b.txt"]
    );
    assert_eq!(
        files(idx.grep_candidates("hello", false)),
        ["a.txt", "b.txt"]
    );
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
        files(idx.grep_candidates("foo|barbaz", false)),
        ["a.rs", "b.rs", "c.rs"]
    );
    assert_eq!(
        files(idx.grep_candidates("(foo|barbaz)", false)),
        ["a.rs", "b.rs", "c.rs"]
    );
    // A concatenation requires both parts.
    assert_eq!(files(idx.grep_candidates("foo.*barbaz", false)), ["c.rs"]);
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
        assert_eq!(
            idx.grep_candidates(pat, false),
            Candidates::FullScan,
            "{pat:?}"
        );
    }
    assert!(matches!(
        idx.grep_candidates("foo", false),
        Candidates::Files(_)
    ));
}

#[tokio::test]
async fn not_ready_index_falls_back_to_full_scan() {
    let fx = Fixture::new(&[("a.rs", "foo bar\n")]);
    let idx = fx.open();
    assert_eq!(idx.grep_candidates("foo bar", false), Candidates::FullScan);
    assert!(!idx.is_ready() && !idx.trigrams_ready());
    idx.refresh().await.unwrap();
    // Symbols are ready, postings are not: still a full scan.
    assert!(idx.is_ready() && !idx.trigrams_ready());
    assert_eq!(idx.grep_candidates("foo bar", false), Candidates::FullScan);
    idx.build_trigrams().await.unwrap();
    assert!(idx.trigrams_ready());
    assert_eq!(files(idx.grep_candidates("foo bar", false)), ["a.rs"]);
    // A new handle on a populated database is not ready until it has refreshed and
    // confirmed its postings.
    let again = fx.open();
    assert_eq!(
        again.grep_candidates("foo bar", false),
        Candidates::FullScan
    );
    again.refresh().await.unwrap();
    assert_eq!(
        again.grep_candidates("foo bar", false),
        Candidates::FullScan
    );
    assert_eq!(
        again.build_trigrams().await.unwrap(),
        0,
        "nothing was pending"
    );
    assert_eq!(files(again.grep_candidates("foo bar", false)), ["a.rs"]);
}

#[tokio::test]
async fn refresh_leaves_postings_pending_and_pending_files_stay_candidates() {
    let fx = Fixture::new(&[("a.rs", "fn alpha() {}\n"), ("b.rs", "fn beta() {}\n")]);
    let idx = fx.open();
    idx.refresh().await.unwrap();
    assert_eq!(
        trigram_count(&fx.db(), "a.rs"),
        0,
        "phase 1 writes no postings"
    );
    assert_eq!(idx.build_trigrams().await.unwrap(), 2);
    assert!(trigram_count(&fx.db(), "a.rs") > 0);
    assert_eq!(files(idx.grep_candidates("alpha", false)), ["a.rs"]);

    // A refresh that re-reads a changed file drops its postings and marks it pending;
    // until phase 2 runs the index cannot vouch for it, so it stays a candidate.
    std::fs::write(fx.path("a.rs"), "fn gamma() {}\n").unwrap();
    set_future_mtime(&fx.path("a.rs"));
    idx.refresh().await.unwrap();
    assert_eq!(trigram_count(&fx.db(), "a.rs"), 0);
    assert_eq!(files(idx.grep_candidates("gamma", false)), ["a.rs"]);
    assert_eq!(files(idx.grep_candidates("beta", false)), ["a.rs", "b.rs"]);
    assert_eq!(idx.build_trigrams().await.unwrap(), 1);
    assert_eq!(files(idx.grep_candidates("gamma", false)), ["a.rs"]);
    assert_eq!(files(idx.grep_candidates("beta", false)), ["b.rs"]);
}

fn set_future_mtime(path: &Path) {
    let f = std::fs::File::options().write(true).open(path).unwrap();
    f.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(5))
        .unwrap();
}

#[tokio::test]
async fn background_refresh_runs_both_phases() {
    let fx = Fixture::new(&[("a.rs", "fn alpha() {}\n")]);
    let idx = Arc::new(fx.open());
    idx.clone().spawn_background_refresh().await.unwrap();
    assert!(idx.is_ready() && idx.trigrams_ready());
    assert_eq!(files(idx.grep_candidates("alpha", false)), ["a.rs"]);
}

#[tokio::test]
async fn postings_survive_vacuum() {
    let (fx, idx) = ready(&[
        ("a.rs", "fn first_one() {}\n"),
        ("b.rs", "fn second_one() {}\n"),
        ("c.rs", "fn third_one() {}\n"),
    ])
    .await;
    std::fs::remove_file(fx.path("a.rs")).unwrap();
    idx.reindex_file(Path::new("a.rs")).await.unwrap();
    // VACUUM may renumber implicit rowids; postings must not depend on them.
    rusqlite::Connection::open(fx.db())
        .unwrap()
        .execute_batch("VACUUM")
        .unwrap();
    assert_eq!(files(idx.grep_candidates("second_one", false)), ["b.rs"]);
    assert_eq!(files(idx.grep_candidates("third_one", false)), ["c.rs"]);
    assert!(files(idx.grep_candidates("first_one", false)).is_empty());
}

#[tokio::test]
async fn trigram_rows_follow_edits_deletes_and_refresh() {
    let (fx, idx) = ready(&[("a.rs", "fn alpha() {}\n"), ("b.rs", "fn beta() {}\n")]).await;
    assert_eq!(files(idx.grep_candidates("alpha", false)), ["a.rs"]);

    std::fs::write(fx.path("a.rs"), "fn gamma() {}\n").unwrap();
    idx.reindex_file(Path::new("a.rs")).await.unwrap();
    assert!(files(idx.grep_candidates("alpha", false)).is_empty());
    assert_eq!(files(idx.grep_candidates("gamma", false)), ["a.rs"]);

    std::fs::remove_file(fx.path("b.rs")).unwrap();
    idx.reindex_file(Path::new("b.rs")).await.unwrap();
    assert!(files(idx.grep_candidates("beta", false)).is_empty());

    write_all(fx.root.path(), &[("c.txt", "delta text\n")]);
    std::fs::remove_file(fx.path("a.rs")).unwrap();
    idx.refresh().await.unwrap();
    idx.build_trigrams().await.unwrap();
    assert_eq!(files(idx.grep_candidates("delta", false)), ["c.txt"]);
    assert!(files(idx.grep_candidates("gamma", false)).is_empty());
    // No orphaned postings are left behind by the deletes.
    let conn = rusqlite::Connection::open(fx.db()).unwrap();
    let orphans: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM trigrams WHERE file_id NOT IN (SELECT id FROM files)",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(orphans, 0);
}

fn trigram_count(db: &Path, rel: &str) -> i64 {
    let conn = rusqlite::Connection::open(db).unwrap();
    conn.query_row(
        "SELECT COUNT(*) FROM trigrams WHERE file_id = (SELECT id FROM files WHERE path = ?1)",
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
    idx.build_trigrams().await.unwrap();
    assert!(trigram_count(&fx.db(), "ok.txt") > 0);
    assert_eq!(trigram_count(&fx.db(), "bin.dat"), 0);
    assert_eq!(trigram_count(&fx.db(), "big.txt"), 0);
    // They are still tracked, and a narrowed search never silently skips them: the
    // index cannot vouch for their content, so they stay candidates.
    assert_eq!(
        files(idx.grep_candidates("needle", false)),
        ["big.txt", "bin.dat"]
    );
    assert_eq!(
        files(idx.grep_candidates("plenty", false)),
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
    // v1 contents are derived data and are discarded by the migration.
    assert_eq!(idx.tree().len(), 0);

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
    assert_eq!(
        idx.grep_candidates("needle_fn", false),
        Candidates::FullScan
    );

    let stats = idx.refresh().await.unwrap();
    assert_eq!(
        stats.parsed, 2,
        "every file is reparsed after the migration"
    );
    idx.build_trigrams().await.unwrap();
    assert_eq!(files(idx.grep_candidates("needle_fn", false)), ["a.rs"]);
    assert_eq!(idx.find_symbol("needle_fn", None).unwrap().len(), 1);

    // The reparse is a one-time cost.
    let again = idx.refresh().await.unwrap();
    assert_eq!((again.parsed, again.unchanged), (0, 2));
}

#[tokio::test]
async fn changed_extractor_version_forces_a_full_reparse() {
    let (fx, idx) = ready(&[("a.rs", "fn a() {}\n"), ("b.rs", "fn b() {}\n")]).await;
    assert_eq!(idx.refresh().await.unwrap().parsed, 0);
    assert_eq!(idx.build_trigrams().await.unwrap(), 0);
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
    // Every file is pending again, so phase 2 rebuilds all postings.
    assert_eq!(idx.build_trigrams().await.unwrap(), 2);
}

// ----------------------------------------------------------------- proptest

/// Includes Unicode case-fold families: ſ/s, K (Kelvin sign)/k, ẞ/ß, Σ/σ/ς, İ/ı.
const WORDS: [&str; 18] = [
    "abc",
    "bca",
    "cab",
    "aab",
    "ABC",
    "bab",
    "éab",
    "ÉAB",
    "sab",
    "ſab",
    "kab",
    "\u{212a}ab",
    "ßab",
    "ẞab",
    "σab",
    "Σab",
    "ςab",
    "İab",
];

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
        Just("[sſ]ab".to_string()),
        Just("(?i:k)ab".to_string()),
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
        patterns in proptest::collection::vec((arb_pattern(), any::<bool>()), 1..8),
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
        rt.block_on(idx.build_trigrams()).unwrap();
        for (pat, ci) in &patterns {
            let re = regex::RegexBuilder::new(pat).case_insensitive(*ci).build().unwrap();
            // The full scan is the oracle.
            let truth: BTreeSet<&str> = named
                .iter()
                .filter(|(_, c)| re.is_match(c))
                .map(|(r, _)| r.as_str())
                .collect();
            match idx.grep_candidates(pat, *ci) {
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

// ------------------------------------------------------- review round 1 (RED first)

fn contains(c: Candidates, rel: &str) -> bool {
    files(c).iter().any(|f| f == rel)
}

#[tokio::test]
async fn case_insensitive_flag_covers_unicode_case_folds() {
    // (text in the file, pattern): the regex with `case_insensitive(true)` matches each.
    let cases = [
        ("CAFÉ au lait", "café"),
        ("STRAẞE nord", "straße"),
        ("ΣΟΦΊΑ", "σοφία"),
        ("sofia ΣΟΦΊΑ", "σοφία"),
        ("taſk list", "TASK"),
        ("TAſK", "task"),
        ("the kelvin sign: \u{212a}elvin", "kelvin"),
        ("Kelvin", "\u{212a}ELVIN"),
        ("ΜΈΤΡΟΝ", "μέτρον"),
        ("ςοφία", "ΣΟΦΊΑ"),
    ];
    let fx = Fixture::new(&[]);
    for (i, (text, _)) in cases.iter().enumerate() {
        std::fs::write(fx.path(&format!("f{i}.txt")), text).unwrap();
    }
    let idx = fx.open();
    idx.refresh().await.unwrap();
    idx.build_trigrams().await.unwrap();
    for (i, (text, pattern)) in cases.iter().enumerate() {
        let re = regex::RegexBuilder::new(pattern)
            .case_insensitive(true)
            .build()
            .unwrap();
        assert!(
            re.is_match(text),
            "fixture: {pattern:?} must match {text:?}"
        );
        let got = idx.grep_candidates(pattern, true);
        assert!(
            contains(got, &format!("f{i}.txt")),
            "{pattern:?} lost {text:?}"
        );
    }
}

#[tokio::test]
async fn candidates_follow_a_second_handles_changes() {
    let fx = Fixture::new(&[("f.txt", "alpha only\n"), ("g.txt", "other\n")]);
    let a = fx.open();
    a.refresh().await.unwrap();
    a.build_trigrams().await.unwrap();
    assert_eq!(files(a.grep_candidates("alpha", false)), ["f.txt"]);

    // Another handle (think: another process) re-reads the changed file but does not
    // build postings yet. Handle `a` cannot know from its own memory.
    std::fs::write(fx.path("f.txt"), "alpha and gamma\n").unwrap();
    set_future_mtime(&fx.path("f.txt"));
    let b = fx.open();
    b.refresh().await.unwrap();
    assert!(contains(a.grep_candidates("gamma", false), "f.txt"));
}

#[tokio::test]
async fn a_file_replaced_between_lookups_is_not_lost() {
    // Same-handle variant: postings and the always-candidate set come from one snapshot.
    let (fx, idx) = ready(&[("f.txt", "alpha\n"), ("g.txt", "beta\n")]).await;
    std::fs::write(fx.path("f.txt"), "alpha gamma\n").unwrap();
    set_future_mtime(&fx.path("f.txt"));
    idx.refresh().await.unwrap();
    // f.txt is pending; finishing phase 2 flips it to done with fresh postings.
    assert!(contains(idx.grep_candidates("gamma", false), "f.txt"));
    idx.build_trigrams().await.unwrap();
    assert!(contains(idx.grep_candidates("gamma", false), "f.txt"));
}

#[tokio::test]
async fn refresh_read_failure_leaves_the_file_a_candidate() {
    let fx = Fixture::new(&[("f.txt", "alpha\n"), ("g.txt", "beta\n")]);
    let sb = common::ProbeSandbox::new(fx.root.path());
    let idx = Index::open(&fx.db(), sb.clone()).unwrap();
    idx.refresh().await.unwrap();
    idx.build_trigrams().await.unwrap();
    std::fs::write(fx.path("f.txt"), "alpha gamma\n").unwrap();
    set_future_mtime(&fx.path("f.txt"));
    sb.fail_reads_of("f.txt", true);
    idx.refresh().await.unwrap();
    // The new content could not be read, so the old postings must not vouch for it.
    assert!(contains(idx.grep_candidates("gamma", false), "f.txt"));
    // Once readable again the next refresh repairs it.
    sb.fail_reads_of("f.txt", false);
    idx.refresh().await.unwrap();
    idx.build_trigrams().await.unwrap();
    assert_eq!(files(idx.grep_candidates("gamma", false)), ["f.txt"]);
    assert_eq!(files(idx.grep_candidates("beta", false)), ["g.txt"]);
}

#[tokio::test]
async fn reindex_file_read_failure_leaves_the_file_a_candidate() {
    let fx = Fixture::new(&[("f.txt", "alpha\n"), ("g.txt", "beta\n")]);
    let sb = common::ProbeSandbox::new(fx.root.path());
    let idx = Index::open(&fx.db(), sb.clone()).unwrap();
    idx.refresh().await.unwrap();
    idx.build_trigrams().await.unwrap();
    std::fs::write(fx.path("f.txt"), "alpha gamma\n").unwrap();
    sb.fail_reads_of("f.txt", true);
    assert!(idx.reindex_file(Path::new("f.txt")).await.is_err());
    assert!(contains(idx.grep_candidates("gamma", false), "f.txt"));
    // The failed file is a candidate for everything; other files stay narrowed.
    assert_eq!(
        files(idx.grep_candidates("beta", false)),
        ["f.txt", "g.txt"]
    );
    assert_eq!(files(idx.grep_candidates("alpha", false)), ["f.txt"]);
}
