use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use hx_index::SymbolKind::{Constant, Function, Method, Module, Type};
use hx_index::{Extracted, Index, Lang, RefreshStats, SymbolKind, extract};
use hx_sandbox::LocalSandbox;

fn names_kinds(e: &Extracted) -> Vec<(&str, SymbolKind)> {
    e.symbols
        .iter()
        .map(|s| (s.name.as_str(), s.kind))
        .collect()
}

fn touch_write(path: PathBuf, content: &str) {
    std::fs::write(&path, content).unwrap();
    // Guarantee a later mtime even on coarse-grained filesystems.
    let f = std::fs::File::options().write(true).open(&path).unwrap();
    f.set_modified(SystemTime::now() + Duration::from_secs(5))
        .unwrap();
}

fn write_all(root: &Path, files: &[(&str, &str)]) {
    for (rel, content) in files {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }
}

fn open_at(root: &Path, db: &Path) -> Index {
    let sb = Arc::new(LocalSandbox::new(root.to_path_buf()).unwrap());
    Index::open(db, sb).unwrap()
}

fn index_with(files: &[(&str, &str)]) -> (tempfile::TempDir, Index) {
    let t = tempfile::tempdir().unwrap();
    write_all(t.path(), files);
    let idx = open_at(t.path(), &t.path().join(".index.db"));
    (t, idx)
}

// ---------------------------------------------------------------- extraction

#[test]
fn extracts_rust_items_with_parents() {
    let e = extract(
        Lang::Rust,
        "struct A;\nimpl A {\n    fn go(&self) -> u8 { 1 }\n}\nconst N: u8 = 3;\nfn main() {}\n",
    );
    assert_eq!(
        names_kinds(&e),
        [
            ("A", Type),
            ("go", Method),
            ("N", Constant),
            ("main", Function)
        ]
    );
    assert_eq!(e.symbols[1].parent.as_deref(), Some("A"));
    assert_eq!(e.symbols[1].signature, "fn go(&self) -> u8");
    assert_eq!(e.symbols[1].start_line, 3);
    assert_eq!(e.symbols[1].end_line, 3);
    assert!(e.symbols.iter().all(|s| s.path.as_os_str().is_empty()));
}

#[test]
fn rust_traits_modules_and_nested_fns() {
    let src = "mod m {\n    pub fn f() {\n        fn inner() {}\n    }\n}\ntrait T {\n    fn req(&self);\n    fn prov(&self) {}\n}\nimpl<X> T for Wrapper<X> {\n    fn req(&self) {}\n}\nenum E { A }\n";
    let e = extract(Lang::Rust, src);
    assert_eq!(
        names_kinds(&e),
        [
            ("m", Module),
            ("f", Function),
            ("inner", Function),
            ("T", Type),
            ("req", Method),
            ("prov", Method),
            ("req", Method),
            ("E", Type),
        ]
    );
    assert_eq!(e.symbols[4].parent.as_deref(), Some("T"));
    assert_eq!(e.symbols[6].parent.as_deref(), Some("Wrapper"));
    assert_eq!(e.symbols[2].parent, None);
    assert_eq!(e.symbols[0].start_line, 1);
    assert_eq!(e.symbols[0].end_line, 5);
}

#[test]
fn signature_is_whitespace_collapsed_and_capped_at_200_chars() {
    let e = extract(
        Lang::Rust,
        "pub fn long(\n    a: u8,\n    b: u8,\n) -> u8 {\n    a + b\n}\n",
    );
    assert_eq!(e.symbols[0].signature, "pub fn long( a: u8, b: u8, ) -> u8");
    let params: String = (0..100).map(|i| format!("p{i}: u8, ")).collect();
    let e = extract(Lang::Rust, &format!("fn big({params}) {{}}"));
    assert_eq!(e.symbols[0].signature.chars().count(), 200);
}

