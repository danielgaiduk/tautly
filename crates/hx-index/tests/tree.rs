mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use common::{BlockingSandbox, Fixture, eventually, paths, write_all};
use hx_index::{Index, RefreshStats};
use hx_sandbox::LocalSandbox;

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
    match idx.grep_candidates("unique_link_token") {
        hx_index::Candidates::Files(f) => {
            let f = paths(&f);
            assert!(f.contains(&"alias/x.rs") && f.contains(&"link.rs"), "{f:?}");
            assert!(
                !f.iter().any(|p| p.contains("secret") || *p == "out.rs"),
                "{f:?}"
            );
        }
        hx_index::Candidates::FullScan => panic!("expected candidates"),
    }

    // A single-file reindex through the link agrees with the walk.
    std::fs::write(
        fx.path("real/x.rs"),
        "fn via_link() { changed_token_xyz(); }\n",
    )
    .unwrap();
    idx.reindex_file(Path::new("alias/x.rs")).await.unwrap();
    assert_ne!(
        tree.get(Path::new("alias/x.rs")).unwrap().hash,
        tree.get(Path::new("link.rs")).unwrap().hash
    );
    assert!(tree.get(Path::new("alias/x.rs")).is_some());
}
