mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use common::{BlockingSandbox, Fixture, HookSandbox, eventually, paths, write_all};
use tautly_index::{Index, RefreshStats};
use tautly_sandbox::LocalSandbox;

const WATCH_WAIT: Duration = Duration::from_secs(2);

fn sample() -> Fixture {
    Fixture::new(&[
        (".gitignore", "ignored/\n*.gen.rs\n"),
        ("src/a.rs", "fn a() {}\n"),
        ("src/b.rs", "fn b() {}\n"),
        ("Cargo.toml", "[package]\nname = \"x\"\n"),
        ("README.md", "# hello\n"),
        ("data.json", "{}"),
        ("ignored/skip.rs", "fn skip() {}\n"),
        ("src/x.gen.rs", "fn skip() {}\n"),
    ])
}

#[tokio::test]
async fn tree_loads_from_db_and_matches_disk() {
    let fx = sample();
    // Neither binary nor oversize files have symbols or trigrams, but both are in the tree.
    std::fs::write(fx.path("bin.dat"), b"\xff\xfe\x00 raw").unwrap();
    std::fs::write(fx.path("big.txt"), "x".repeat(1024 * 1024 + 1)).unwrap();
    let idx = fx.open();
    assert!(idx.tree().is_empty());
    idx.refresh().await.unwrap();

    let expected = [
        "Cargo.toml",
        "README.md",
        "big.txt",
        "bin.dat",
        "data.json",
        "src/a.rs",
        "src/b.rs",
    ];
    let tree = idx.tree();
    assert_eq!(tree.len(), expected.len());
    assert_eq!(paths(&tree.glob("**/*", Path::new("."))), expected);
    for rel in expected {
        let e = tree.get(Path::new(rel)).unwrap_or_else(|| panic!("{rel}"));
        let meta = std::fs::metadata(fx.path(rel)).unwrap();
        assert_eq!(e.path, Path::new(rel));
        assert_eq!(e.size, meta.len(), "{rel}");
        assert_eq!(e.mtime, meta.modified().unwrap(), "{rel}");
        if rel == "big.txt" {
            assert_eq!(e.hash, "", "oversize files are never read");
        } else {
            let want = blake3::hash(&std::fs::read(fx.path(rel)).unwrap()).to_hex();
            assert_eq!(e.hash, want.as_str(), "{rel}");
            assert_eq!(
                idx.file_hash(Path::new(rel)).as_deref(),
                Some(e.hash.as_str())
            );
        }
    }

    // A fresh handle loads the same tree from the database, before any refresh.
    let again = fx.open();
    assert!(!again.is_ready());
    assert_eq!(again.tree().len(), expected.len());
    for rel in expected {
        assert_eq!(
            again.tree().get(Path::new(rel)),
            tree.get(Path::new(rel)),
            "{rel}"
        );
    }
}

#[tokio::test]
async fn binary_and_oversize_files_are_not_re_read_by_refresh() {
    let fx = Fixture::new(&[("a.rs", "fn a() {}\n")]);
    std::fs::write(fx.path("bin.dat"), b"\xff\xfe\x00 raw").unwrap();
    std::fs::write(fx.path("big.txt"), "x".repeat(1024 * 1024 + 1)).unwrap();
    let idx = fx.open();
    let first = idx.refresh().await.unwrap();
    assert_eq!((first.parsed, first.removed), (3, 0));
    // Every file is a known row now, so nothing is read again.
    assert_eq!(
        idx.refresh().await.unwrap(),
        RefreshStats {
            parsed: 0,
            removed: 0,
            unchanged: 3
        }
    );
}

#[tokio::test]
async fn tree_glob_matches_glob_tool_semantics() {
    let fx = Fixture::new(&[
        ("src/a.rs", "fn a() {}"),
        ("src/a/b.rs", "fn b() {}"),
        ("src/c.txt", "text"),
        ("lib.rs", "fn l() {}"),
        ("docs/x.md", "# x"),
    ]);
    let idx = fx.open();
    idx.refresh().await.unwrap();
    let t = idx.tree();
    let root = Path::new(".");
    // A pattern with a slash is anchored and `*` never crosses `/`.
    assert_eq!(paths(&t.glob("src/*.rs", root)), ["src/a.rs"]);
    assert_eq!(
        paths(&t.glob("src/**/*.rs", root)),
        ["src/a.rs", "src/a/b.rs"]
    );
    // A pattern without a slash matches at any depth.
    assert_eq!(
        paths(&t.glob("*.rs", root)),
        ["lib.rs", "src/a.rs", "src/a/b.rs"]
    );
    assert_eq!(paths(&t.glob("**/*.md", root)), ["docs/x.md"]);
    // Patterns are relative to `base`; results stay root-relative and sorted.
    assert_eq!(
        paths(&t.glob("*.rs", Path::new("src"))),
        ["src/a.rs", "src/a/b.rs"]
    );
    assert_eq!(paths(&t.glob("a/*.rs", Path::new("src"))), ["src/a/b.rs"]);
    assert_eq!(
        paths(&t.glob("*.rs", Path::new("./src/"))),
        ["src/a.rs", "src/a/b.rs"]
    );
    assert_eq!(
        paths(&t.glob("*.rs", &fx.path("src"))),
        ["src/a.rs", "src/a/b.rs"]
    );
    assert!(t.glob("*.rs", Path::new("nope")).is_empty());
    assert!(t.glob("[unclosed", root).is_empty());
}