#[test]
fn extracts_python_ts_go_definitions() {
    let py = extract(
        Lang::Python,
        "MAX = 3\nlower = 4\nclass C:\n    def m(self):\n        pass\n\n@deco\ndef f(x):\n    return x\n",
    );
    assert_eq!(
        names_kinds(&py),
        [
            ("MAX", Constant),
            ("C", Type),
            ("m", Method),
            ("f", Function)
        ]
    );
    assert_eq!(py.symbols[2].parent.as_deref(), Some("C"));
    assert_eq!(py.symbols[3].signature, "def f(x):");

    let ts_src = "export function f(a: number): number { return a; }\nclass K {\n  go(x: string): void {}\n}\ninterface I { a: number }\ntype Alias = string;\nenum Color { Red }\nconst LIMIT = 3;\nconst arrow = (a: number) => a + 1;\nlet mutable = 1;\n";
    for lang in [Lang::TypeScript, Lang::Tsx] {
        let ts = extract(lang, ts_src);
        assert_eq!(
            names_kinds(&ts),
            [
                ("f", Function),
                ("K", Type),
                ("go", Method),
                ("I", Type),
                ("Alias", Type),
                ("Color", Type),
                ("LIMIT", Constant),
                ("arrow", Function),
            ],
            "{lang:?}"
        );
        assert_eq!(ts.symbols[2].parent.as_deref(), Some("K"));
        assert_eq!(ts.symbols[0].signature, "function f(a: number): number");
        assert_eq!(ts.symbols[7].signature, "const arrow = (a: number) =>");
    }

    let js = extract(
        Lang::JavaScript,
        "function f() {}\nclass K { m() {} }\nconst X = 1;\nconst obj = { m() {} };\n",
    );
    assert_eq!(
        names_kinds(&js),
        [
            ("f", Function),
            ("K", Type),
            ("m", Method),
            ("X", Constant),
            ("obj", Constant)
        ]
    );

    let go = extract(
        Lang::Go,
        "package p\n\nconst Max = 3\n\ntype S struct{}\n\nfunc (s *S) Do(x int) int { return x }\n\nfunc Free() {}\n",
    );
    assert_eq!(
        names_kinds(&go),
        [
            ("Max", Constant),
            ("S", Type),
            ("Do", Method),
            ("Free", Function)
        ]
    );
    assert_eq!(go.symbols[2].parent.as_deref(), Some("S"));
    assert_eq!(go.symbols[2].signature, "func (s *S) Do(x int) int");
}

#[test]
fn config_formats_have_no_symbols_or_refs() {
    for (lang, src) in [
        (Lang::Json, "{\"a\": 1}"),
        (Lang::Toml, "a = 1\n[t]\nb = 2\n"),
        (Lang::Yaml, "a: 1\n"),
    ] {
        assert_eq!(extract(lang, src), Extracted::default(), "{lang:?}");
    }
}

#[test]
fn refs_exclude_definition_sites() {
    let src = "fn helper() {}\nfn main() {\n    helper();\n    let x = helper;\n}\n";
    let e = extract(Lang::Rust, src);
    let helper: Vec<u32> = e
        .refs
        .iter()
        .filter(|(_, n)| n == "helper")
        .map(|(l, _)| *l)
        .collect();
    assert_eq!(helper, [3, 4]);
    assert!(!e.refs.iter().any(|(_, n)| n == "main"));
    // A name used twice on one line is reported once for that line.
    let e = extract(Lang::Rust, "fn a() { b(); b(); }\nfn b() {}\n");
    assert_eq!(
        e.refs.iter().filter(|(_, n)| n == "b").count(),
        1,
        "{:?}",
        e.refs
    );
}

#[test]
fn symbol_hash_changes_only_when_that_symbol_changes() {
    let a = extract(Lang::Rust, "fn one() { 1 }\nfn two() { 2 }\n");
    let b = extract(Lang::Rust, "\n\nfn one() { 1 }\nfn two() { 3 }\n");
    assert_eq!(a.symbols[0].hash, b.symbols[0].hash, "moved but unchanged");
    assert_ne!(a.symbols[1].hash, b.symbols[1].hash, "body changed");
    assert_ne!(a.symbols[0].hash, a.symbols[1].hash);
}

// ------------------------------------------------------------------- index

#[tokio::test]
async fn refresh_reparses_only_changed_files() {
    let (t, idx) = index_with(&[("a.rs", "fn a(){}"), ("b.rs", "fn b(){}")]);
    assert_eq!(
        idx.refresh().await.unwrap(),
        RefreshStats {
            parsed: 2,
            removed: 0,
            unchanged: 0
        }
    );
    touch_write(t.path().join("a.rs"), "fn a2(){}");
    assert_eq!(
        idx.refresh().await.unwrap(),
        RefreshStats {
            parsed: 1,
            removed: 0,
            unchanged: 1
        }
    );
    assert_eq!(idx.outline(Path::new("a.rs")).unwrap()[0].name, "a2");
}

#[tokio::test]
async fn touched_but_identical_file_is_not_reparsed() {
    let (t, idx) = index_with(&[("a.rs", "fn a(){}")]);
    idx.refresh().await.unwrap();
    touch_write(t.path().join("a.rs"), "fn a(){}");
    assert_eq!(
        idx.refresh().await.unwrap(),
        RefreshStats {
            parsed: 0,
            removed: 0,
            unchanged: 1
        }
    );
}

#[tokio::test]
async fn deleted_files_are_removed() {
    let (t, idx) = index_with(&[("a.rs", "fn a(){ b(); }"), ("b.rs", "fn b(){}")]);
    idx.refresh().await.unwrap();
    std::fs::remove_file(t.path().join("a.rs")).unwrap();
    assert_eq!(
        idx.refresh().await.unwrap(),
        RefreshStats {
            parsed: 0,
            removed: 1,
            unchanged: 1
        }
    );
    assert!(idx.outline(Path::new("a.rs")).unwrap().is_empty());
    assert!(idx.find_symbol("a", None).unwrap().is_empty());
    assert!(idx.refs("b", 10).unwrap().is_empty());
    assert_eq!(idx.file_hash(Path::new("a.rs")), None);
    let s = idx.stats();
    assert_eq!((s.files, s.symbols), (1, 1));
}

#[tokio::test]
async fn gitignored_files_skipped() {
    let (_t, idx) = index_with(&[
        (".gitignore", "ignored/\n*.gen.rs\n"),
        ("src/keep.rs", "fn keep(){}"),
        ("ignored/skip.rs", "fn skip_dir(){}"),
        ("src/x.gen.rs", "fn skip_glob(){}"),
    ]);
    idx.refresh().await.unwrap();
    assert_eq!(idx.stats().files, 1);
    assert_eq!(idx.find_symbol("keep", None).unwrap().len(), 1);
    assert!(idx.find_symbol("skip_dir", None).unwrap().is_empty());
    assert!(idx.find_symbol("skip_glob", None).unwrap().is_empty());
}

#[tokio::test]
async fn oversize_and_non_utf8_files_are_tracked_without_symbols() {
    let big = format!("fn big(){{}}\n// {}\n", "x".repeat(1024 * 1024));
    let (t, idx) = index_with(&[("big.rs", &big), ("ok.rs", "fn ok(){}")]);
    std::fs::write(t.path().join("bin.rs"), b"fn bin(){}\n// \xff\xfe\n").unwrap();
    idx.refresh().await.unwrap();
    // All three are rows (the tree knows them); only the readable one is parsed.
    assert_eq!(idx.stats().files, 3);
    assert_eq!(idx.stats().symbols, 1);
    assert!(idx.find_symbol("big", None).unwrap().is_empty());
    assert!(idx.find_symbol("bin", None).unwrap().is_empty());
    assert_eq!(idx.find_symbol("ok", None).unwrap().len(), 1);
}