#[tokio::test]
async fn list_dir_returns_direct_files_sorted() {
    let fx = Fixture::new(&[
        ("a.rs", "fn a() {}"),
        ("z.md", "z"),
        ("src/b.rs", "fn b() {}"),
        ("src/a.rs", "fn a() {}"),
        ("src/deep/c.rs", "fn c() {}"),
        ("src.rs", "fn s() {}"),
    ]);
    let idx = fx.open();
    idx.refresh().await.unwrap();
    let t = idx.tree();
    let names = |dir: &str| -> Vec<String> {
        t.list_dir(Path::new(dir))
            .iter()
            .map(|e| e.path.to_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(names("."), ["a.rs", "src.rs", "z.md"]);
    assert_eq!(names("src"), ["src/a.rs", "src/b.rs"]);
    assert_eq!(names("./src/"), ["src/a.rs", "src/b.rs"]);
    assert_eq!(names("src/deep"), ["src/deep/c.rs"]);
    assert!(names("missing").is_empty());
}

#[tokio::test]
async fn edit_via_reindex_updates_tree_synchronously() {
    let fx = Fixture::new(&[("a.rs", "fn a() {}\n")]);
    let idx = fx.open();
    idx.refresh().await.unwrap();
    assert_eq!(idx.tree().len(), 1);

    // Created (any extension is tracked), then edited, then deleted.
    std::fs::write(fx.path("notes.md"), "one").unwrap();
    idx.reindex_file(Path::new("notes.md")).await.unwrap();
    let first = idx.tree().get(Path::new("notes.md")).unwrap();
    assert_eq!(first.size, 3);

    std::fs::write(fx.path("notes.md"), "three").unwrap();
    idx.reindex_file(Path::new("notes.md")).await.unwrap();
    let second = idx.tree().get(Path::new("notes.md")).unwrap();
    assert_eq!(second.size, 5);
    assert_ne!(second.hash, first.hash);
    assert_eq!(idx.tree().len(), 2);

    std::fs::remove_file(fx.path("notes.md")).unwrap();
    idx.reindex_file(Path::new("notes.md")).await.unwrap();
    assert!(idx.tree().get(Path::new("notes.md")).is_none());
    assert_eq!(idx.tree().len(), 1);
}

#[tokio::test]
async fn refresh_reconciles_the_tree() {
    let fx = Fixture::new(&[("a.rs", "fn a() {}\n"), ("b.rs", "fn b() {}\n")]);
    let idx = fx.open();
    idx.refresh().await.unwrap();
    std::fs::remove_file(fx.path("a.rs")).unwrap();
    write_all(fx.root.path(), &[("c.txt", "new")]);
    idx.refresh().await.unwrap();
    assert_eq!(
        paths(&idx.tree().glob("**/*", Path::new("."))),
        ["b.rs", "c.txt"]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn watcher_picks_up_external_create_modify_delete_within_1s() {
    let fx = Fixture::new(&[("a.rs", "fn a() {}\n")]);
    let idx = Arc::new(fx.open());
    idx.refresh().await.unwrap();
    let _watch = idx.clone().start_watcher().unwrap();
    let one_second = Duration::from_secs(1);

    std::fs::write(fx.path("new.rs"), "fn fresh() {}\n").unwrap();
    assert!(
        eventually(one_second, || idx.tree().get(Path::new("new.rs")).is_some()).await,
        "create"
    );
    assert!(
        eventually(one_second, || {
            idx.find_symbol("fresh", None).is_ok_and(|s| s.len() == 1)
        })
        .await,
        "symbols follow"
    );

    let before = idx.tree().get(Path::new("new.rs")).unwrap().hash;
    std::fs::write(fx.path("new.rs"), "fn fresh() {}\nfn more() {}\n").unwrap();
    assert!(
        eventually(one_second, || {
            idx.tree()
                .get(Path::new("new.rs"))
                .is_some_and(|e| e.hash != before)
        })
        .await,
        "modify"
    );
    assert_eq!(idx.find_symbol("more", None).unwrap().len(), 1);

    std::fs::remove_file(fx.path("new.rs")).unwrap();
    assert!(
        eventually(one_second, || idx.tree().get(Path::new("new.rs")).is_none()).await,
        "delete"
    );
    assert!(idx.outline(Path::new("new.rs")).unwrap().is_empty());
    assert_eq!(idx.file_hash(Path::new("new.rs")), None);
}

#[tokio::test(flavor = "multi_thread")]
async fn watcher_picks_up_new_and_removed_directories() {
    let fx = Fixture::new(&[("a.rs", "fn a() {}\n")]);
    let idx = Arc::new(fx.open());
    idx.refresh().await.unwrap();
    let _watch = idx.clone().start_watcher().unwrap();

    write_all(
        fx.root.path(),
        &[("pkg/inner/x.rs", "fn x() {}\n"), ("pkg/y.md", "y")],
    );
    assert!(
        eventually(WATCH_WAIT, || {
            idx.tree().get(Path::new("pkg/inner/x.rs")).is_some()
                && idx.tree().get(Path::new("pkg/y.md")).is_some()
        })
        .await,
        "new directory tree"
    );

    std::fs::remove_dir_all(fx.path("pkg")).unwrap();
    assert!(
        eventually(WATCH_WAIT, || idx.tree().len() == 1).await,
        "removed directory tree"
    );
    assert_eq!(idx.stats().files, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn watcher_ignores_gitignored_paths() {
    let fx = sample();
    let idx = Arc::new(fx.open());
    idx.refresh().await.unwrap();
    let before = idx.tree().len();
    let watch = idx.clone().start_watcher().unwrap();

    write_all(
        fx.root.path(),
        &[
            ("ignored/new.rs", "fn n() {}\n"),
            ("src/y.gen.rs", "fn g() {}\n"),
            (".hidden/h.rs", "fn h() {}\n"),
        ],
    );
    // The sentinel is written last; once it is indexed the ignored events were seen too.
    write_all(fx.root.path(), &[("sentinel.rs", "fn sentinel() {}\n")]);
    assert!(
        eventually(WATCH_WAIT, || idx
            .tree()
            .get(Path::new("sentinel.rs"))
            .is_some())
        .await,
        "sentinel"
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(idx.tree().len(), before + 1);
    for rel in ["ignored/new.rs", "src/y.gen.rs", ".hidden/h.rs"] {
        assert!(idx.tree().get(Path::new(rel)).is_none(), "{rel}");
        assert_eq!(idx.file_hash(Path::new(rel)), None, "{rel}");
    }

    // Dropping the handle stops the watcher.
    drop(watch);
    tokio::time::sleep(Duration::from_millis(200)).await;
    write_all(fx.root.path(), &[("after_drop.rs", "fn d() {}\n")]);
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(idx.tree().get(Path::new("after_drop.rs")).is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn watcher_reconciles_after_gitignore_changes() {
    let fx = Fixture::new(&[("keep.rs", "fn k() {}\n"), ("later.rs", "fn l() {}\n")]);
    let idx = Arc::new(fx.open());
    idx.refresh().await.unwrap();
    assert_eq!(idx.tree().len(), 2);
    let _watch = idx.clone().start_watcher().unwrap();
    std::fs::write(fx.path(".gitignore"), "later.rs\n").unwrap();
    assert!(
        eventually(WATCH_WAIT, || idx
            .tree()
            .get(Path::new("later.rs"))
            .is_none())
        .await,
        "newly ignored file leaves the tree"
    );
    assert!(idx.tree().get(Path::new("keep.rs")).is_some());
}

#[tokio::test]
async fn reindex_honors_the_sandbox_walkers_own_filter() {
    let t = tempfile::tempdir().unwrap();
    write_all(
        t.path(),
        &[("ok/y.rs", "fn y() {}\n"), ("blocked/x.rs", "fn x() {}\n")],
    );
    let sb = Arc::new(BlockingSandbox {
        inner: LocalSandbox::new(t.path().to_path_buf()).unwrap(),
    });
    let idx = Index::open(&t.path().join(".index.db"), sb).unwrap();
    idx.refresh().await.unwrap();
    assert_eq!(paths(&idx.tree().glob("**/*", Path::new("."))), ["ok/y.rs"]);

    std::fs::write(t.path().join("blocked/x.rs"), "fn edited() {}\n").unwrap();
    idx.reindex_file(Path::new("blocked/x.rs")).await.unwrap();
    assert!(idx.tree().get(Path::new("blocked/x.rs")).is_none());
    assert!(idx.find_symbol("edited", None).unwrap().is_empty());
    std::fs::write(t.path().join("ok/y.rs"), "fn y2() {}\n").unwrap();
    idx.reindex_file(Path::new("ok/y.rs")).await.unwrap();
    assert_eq!(idx.find_symbol("y2", None).unwrap().len(), 1);
}

#[tokio::test]
async fn reindex_never_follows_symlinks_out_of_the_root() {
    let outside = tempfile::tempdir().unwrap();
    write_all(outside.path(), &[("secret.rs", "fn secret() {}\n")]);
    let fx = Fixture::new(&[("a.rs", "fn a() {}\n")]);
    std::os::unix::fs::symlink(outside.path(), fx.path("link")).unwrap();
    std::os::unix::fs::symlink(outside.path().join("secret.rs"), fx.path("s.rs")).unwrap();
    let idx = fx.open();
    idx.refresh().await.unwrap();
    for rel in ["link/secret.rs", "s.rs"] {
        idx.reindex_file(Path::new(rel)).await.unwrap();
    }
    assert!(idx.find_symbol("secret", None).unwrap().is_empty());
    assert_eq!(paths(&idx.tree().glob("**/*", Path::new("."))), ["a.rs"]);
}

#[tokio::test]
async fn single_file_admission_agrees_with_the_full_walk() {
    let fx = Fixture::new(&[
        (
            ".gitignore",
            "/top_only.rs\nsrc/gen/\n*.log\n!keep.log\nbuild\n",
        ),
        ("top_only.rs", "fn a() {}\n"),
        ("sub/top_only.rs", "fn b() {}\n"),
        ("src/lib.rs", "fn c() {}\n"),
        ("src/gen/out.rs", "fn d() {}\n"),
        ("src/deep/gen/x.rs", "fn e() {}\n"),
        ("run.log", "log"),
        ("keep.log", "log"),
        ("build/out.txt", "o"),
        ("src/build", "a file, not a directory"),
        ("nested/.gitignore", "local.rs\n"),
        ("nested/local.rs", "fn f() {}\n"),
        ("nested/other.rs", "fn g() {}\n"),
        (".hidden/h.rs", "fn h() {}\n"),
    ]);
    let idx = fx.open();
    idx.refresh().await.unwrap();
    let walked = idx.tree().glob("**/*", Path::new("."));
    // Touch every file on disk (ignored ones too) through the single-file path: it
    // must admit exactly the files the full walk did.
    let mut on_disk = Vec::new();
    let mut stack = vec![fx.root.path().to_path_buf()];
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(dir).unwrap() {
            let e = e.unwrap();
            if e.file_name() == ".git" {
                continue;
            }
            if e.file_type().unwrap().is_dir() {
                stack.push(e.path());
            } else {
                on_disk.push(e.path().strip_prefix(fx.root.path()).unwrap().to_path_buf());
            }
        }
    }
    assert!(
        on_disk.len() > walked.len(),
        "fixture must contain ignored files"
    );
    for rel in &on_disk {
        let mut text = std::fs::read(fx.path(rel.to_str().unwrap())).unwrap();
        text.push(b'\n');
        std::fs::write(fx.path(rel.to_str().unwrap()), text).unwrap();
        idx.reindex_file(rel).await.unwrap();
    }
    assert_eq!(idx.tree().glob("**/*", Path::new(".")), walked);
    assert!(walked.iter().any(|p| p == Path::new("keep.log")));
    assert!(!walked.iter().any(|p| p == Path::new("src/gen/out.rs")));
}

#[tokio::test]
async fn files_reachable_through_in_root_symlinks_are_indexed_like_the_tools_see_them() {
    let fx = Fixture::new(&[
        ("real/x.rs", "fn via_link() { unique_link_token(); }\n"),
        ("plain.rs", "fn plain() {}\n"),
    ]);
    let outside = tempfile::tempdir().unwrap();
    write_all(
        outside.path(),
        &[("secret.rs", "fn unique_link_token() {}\n")],
    );
    // A linked directory, a linked file, a cycle, and links leaving the root.
    std::os::unix::fs::symlink("real", fx.path("alias")).unwrap();
    std::os::unix::fs::symlink("real/x.rs", fx.path("link.rs")).unwrap();
    std::os::unix::fs::symlink(".", fx.path("real/loop")).unwrap();
    std::os::unix::fs::symlink(outside.path(), fx.path("escape")).unwrap();
    std::os::unix::fs::symlink(outside.path().join("secret.rs"), fx.path("out.rs")).unwrap();
    std::os::unix::fs::symlink("missing.rs", fx.path("dangling.rs")).unwrap();
    let idx = fx.open();
    idx.refresh().await.unwrap();

    let tree = idx.tree();
    let walked = tree.glob("**/*", Path::new("."));
    let all = paths(&walked);
    for want in ["alias/x.rs", "link.rs", "real/x.rs", "plain.rs"] {
        assert!(all.contains(&want), "{want} missing from {all:?}");
    }
    for never in ["escape/secret.rs", "out.rs", "dangling.rs"] {
        assert!(!all.contains(&never), "{never} must stay outside the tree");
    }
    assert_eq!(
        tree.get(Path::new("alias/x.rs")).unwrap().hash,
        tree.get(Path::new("real/x.rs")).unwrap().hash
    );
    idx.build_trigrams().await.unwrap();
    match idx.grep_candidates("unique_link_token", false) {
        tautly_index::Candidates::Files(f) => {
            let f = paths(&f);
            assert!(f.contains(&"alias/x.rs") && f.contains(&"link.rs"), "{f:?}");
            assert!(
                !f.iter().any(|p| p.contains("secret") || *p == "out.rs"),
                "{f:?}"
            );
        }
        tautly_index::Candidates::FullScan => panic!("expected candidates"),
    }

    // Rows reached through a link are never vouched for by postings: they stay
    // candidates for every pattern, so an edit through any alias cannot hide a match.
    for rel in ["alias/x.rs", "link.rs"] {
        let any = idx.grep_candidates("zzz_absent_zzz", false);
        assert!(
            matches!(&any, tautly_index::Candidates::Files(f) if paths(f).contains(&rel)),
            "{rel}: {any:?}"
        );
    }
    // Edit through one alias: every path to the same file stays a candidate.
    std::fs::write(
        fx.path("real/x.rs"),
        "fn via_link() { changed_token_xyz(); }\n",
    )
    .unwrap();
    idx.reindex_file(Path::new("alias/x.rs")).await.unwrap();
    match idx.grep_candidates("changed_token_xyz", false) {
        tautly_index::Candidates::Files(f) => {
            let f = paths(&f);
            for want in ["alias/x.rs", "link.rs", "real/x.rs"] {
                assert!(
                    f.contains(&want),
                    "{want} lost after an edit through a link: {f:?}"
                );
            }
        }
        tautly_index::Candidates::FullScan => panic!("expected candidates"),
    }
}

fn probe_fixture(extra: &[(&str, &str)]) -> (Fixture, Arc<common::ProbeSandbox>, Arc<Index>) {
    let mut files = vec![
        (".gitignore", "target/\n"),
        ("src/a.rs", "fn a() {}\n"),
        ("src/b.rs", "fn b() {}\n"),
        ("src/c.rs", "fn c() {}\n"),
        ("src/d.rs", "fn d() {}\n"),
        ("src/e.rs", "fn e() {}\n"),
        ("target/keep", "x"),
    ];
    files.extend_from_slice(extra);
    let fx = Fixture::new(&files);
    let sb = common::ProbeSandbox::new(fx.root.path());
    let idx = Arc::new(Index::open(&fx.db(), sb.clone()).unwrap());
    (fx, sb, idx)
}

#[tokio::test(flavor = "multi_thread")]
async fn events_under_ignored_directories_cause_no_work_even_with_symlinks() {
    let (fx, sb, idx) = probe_fixture(&[]);
    std::os::unix::fs::symlink("src", fx.path("alias")).unwrap();
    idx.refresh().await.unwrap();
    let _watch = idx.clone().start_watcher().unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let before = sb.calls();
    let refreshes = idx.refresh_count();

    for i in 0..20 {
        write_all(fx.root.path(), &[(&format!("target/debug/f{i}.o"), "obj")]);
    }
    std::fs::write(fx.path("target/.gitignore"), "*\n").unwrap();
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(
        sb.calls(),
        before,
        "ignored paths must not trigger a reindex"
    );
    assert_eq!(
        idx.refresh_count(),
        refreshes,
        "ignored paths must not trigger a refresh"
    );

    // Control: a change to an admitted file is picked up (re-indexed, and with links in
    // the tree, reconciled by a full refresh).
    std::fs::write(fx.path("src/a.rs"), "fn a() {}\nfn extra() {}\n").unwrap();
    assert!(
        eventually(WATCH_WAIT, || sb.calls() > before
            && idx.refresh_count() > refreshes)
        .await,
        "admitted change must be processed"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn watcher_failure_leaves_the_file_a_candidate_and_is_reported() {
    let (fx, sb, idx) = probe_fixture(&[]);
    idx.refresh().await.unwrap();
    idx.build_trigrams().await.unwrap();
    let _watch = idx.clone().start_watcher().unwrap();
    sb.fail_reads_of("src/a.rs", true);
    std::fs::write(fx.path("src/a.rs"), "fn a() { gamma_marker(); }\n").unwrap();
    assert!(
        eventually(WATCH_WAIT, || idx.last_error().is_some()).await,
        "the failed reindex is reported"
    );
    match idx.grep_candidates("gamma_marker", false) {
        tautly_index::Candidates::Files(f) => assert!(paths(&f).contains(&"src/a.rs"), "{f:?}"),
        tautly_index::Candidates::FullScan => {}
    }
}

// ------------------------------------------------------ review round 2 (RED first)

fn has_candidate(idx: &Index, pattern: &str, rel: &str) -> bool {
    match idx.grep_candidates(pattern, false) {
        tautly_index::Candidates::Files(f) => paths(&f).contains(&rel),
        tautly_index::Candidates::FullScan => true,
    }
}

fn row(db: &Path, rel: &str) -> (i64, i64) {
    let c = rusqlite::Connection::open(db).unwrap();
    c.query_row(
        "SELECT trigram_state, (SELECT COUNT(*) FROM trigrams t WHERE t.file_id = f.id) \
         FROM files f WHERE path = ?1",
        [rel],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .unwrap()
}

#[tokio::test]
async fn an_edit_via_a_link_after_an_edit_via_the_real_path_keeps_the_real_row_correct() {
    let fx = Fixture::new(&[("real/x.rs", "fn h0() {}\n")]);
    std::os::unix::fs::symlink("real", fx.path("alias")).unwrap();
    let idx = fx.open();
    idx.refresh().await.unwrap();
    idx.build_trigrams().await.unwrap();
    std::fs::write(fx.path("real/x.rs"), "fn h1_first_edit() {}\n").unwrap();
    idx.reindex_file(Path::new("real/x.rs")).await.unwrap();
    std::fs::write(fx.path("alias/x.rs"), "fn h2_second_token() {}\n").unwrap();
    idx.reindex_file(Path::new("alias/x.rs")).await.unwrap();
    for rel in ["real/x.rs", "alias/x.rs"] {
        assert!(has_candidate(&idx, "h2_second_token", rel), "{rel}");
    }
    // The canonical row was re-read, so it is narrowed again rather than pending forever.
    assert!(!has_candidate(&idx, "zzz_absent_zzz", "real/x.rs"));
    assert_eq!(row(&fx.db(), "real/x.rs").0, 1);
}

#[tokio::test]
async fn an_edit_via_a_link_does_not_invalidate_unrelated_copies() {
    let fx = Fixture::new(&[
        ("real/x.rs", "fn same() {}\n"),
        ("copy1.rs", "fn same() {}\n"),
        ("copy2.rs", "fn same() {}\n"),
    ]);
    std::os::unix::fs::symlink("real", fx.path("alias")).unwrap();
    let idx = fx.open();
    idx.refresh().await.unwrap();
    idx.build_trigrams().await.unwrap();
    std::fs::write(fx.path("alias/x.rs"), "fn changed() {}\n").unwrap();
    idx.reindex_file(Path::new("alias/x.rs")).await.unwrap();
    for rel in ["copy1.rs", "copy2.rs"] {
        assert!(
            !has_candidate(&idx, "zzz_absent_zzz", rel),
            "{rel} was swept"
        );
        assert_eq!(row(&fx.db(), rel).0, 1);
    }
}

#[tokio::test]
async fn reindexing_unchanged_content_restores_postings_after_the_pending_mark() {
    let fx = Fixture::new(&[
        ("a.rs", "fn alpha_token() {}\n"),
        ("b.rs", "fn beta_token() {}\n"),
    ]);
    let idx = fx.open();
    idx.refresh().await.unwrap();
    idx.build_trigrams().await.unwrap();
    // What the watcher does before re-reading: postings dropped, file pending.
    {
        let c = rusqlite::Connection::open(fx.db()).unwrap();
        c.execute_batch(
            "DELETE FROM trigrams WHERE file_id = (SELECT id FROM files WHERE path = 'a.rs');
             UPDATE files SET trigram_state = 2 WHERE path = 'a.rs';",
        )
        .unwrap();
    }
    idx.reindex_file(Path::new("a.rs")).await.unwrap();
    let (state, postings) = row(&fx.db(), "a.rs");
    assert_eq!(state, 1, "unchanged content must not leave the row pending");
    assert!(postings > 0);
    assert!(!has_candidate(&idx, "beta_token", "a.rs"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_same_content_save_with_the_watcher_running_keeps_postings() {
    let fx = Fixture::new(&[
        ("src/a.rs", "fn alpha_token() {}\n"),
        ("src/b.rs", "fn beta_token() {}\n"),
    ]);
    let idx = Arc::new(fx.open());
    idx.refresh().await.unwrap();
    idx.build_trigrams().await.unwrap();
    let _w = idx.clone().start_watcher().unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (_, before) = row(&fx.db(), "src/a.rs");
    std::fs::write(fx.path("src/a.rs"), "fn alpha_token() {}\n").unwrap();
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(row(&fx.db(), "src/a.rs"), (1, before));
    assert!(!has_candidate(&idx, "beta_token", "src/a.rs"));
}

#[tokio::test(flavor = "multi_thread")]
async fn events_in_many_ignored_directories_cost_one_walk_per_ignored_ancestor() {
    let (fx, sb, idx) = probe_fixture(&[]);
    idx.refresh().await.unwrap();
    let _watch = idx.clone().start_watcher().unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let before = sb.walks();
    for i in 0..30 {
        write_all(fx.root.path(), &[(&format!("target/gen/d{i}/f.o"), "obj")]);
    }
    tokio::time::sleep(Duration::from_millis(900)).await;
    let walks = sb.walks() - before;
    assert!(
        walks <= 3,
        "{walks} walks for events under one ignored directory"
    );
}

#[tokio::test]
async fn failures_before_the_read_also_leave_the_file_a_candidate() {
    let (fx, sb, idx) = probe_fixture(&[]);
    idx.refresh().await.unwrap();
    idx.build_trigrams().await.unwrap();

    std::fs::write(fx.path("src/a.rs"), "fn a() { meta_marker(); }\n").unwrap();
    sb.fail_metadata_of("src/a.rs", true);
    assert!(idx.reindex_file(Path::new("src/a.rs")).await.is_err());
    assert!(has_candidate(&idx, "meta_marker", "src/a.rs"));
    assert!(!has_candidate(&idx, "meta_marker", "src/b.rs"));

    std::fs::write(fx.path("src/b.rs"), "fn b() { walk_marker(); }\n").unwrap();
    sb.fail_walk
        .store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(idx.reindex_file(Path::new("src/b.rs")).await.is_err());
    sb.fail_walk
        .store(false, std::sync::atomic::Ordering::SeqCst);
    assert!(has_candidate(&idx, "walk_marker", "src/b.rs"));
    assert!(!has_candidate(&idx, "walk_marker", "src/c.rs"));
}

#[tokio::test]
async fn hard_linked_files_are_never_vouched_for_by_postings() {
    let fx = Fixture::new(&[("a.rs", "fn shared() {}\n"), ("lone.rs", "fn lone() {}\n")]);
    std::fs::hard_link(fx.path("a.rs"), fx.path("sub_link.rs")).unwrap();
    let idx = fx.open();
    idx.refresh().await.unwrap();
    idx.build_trigrams().await.unwrap();
    for rel in ["a.rs", "sub_link.rs"] {
        assert!(has_candidate(&idx, "zzz_absent_zzz", rel), "{rel}");
        assert_eq!(row(&fx.db(), rel), (0, 0));
    }
    assert!(!has_candidate(&idx, "zzz_absent_zzz", "lone.rs"));
}

#[tokio::test(flavor = "multi_thread")]
async fn watcher_reconciles_after_git_info_exclude_changes() {
    let fx = Fixture::new(&[
        ("keep.rs", "fn k() {}\n"),
        ("later.rs", "fn l() {}\n"),
        (".git/info/exclude", "# nothing yet\n"),
    ]);
    let idx = Arc::new(fx.open());
    idx.refresh().await.unwrap();
    assert_eq!(idx.tree().len(), 2);
    let _watch = idx.clone().start_watcher().unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    std::fs::write(fx.path(".git/info/exclude"), "later.rs\n").unwrap();
    assert!(
        eventually(WATCH_WAIT, || idx
            .tree()
            .get(Path::new("later.rs"))
            .is_none())
        .await,
        "newly excluded file leaves the tree"
    );
    assert!(idx.tree().get(Path::new("keep.rs")).is_some());
}

// ------------------------------------------------------ review round 3 (RED first)

/// Writes through `alias` (a name the walker never admits) and reports the change under
/// that name: the canonical row, which is the only one that can hold postings, must end
/// up a candidate for the new content.
async fn edit_through_unadmitted_name(
    files: &[(&str, &str)],
    alias: &str,
    link: Option<(&str, &str)>,
) {
    let fx = Fixture::new(files);
    if let Some((target, name)) = link {
        std::os::unix::fs::symlink(target, fx.path(name)).unwrap();
    }
    let idx = fx.open();
    idx.refresh().await.unwrap();
    idx.build_trigrams().await.unwrap();
    assert!(!has_candidate(&idx, "alias_edit_token", "real/x.rs"));
    std::fs::write(fx.path(alias), "fn changed() { alias_edit_token(); }\n").unwrap();
    idx.reindex_file(Path::new(alias)).await.unwrap();
    assert!(
        has_candidate(&idx, "alias_edit_token", "real/x.rs"),
        "edit through {alias} hid real/x.rs"
    );
    // The canonical row was re-read, not just distrusted.
    assert!(!has_candidate(&idx, "zzz_absent_zzz", "real/x.rs"));
    assert_eq!(row(&fx.db(), "real/x.rs").0, 1);
}

#[tokio::test]
async fn an_edit_through_a_hidden_alias_updates_the_canonical_row() {
    edit_through_unadmitted_name(
        &[("real/x.rs", "fn old() {}\n")],
        ".hl/x.rs",
        Some(("real", ".hl")),
    )
    .await;
}

#[tokio::test]
async fn an_edit_through_a_gitignored_alias_updates_the_canonical_row() {
    edit_through_unadmitted_name(
        &[
            ("real/x.rs", "fn old() {}\n"),
            (".gitignore", "lnk_ignored\n"),
        ],
        "lnk_ignored/x.rs",
        Some(("real", "lnk_ignored")),
    )
    .await;
}

#[tokio::test]
async fn an_edit_through_a_case_variant_path_updates_the_canonical_row() {
    let probe = tempfile::tempdir().unwrap();
    std::fs::write(probe.path().join("a"), "").unwrap();
    if !probe.path().join("A").exists() {
        eprintln!("skipped: case-sensitive filesystem");
        return;
    }
    edit_through_unadmitted_name(&[("real/x.rs", "fn old() {}\n")], "REAL/x.rs", None).await;
}

#[tokio::test]
async fn failures_on_an_unadmitted_alias_still_distrust_the_canonical_row() {
    let (fx, sb, idx) = probe_fixture(&[("real/x.rs", "fn old() {}\n")]);
    std::os::unix::fs::symlink("real", fx.path(".hl")).unwrap();
    idx.refresh().await.unwrap();
    idx.build_trigrams().await.unwrap();
    std::fs::write(
        fx.path("real/x.rs"),
        "fn changed() { meta_alias_token(); }\n",
    )
    .unwrap();
    sb.fail_metadata_of(".hl/x.rs", true);
    assert!(idx.reindex_file(Path::new(".hl/x.rs")).await.is_err());
    assert!(has_candidate(&idx, "meta_alias_token", "real/x.rs"));
}

#[tokio::test]
async fn deleting_through_an_alias_drops_the_canonical_row() {
    let fx = Fixture::new(&[
        ("real/x.rs", "fn old() {}\n"),
        ("keep.rs", "fn keep() {}\n"),
    ]);
    std::os::unix::fs::symlink("real", fx.path("alias")).unwrap();
    let idx = fx.open();
    idx.refresh().await.unwrap();
    std::fs::remove_file(fx.path("alias/x.rs")).unwrap();
    idx.reindex_file(Path::new("alias/x.rs")).await.unwrap();
    assert!(idx.tree().get(Path::new("real/x.rs")).is_none());
    assert!(idx.tree().get(Path::new("alias/x.rs")).is_none());
}

#[tokio::test]
async fn tree_contains_files_and_directories_with_files_only() {
    let fx = sample();
    let idx = fx.open();
    idx.refresh().await.unwrap();
    let tree = idx.tree();
    for known in ["src", "src/a.rs", "README.md", "./src", "."] {
        assert!(tree.contains(Path::new(known)), "{known}");
    }
    // Ignored, hidden-by-ignore and missing paths are not in the tree.
    for unknown in [
        "ignored",
        "ignored/skip.rs",
        "src/x.gen.rs",
        "nope",
        "src/a",
    ] {
        assert!(!tree.contains(Path::new(unknown)), "{unknown}");
    }
}

#[tokio::test]
async fn mark_stale_lasts_until_a_refresh_that_started_after_it_completes() {
    let fx = sample();
    let idx = fx.open();
    idx.refresh().await.unwrap();
    assert!(!idx.is_stale());
    idx.mark_stale();
    assert!(idx.is_stale());
    idx.refresh().await.unwrap();
    assert!(!idx.is_stale());
    idx.mark_stale();
    idx.mark_stale();
    assert!(idx.is_stale());
    idx.refresh().await.unwrap();
    assert!(!idx.is_stale());
}

#[tokio::test]
async fn a_refresh_that_started_before_the_mark_does_not_clear_it() {
    let fx = sample();
    let slot: Arc<std::sync::OnceLock<Arc<Index>>> = Arc::default();
    let hook_slot = Arc::clone(&slot);
    let sb = Arc::new(HookSandbox {
        inner: LocalSandbox::new(fx.root.path().to_path_buf()).unwrap(),
        // Runs after the refresh has started scanning.
        on_walk: std::sync::Mutex::new(Some(Box::new(move || {
            hook_slot.get().unwrap().mark_stale();
        }))),
    });
    let idx = Arc::new(Index::open(&fx.root.path().join(".index.db"), sb).unwrap());
    slot.set(Arc::clone(&idx)).ok();
    idx.refresh().await.unwrap();
    assert!(idx.is_stale(), "the mark came after this refresh began");
    idx.refresh().await.unwrap();
    assert!(!idx.is_stale());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_watcher_batch_alone_does_not_clear_staleness() {
    let fx = sample();
    let idx = Arc::new(fx.open());
    idx.refresh().await.unwrap();
    let _watch = idx.clone().start_watcher().unwrap();
    std::fs::write(fx.path("made_by_shell.rs"), "fn s() {}\n").unwrap();
    idx.mark_stale();
    // The batch has run once the tree knows the file; give it time to (wrongly) clear.
    assert!(
        eventually(WATCH_WAIT, || idx
            .tree()
            .get(Path::new("made_by_shell.rs"))
            .is_some())
        .await
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(idx.is_stale());
    idx.refresh().await.unwrap();
    assert!(!idx.is_stale());
}

#[tokio::test]
async fn background_refresh_clears_staleness_and_coalesces() {
    let fx = sample();
    let idx = Arc::new(fx.open());
    idx.refresh().await.unwrap();
    // Nothing stale: nothing to do, but the loop still ends cleanly.
    idx.refresh_stale_in_background().unwrap().await.unwrap();

    std::fs::write(fx.path("later.rs"), "fn later() {}\n").unwrap();
    idx.mark_stale();
    let first = idx.refresh_stale_in_background().expect("starts a loop");
    // On this single-threaded runtime the loop has not run yet.
    assert!(idx.refresh_stale_in_background().is_none(), "coalesced");
    first.await.unwrap();
    assert!(!idx.is_stale());
    assert!(idx.tree().get(Path::new("later.rs")).is_some());
    // A new loop can start once the previous one finished.
    idx.mark_stale();
    idx.refresh_stale_in_background().unwrap().await.unwrap();
    assert!(!idx.is_stale());
}

#[tokio::test]
async fn background_refresh_runs_again_for_a_mark_made_during_it() {
    let fx = sample();
    let slot: Arc<std::sync::OnceLock<Arc<Index>>> = Arc::default();
    let hook_slot = Arc::clone(&slot);
    let sb = Arc::new(HookSandbox {
        inner: LocalSandbox::new(fx.root.path().to_path_buf()).unwrap(),
        on_walk: std::sync::Mutex::new(None),
    });
    let idx = Arc::new(Index::open(&fx.root.path().join(".index.db"), sb.clone()).unwrap());
    slot.set(Arc::clone(&idx)).ok();
    idx.refresh().await.unwrap();
    idx.mark_stale();
    // The loop's first refresh is overtaken by a second mark.
    *sb.on_walk.lock().unwrap() = Some(Box::new(move || hook_slot.get().unwrap().mark_stale()));
    idx.refresh_stale_in_background().unwrap().await.unwrap();
    assert!(!idx.is_stale(), "a second pass covered the second mark");
}

#[tokio::test]
async fn dirs_count_files_and_languages_per_directory() {
    let fx = Fixture::new(&[
        ("top.md", "# top\n"),
        ("a/x.rs", "fn x() {}\n"),
        ("a/b/y.py", "def y(): pass\n"),
        ("a/b/z.rs", "fn z() {}\n"),
        ("a-b/w.txt", "w\n"),
        ("c/d/e/deep.go", "package e\n"),
    ]);
    let idx = fx.open();
    idx.refresh().await.unwrap();
    let tree = idx.tree();
    let got: Vec<(String, usize)> = tree
        .dirs(1)
        .into_iter()
        .map(|d| (d.path.display().to_string(), d.files))
        .collect();
    assert_eq!(got, [("a".into(), 3), ("a-b".into(), 1), ("c".into(), 1)]);
    let two = tree.dirs(2);
    let names: Vec<String> = two.iter().map(|d| d.path.display().to_string()).collect();
    assert_eq!(names, ["a", "a/b", "a-b", "c", "c/d"]);
    let a = &two[0];
    assert_eq!(
        a.langs,
        [
            (tautly_index::Lang::Rust, 2),
            (tautly_index::Lang::Python, 1)
        ]
    );
    assert!(tree.dirs(0).is_empty());
    assert_eq!(
        tree.get(Path::new("a/b/y.py")).unwrap().lang,
        Some(tautly_index::Lang::Python)
    );
    assert_eq!(tree.get(Path::new("top.md")).unwrap().lang, None);
}

#[tokio::test]
async fn list_dir_and_glob_keep_path_order_across_sibling_directories() {
    let fx = Fixture::new(&[
        ("a/z.rs", ""),
        ("a/b/x.rs", ""),
        ("a-b/y.rs", ""),
        ("a.rs", ""),
    ]);
    let idx = fx.open();
    idx.refresh().await.unwrap();
    let tree = idx.tree();
    assert_eq!(
        paths(&tree.glob("**/*.rs", Path::new("."))),
        ["a-b/y.rs", "a.rs", "a/b/x.rs", "a/z.rs"]
    );
    let listed: Vec<String> = tree
        .list_dir(Path::new("a"))
        .into_iter()
        .map(|e| e.path.display().to_string())
        .collect();
    assert_eq!(listed, ["a/z.rs"]);
    assert!(tree.contains(Path::new("a/b")));
    assert!(!tree.contains(Path::new("a/c")));
}