#[tokio::test]
async fn reindex_file_updates_outline() {
    let (t, idx) = index_with(&[("a.rs", "fn a(){}\n")]);
    idx.refresh().await.unwrap();
    let before = idx.file_hash(Path::new("a.rs")).unwrap();
    std::fs::write(t.path().join("a.rs"), "fn a(){}\nfn extra(){}\n").unwrap();
    idx.reindex_file(Path::new("a.rs")).await.unwrap();
    let names: Vec<_> = idx
        .outline(Path::new("a.rs"))
        .unwrap()
        .into_iter()
        .map(|s| s.name)
        .collect();
    assert_eq!(names, ["a", "extra"]);
    assert_ne!(idx.file_hash(Path::new("a.rs")).unwrap(), before);
    // Absolute and ./-prefixed paths resolve to the same file.
    assert_eq!(idx.outline(Path::new("./a.rs")).unwrap().len(), 2);
    assert_eq!(idx.outline(&t.path().join("a.rs")).unwrap().len(), 2);
    // A file created after the last refresh can be indexed on its own.
    write_all(t.path(), &[("new.py", "def made(): pass\n")]);
    idx.reindex_file(Path::new("new.py")).await.unwrap();
    assert_eq!(idx.find_symbol("made", None).unwrap().len(), 1);
    // A deleted file is dropped; one without a grammar is tracked but has no symbols.
    std::fs::remove_file(t.path().join("a.rs")).unwrap();
    idx.reindex_file(Path::new("a.rs")).await.unwrap();
    assert!(idx.outline(Path::new("a.rs")).unwrap().is_empty());
    write_all(t.path(), &[("notes.txt", "fn nope(){}")]);
    idx.reindex_file(Path::new("notes.txt")).await.unwrap();
    assert_eq!(idx.stats().files, 2);
    assert!(idx.find_symbol("nope", None).unwrap().is_empty());
    assert!(idx.file_hash(Path::new("notes.txt")).is_some());
}

#[tokio::test]
async fn reindex_file_is_fast() {
    let src: String = (0..200)
        .map(|i| format!("fn f{i}() {{ g{i}(); }}\n"))
        .collect();
    let (t, idx) = index_with(&[("a.rs", &src)]);
    idx.refresh().await.unwrap();
    std::fs::write(t.path().join("a.rs"), format!("{src}fn more() {{}}\n")).unwrap();
    let start = std::time::Instant::now();
    idx.reindex_file(Path::new("a.rs")).await.unwrap();
    assert!(
        start.elapsed() < Duration::from_millis(50),
        "{:?}",
        start.elapsed()
    );
}

#[tokio::test]
async fn find_symbol_refs_and_hashes() {
    let (_t, idx) = index_with(&[
        (
            "src/a.rs",
            "pub struct Widget;\nimpl Widget {\n    pub fn new() -> Widget { Widget }\n}\n",
        ),
        (
            "src/b.rs",
            "fn make() {\n    let w = Widget::new();   \n}\nfn Widget() {}\n",
        ),
    ]);
    idx.refresh().await.unwrap();

    let all = idx.find_symbol("Widget", None).unwrap();
    assert_eq!(
        all.iter()
            .map(|s| (s.path.as_path(), s.kind))
            .collect::<Vec<_>>(),
        [
            (Path::new("src/a.rs"), Type),
            (Path::new("src/b.rs"), Function)
        ]
    );
    let only_types = idx.find_symbol("Widget", Some(Type)).unwrap();
    assert_eq!(only_types.len(), 1);
    assert_eq!(only_types[0].path, Path::new("src/a.rs"));
    let new = idx.find_symbol("new", Some(Method)).unwrap();
    assert_eq!(new[0].parent.as_deref(), Some("Widget"));

    let refs = idx.refs("Widget", 10).unwrap();
    let got: Vec<_> = refs
        .iter()
        .map(|r| (r.path.to_str().unwrap(), r.line, r.context.as_str()))
        .collect();
    assert_eq!(
        got,
        [
            ("src/a.rs", 2, "impl Widget {"),
            ("src/a.rs", 3, "pub fn new() -> Widget { Widget }"),
            ("src/b.rs", 2, "let w = Widget::new();"),
        ]
    );
    assert_eq!(idx.refs("Widget", 2).unwrap().len(), 2);
    assert!(idx.refs("Nope", 5).unwrap().is_empty());

    let fh = idx.file_hash(Path::new("src/a.rs")).unwrap();
    assert_eq!(
        fh,
        blake3::hash(
            b"pub struct Widget;\nimpl Widget {\n    pub fn new() -> Widget { Widget }\n}\n"
        )
        .to_hex()
        .to_string()
    );
    assert_eq!(idx.file_hash(Path::new("missing.rs")), None);
    assert_eq!(
        idx.symbol_hash(Path::new("src/a.rs"), "new").unwrap(),
        blake3::hash(b"pub fn new() -> Widget { Widget }")
            .to_hex()
            .to_string()
    );
    assert_eq!(idx.symbol_hash(Path::new("src/a.rs"), "nothing"), None);

    let s = idx.stats();
    assert_eq!((s.files, s.symbols), (2, 4));
    assert!(s.refs >= 3);
}

#[tokio::test]
async fn ref_context_is_trimmed_and_capped_at_200_chars() {
    let long = format!("fn f() {{ target(); }} // {}", "z".repeat(400));
    let (_t, idx) = index_with(&[("a.rs", &format!("    {long}\nfn target() {{}}\n"))]);
    idx.refresh().await.unwrap();
    let r = idx.refs("target", 5).unwrap();
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].context.chars().count(), 200);
    assert!(r[0].context.starts_with("fn f()"));
}

#[tokio::test]
async fn config_files_are_tracked_by_hash_only() {
    let (_t, idx) = index_with(&[("Cargo.toml", "a = 1\n"), ("c.json", "{}")]);
    idx.refresh().await.unwrap();
    let s = idx.stats();
    assert_eq!((s.files, s.symbols, s.refs), (2, 0, 0));
    assert!(idx.file_hash(Path::new("Cargo.toml")).is_some());
}

#[tokio::test]
async fn two_handles_share_db_in_wal_mode() {
    let t = tempfile::tempdir().unwrap();
    write_all(t.path(), &[("a.rs", "fn shared(){}")]);
    // The database lives outside the workspace, as in production; inside, its own files
    // would be workspace files.
    let db_dir = tempfile::tempdir().unwrap();
    let db = db_dir.path().join("nested/dir/index.db");
    let one = open_at(t.path(), &db);
    let two = open_at(t.path(), &db);
    one.refresh().await.unwrap();
    assert_eq!(two.find_symbol("shared", None).unwrap().len(), 1);
    // The second handle sees the first one's work as unchanged files.
    assert_eq!(
        two.refresh().await.unwrap(),
        RefreshStats {
            parsed: 0,
            removed: 0,
            unchanged: 1
        }
    );
    let conn = rusqlite::Connection::open(&db).unwrap();
    let mode: String = conn
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
    let version: i64 = conn
        .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, 2);
}

#[tokio::test]
async fn index_persists_across_open() {
    let t = tempfile::tempdir().unwrap();
    write_all(t.path(), &[("a.rs", "fn a(){}"), ("b.rs", "fn b(){}")]);
    let db = t.path().join(".index.db");
    open_at(t.path(), &db).refresh().await.unwrap();
    let again = open_at(t.path(), &db);
    assert_eq!(again.stats().files, 2);
    assert_eq!(
        again.refresh().await.unwrap(),
        RefreshStats {
            parsed: 0,
            removed: 0,
            unchanged: 2
        }
    );
}

#[tokio::test]
async fn is_ready_false_until_first_refresh_completes() {
    let (_t, idx) = index_with(&[("a.rs", "fn a(){}")]);
    assert!(!idx.is_ready());
    idx.refresh().await.unwrap();
    assert!(idx.is_ready());
}

#[tokio::test]
async fn background_refresh_builds_the_index_and_marks_ready() {
    let (_t, idx) = index_with(&[("a.rs", "fn a(){}")]);
    let idx = Arc::new(idx);
    idx.clone().spawn_background_refresh().await.unwrap();
    assert!(idx.is_ready());
    assert_eq!(idx.find_symbol("a", None).unwrap().len(), 1);
}

#[test]
fn index_is_send_and_sync() {
    fn check<T: Send + Sync>() {}
    check::<Index>();
}

// ------------------------------------------------------------ review round 1

#[tokio::test]
async fn reindex_file_skips_ignored_and_hidden_files() {
    let (t, idx) = index_with(&[
        (".gitignore", "ignored/\n*.gen.rs\n"),
        ("src/keep.rs", "fn keep(){}"),
        ("ignored/skip.rs", "fn a(){}"),
        ("src/x.gen.rs", "fn b(){}"),
        (".hidden/h.rs", "fn c(){}"),
    ]);
    idx.refresh().await.unwrap();
    assert_eq!(idx.stats().files, 1);
    for rel in ["ignored/skip.rs", "src/x.gen.rs", ".hidden/h.rs"] {
        let p = t.path().join(rel);
        std::fs::write(&p, "fn edited(){}\n").unwrap();
        idx.reindex_file(Path::new(rel)).await.unwrap();
        assert!(idx.outline(Path::new(rel)).unwrap().is_empty(), "{rel}");
        assert_eq!(idx.stats().files, 1, "{rel}");
    }
    // A file that was indexed and becomes ignored is dropped by the next reindex.
    std::fs::write(
        t.path().join(".gitignore"),
        "ignored/\n*.gen.rs\nsrc/keep.rs\n",
    )
    .unwrap();
    idx.reindex_file(Path::new("src/keep.rs")).await.unwrap();
    assert_eq!(idx.stats().files, 0);
    // Admitted nested files are still indexed.
    write_all(t.path(), &[("src/deep/x.rs", "fn deep(){}")]);
    idx.reindex_file(Path::new("src/deep/x.rs")).await.unwrap();
    assert_eq!(idx.find_symbol("deep", None).unwrap().len(), 1);
}

/// Delegates to a real sandbox but can be told to fail the directory walk.
struct FlakySandbox {
    inner: LocalSandbox,
    fail_walk: std::sync::atomic::AtomicBool,
}

#[async_trait::async_trait]
impl hx_sandbox::Sandbox for FlakySandbox {
    fn root(&self) -> &Path {
        self.inner.root()
    }
    fn resolve(&self, p: &Path) -> Result<PathBuf, hx_sandbox::SandboxError> {
        self.inner.resolve(p)
    }
    async fn read(&self, p: &Path) -> Result<Vec<u8>, hx_sandbox::SandboxError> {
        self.inner.read(p).await
    }
    async fn metadata(&self, p: &Path) -> Result<hx_sandbox::FileMeta, hx_sandbox::SandboxError> {
        self.inner.metadata(p).await
    }
    async fn write_atomic(
        &self,
        p: &Path,
        bytes: &[u8],
        mode: Option<u32>,
    ) -> Result<(), hx_sandbox::SandboxError> {
        self.inner.write_atomic(p, bytes, mode).await
    }
    async fn remove_file(&self, p: &Path) -> Result<(), hx_sandbox::SandboxError> {
        self.inner.remove_file(p).await
    }
    async fn spill(&self, bytes: &[u8]) -> Result<PathBuf, hx_sandbox::SandboxError> {
        self.inner.spill(bytes).await
    }
    async fn spawn(
        &self,
        spec: hx_sandbox::SpawnSpec,
    ) -> Result<hx_sandbox::Child, hx_sandbox::SandboxError> {
        self.inner.spawn(spec).await
    }
    fn walker(&self, base: &Path) -> Result<ignore::WalkBuilder, hx_sandbox::SandboxError> {
        if self.fail_walk.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(hx_sandbox::SandboxError::Denied("walk refused".into()));
        }
        self.inner.walker(base)
    }
    async fn http_get(
        &self,
        url: &hx_sandbox::Url,
    ) -> Result<hx_sandbox::HttpResponse, hx_sandbox::SandboxError> {
        self.inner.http_get(url).await
    }
}

#[tokio::test]
async fn background_refresh_failure_is_recorded_and_cleared_on_success() {
    use std::sync::atomic::Ordering::SeqCst;
    let t = tempfile::tempdir().unwrap();
    write_all(t.path(), &[("a.rs", "fn a(){}")]);
    let sb = Arc::new(FlakySandbox {
        inner: LocalSandbox::new(t.path().to_path_buf()).unwrap(),
        fail_walk: std::sync::atomic::AtomicBool::new(true),
    });
    let idx = Arc::new(Index::open(&t.path().join(".index.db"), sb.clone()).unwrap());
    assert_eq!(idx.last_error(), None);
    idx.clone().spawn_background_refresh().await.unwrap();
    assert!(!idx.is_ready());
    let err = idx.last_error().expect("failure recorded");
    assert!(err.contains("walk refused"), "{err}");
    assert!(idx.refresh().await.is_err());

    sb.fail_walk.store(false, SeqCst);
    idx.clone().spawn_background_refresh().await.unwrap();
    assert!(idx.is_ready());
    assert_eq!(idx.last_error(), None);
}

fn db_with_version(dir: &Path, version: i64) -> PathBuf {
    let db = dir.join("v.db");
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch("CREATE TABLE schema_version (version INTEGER NOT NULL)")
        .unwrap();
    conn.execute("INSERT INTO schema_version VALUES (?1)", [version])
        .unwrap();
    db
}

#[test]
fn newer_schema_is_refused() {
    let t = tempfile::tempdir().unwrap();
    let db = db_with_version(t.path(), 99);
    let sb = Arc::new(LocalSandbox::new(t.path().to_path_buf()).unwrap());
    let err = Index::open(&db, sb).err().expect("must refuse");
    assert!(
        matches!(err, hx_index::IndexError::SchemaTooNew { found: 99 }),
        "{err:?}"
    );
}

#[test]
fn unsupported_old_schema_is_refused_explicitly() {
    let t = tempfile::tempdir().unwrap();
    let db = db_with_version(t.path(), 0);
    let sb = Arc::new(LocalSandbox::new(t.path().to_path_buf()).unwrap());
    let err = Index::open(&db, sb).err().expect("must refuse");
    assert!(
        matches!(err, hx_index::IndexError::SchemaTooOld { found: 0 }),
        "{err:?}"
    );
    // The message names the oldest version a migration exists for, not the current one.
    assert!(
        err.to_string().contains("oldest migratable version (1)"),
        "{err}"
    );
}

#[tokio::test]
async fn refs_in_filters_by_path_prefix_in_sql() {
    let (_t, idx) = index_with(&[
        (
            "a/one.rs",
            "fn x() { target(); target(); }\nfn y() { target(); }\n",
        ),
        ("a/two.rs", "fn z() { target(); }\n"),
        ("ab.rs", "fn w() { target(); }\n"),
        ("b/deep/three.rs", "fn v() { target(); }\n"),
    ]);
    idx.refresh().await.unwrap();
    let paths = |p: Option<&str>, limit: usize| -> Vec<String> {
        idx.refs_in("target", p.map(Path::new), limit)
            .unwrap()
            .iter()
            .map(|r| r.path.to_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(paths(None, 10).len(), 5);
    // Directory prefixes match whole components: `a` does not match `ab.rs`.
    assert_eq!(paths(Some("a"), 10), ["a/one.rs", "a/one.rs", "a/two.rs"]);
    assert_eq!(
        paths(Some("./a/"), 10),
        ["a/one.rs", "a/one.rs", "a/two.rs"]
    );
    assert_eq!(paths(Some("a"), 2), ["a/one.rs", "a/one.rs"]);
    assert_eq!(paths(Some("b/deep"), 10), ["b/deep/three.rs"]);
    assert_eq!(paths(Some("ab.rs"), 10), ["ab.rs"]);
    assert_eq!(paths(Some("."), 10).len(), 5);
    assert!(paths(Some("nope"), 10).is_empty());
}
