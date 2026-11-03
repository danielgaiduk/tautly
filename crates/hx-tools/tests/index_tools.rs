use std::path::Path;
use std::sync::Arc;

use hx_index::Index;
use hx_provider::{Capabilities, EditFormat};
use hx_sandbox::{LocalSandbox, Sandbox};
use hx_tools::{EditTool, OutlineTool, ReadTool, RefsTool, SymbolTool, Tool, ToolCx, ToolResult};
use serde_json::{Value, json};
use tempfile::TempDir;

struct Env {
    _dir: TempDir,
    _db: TempDir,
    sb: LocalSandbox,
    index: Arc<Index>,
    cx: ToolCx,
}

const LIB: &str = r#"struct Counter { n: u32 }
impl Counter {
    fn bump(&mut self) -> u32 {
        self.n += 1;
        self.n
    }
}
fn helper(x: u8) -> u8 {
    x + 1
}
"#;

async fn env_with(files: &[(&str, &str)], ready: bool, format: EditFormat) -> Env {
    let dir = TempDir::new().unwrap();
    let db = TempDir::new().unwrap();
    let sb = LocalSandbox::new(dir.path().to_path_buf()).unwrap();
    for (rel, content) in files {
        sb.write_atomic(Path::new(rel), content.as_bytes(), None)
            .await
            .unwrap();
    }
    let index_sb = Arc::new(LocalSandbox::new(dir.path().to_path_buf()).unwrap());
    let index = Arc::new(Index::open(&db.path().join("index.db"), index_sb).unwrap());
    if ready {
        index.refresh().await.unwrap();
        assert!(index.is_ready());
    }
    let cx = ToolCx::new(format, dir.path().join(".spill"))
        .with_index(index.clone())
        .with_turn(1);
    Env {
        _dir: dir,
        _db: db,
        sb,
        index,
        cx,
    }
}

async fn env(files: &[(&str, &str)], ready: bool) -> Env {
    env_with(files, ready, EditFormat::StrReplace).await
}

async fn run(e: &Env, tool: &dyn Tool, args: Value) -> ToolResult {
    tool.run(args, &e.sb, &e.cx).await
}

#[tokio::test]
async fn outline_lists_signatures_and_ranges_without_bodies() {
    let e = env(&[("src.rs", LIB)], true).await;
    let r = run(&e, &OutlineTool, json!({"path": "src.rs"})).await;
    assert!(!r.is_error, "{}", r.output);
    let lines: Vec<&str> = r.output.lines().collect();
    assert_eq!(
        lines,
        [
            "L1-1 type struct Counter",
            "  L3-6 method fn bump(&mut self) -> u32",
            "L8-10 function fn helper(x: u8) -> u8",
        ],
        "{}",
        r.output
    );
    assert!(!r.output.contains("self.n += 1"));
    assert!(!r.output.contains("x + 1"));
}

#[tokio::test]
async fn symbol_returns_source_with_path_and_lines() {
    let e = env(&[("src.rs", LIB)], true).await;
    let r = run(&e, &SymbolTool, json!({"name": "helper"})).await;
    assert!(!r.is_error, "{}", r.output);
    let (header, body) = r.output.split_once('\n').unwrap();
    assert_eq!(header, "src.rs:L8-10 function fn helper(x: u8) -> u8");
    assert_eq!(body, "fn helper(x: u8) -> u8 {\n    x + 1\n}");
}

#[tokio::test]
async fn symbol_with_multiple_matches_lists_locations_only() {
    let e = env(
        &[
            ("a.rs", "fn dup() {\n    secret_a();\n}\n"),
            ("b.rs", "fn other() {}\nfn dup() {\n    secret_b();\n}\n"),
        ],
        true,
    )
    .await;
    let r = run(&e, &SymbolTool, json!({"name": "dup"})).await;
    assert!(!r.is_error, "{}", r.output);
    assert!(r.output.contains("2 definitions of `dup`"), "{}", r.output);
    assert!(r.output.contains("a.rs:L1-3 function fn dup()"));
    assert!(r.output.contains("b.rs:L2-4 function fn dup()"));
    assert!(!r.output.contains("secret_a") && !r.output.contains("secret_b"));
}

#[tokio::test]
async fn symbol_kind_filter_narrows() {
    let e = env(
        &[(
            "a.rs",
            "struct Item {\n    id: u8,\n}\nfn Item() {\n    1;\n}\n",
        )],
        true,
    )
    .await;
    let r = run(&e, &SymbolTool, json!({"name": "Item", "kind": "type"})).await;
    assert!(!r.is_error, "{}", r.output);
    assert!(r.output.starts_with("a.rs:L1-3 type "), "{}", r.output);
    assert!(r.output.contains("id: u8"));
    let r = run(&e, &SymbolTool, json!({"name": "Item", "kind": "function"})).await;
    assert!(r.output.starts_with("a.rs:L4-6 function "), "{}", r.output);
    let r = run(&e, &SymbolTool, json!({"name": "Item", "kind": "module"})).await;
    assert!(!r.is_error);
    assert!(r.output.contains("no definition"), "{}", r.output);
}

fn calls(n: usize) -> String {
    let mut s = String::from("fn target() {}\nfn caller() {\n");
    for i in 0..n {
        s.push_str(&format!("    target(); // call {i}\n"));
    }
    s.push_str("}\n");
    s
}

#[tokio::test]
async fn refs_lists_path_line_context_and_respects_limit() {
    let e = env(
        &[
            ("calls.rs", &calls(60)),
            ("other.rs", "fn x() { target(); }\n"),
        ],
        true,
    )
    .await;
    let r = run(&e, &RefsTool, json!({"name": "target"})).await;
    assert!(!r.is_error, "{}", r.output);
    assert!(
        r.output.starts_with(
            "refs to `target` (name-based, may include unrelated identifiers with the same name):\n"
        ),
        "{}",
        r.output
    );
    assert!(r.output.contains("\ncalls.rs:3: target(); // call 0\n"));
    let hits = r.output.lines().filter(|l| l.contains(".rs:")).count();
    assert_eq!(hits, 50, "{}", r.output);
    assert!(
        r.output
            .ends_with("… more results truncated; narrow with path"),
        "{}",
        r.output
    );

    let r = run(&e, &RefsTool, json!({"name": "target", "limit": 3})).await;
    assert_eq!(r.output.lines().filter(|l| l.contains(".rs:")).count(), 3);
    assert!(r.output.contains("narrow with path"));

    // Narrowed by path, everything fits.
    let r = run(&e, &RefsTool, json!({"name": "target", "path": "other.rs"})).await;
    assert!(
        r.output.contains("other.rs:1: fn x() { target(); }"),
        "{}",
        r.output
    );
    assert!(!r.output.contains("calls.rs"));
    assert!(!r.output.contains("truncated"));

    let r = run(&e, &RefsTool, json!({"name": "nothing_here"})).await;
    assert!(!r.is_error);
    assert!(r.output.contains("no references"), "{}", r.output);
}

#[test]
fn refs_description_says_name_based() {
    let spec = RefsTool.spec(&Capabilities::conservative());
    assert!(spec.description.contains("name-based"));
    assert!(
        spec.description
            .contains("may include unrelated identifiers with the same name")
    );
}

#[tokio::test]
async fn fallback_while_index_not_ready() {
    let e = env(
        &[("src.rs", LIB), ("use.rs", "fn user() { helper(1); }\n")],
        false,
    )
    .await;
    assert!(!e.index.is_ready());

    let r = run(&e, &OutlineTool, json!({"path": "src.rs"})).await;
    assert!(!r.is_error, "{}", r.output);
    assert!(r.output.starts_with("(index building)"), "{}", r.output);
    assert!(r.output.contains("L8-10 function fn helper(x: u8) -> u8"));
    assert!(r.output.contains("  L3-6 method fn bump(&mut self) -> u32"));

    let r = run(&e, &SymbolTool, json!({"name": "helper"})).await;
    assert!(!r.is_error, "{}", r.output);
    assert!(r.output.starts_with("(index building)"), "{}", r.output);
    assert!(r.output.contains("src.rs:8:fn helper(x: u8) -> u8 {"));
    assert!(r.output.contains("use.rs:1:fn user() { helper(1); }"));

    let r = run(&e, &RefsTool, json!({"name": "helper"})).await;
    assert!(!r.is_error, "{}", r.output);
    assert!(r.output.starts_with("(index building)"), "{}", r.output);
    assert!(r.output.contains("use.rs:1:"));
}

#[tokio::test]
async fn edit_then_symbol_sees_new_body() {
    let e = env(&[("src.rs", LIB)], true).await;
    let read = run(&e, &ReadTool, json!({"path": "src.rs"})).await;
    assert!(!read.is_error);
    let r = run(
        &e,
        &EditTool,
        json!({"path": "src.rs", "old_text": "    x + 1\n}", "new_text": "    let y = x + 2;\n    y\n}\nfn added() {}"}),
    )
    .await;
    assert!(!r.is_error, "{}", r.output);
    let r = run(&e, &SymbolTool, json!({"name": "helper"})).await;
    assert!(r.output.contains("let y = x + 2;"), "{}", r.output);
    assert!(r.output.contains("L8-11"), "{}", r.output);
    let r = run(&e, &SymbolTool, json!({"name": "added"})).await;
    assert!(r.output.contains("fn added() {}"), "{}", r.output);
}

#[tokio::test]
async fn hashline_and_patch_edits_reindex_too() {
    let e = env_with(&[("src.rs", LIB)], true, EditFormat::Patch).await;
    run(&e, &ReadTool, json!({"path": "src.rs"})).await;
    let patch = "*** Begin Patch\n*** Update File: src.rs\n@@\n fn helper(x: u8) -> u8 {\n-    x + 1\n+    x + 7\n }\n*** Add File: new.rs\n+fn fresh() {}\n*** End Patch";
    let r = run(&e, &EditTool, json!({"patch": patch})).await;
    assert!(!r.is_error, "{}", r.output);
    let r = run(&e, &SymbolTool, json!({"name": "helper"})).await;
    assert!(r.output.contains("x + 7"), "{}", r.output);
    let r = run(&e, &SymbolTool, json!({"name": "fresh"})).await;
    assert!(r.output.contains("new.rs:L1-1"), "{}", r.output);

    let e = env_with(&[("src.rs", LIB)], true, EditFormat::Hashline).await;
    let read = run(
        &e,
        &ReadTool,
        json!({"path": "src.rs", "start_line": 9, "end_line": 9}),
    )
    .await;
    assert!(!read.is_error, "{}", read.output);
    let anchor = read.output.lines().find(|l| l.contains("x + 1")).unwrap();
    let anchor = anchor.split('|').next().unwrap().trim().to_string();
    let r = run(
        &e,
        &EditTool,
        json!({"path": "src.rs", "ops": [{"op": "replace", "start": anchor, "end": anchor, "lines": ["    x + 9"]}]}),
    )
    .await;
    assert!(!r.is_error, "{}", r.output);
    let r = run(&e, &SymbolTool, json!({"name": "helper"})).await;
    assert!(r.output.contains("x + 9"), "{}", r.output);
}

#[tokio::test]
async fn edit_without_index_is_unaffected() {
    let dir = TempDir::new().unwrap();
    let sb = LocalSandbox::new(dir.path().to_path_buf()).unwrap();
    sb.write_atomic(Path::new("a.rs"), b"fn a() {}\n", None)
        .await
        .unwrap();
    let cx = ToolCx::new(EditFormat::StrReplace, dir.path().join(".spill")).with_turn(3);
    assert_eq!(cx.turn(), 3);
    assert!(cx.index().is_none());
    ReadTool.run(json!({"path": "a.rs"}), &sb, &cx).await;
    let r = EditTool
        .run(
            json!({"path": "a.rs", "old_text": "fn a() {}", "new_text": "fn b() {}"}),
            &sb,
            &cx,
        )
        .await;
    assert!(!r.is_error && !r.output.contains("index"), "{}", r.output);
    assert_eq!(sb.read(Path::new("a.rs")).await.unwrap(), b"fn b() {}\n");
}

#[tokio::test]
async fn refs_path_filter_is_not_defeated_by_many_earlier_hits() {
    // More hits in an earlier file than any fetch-then-filter window could hold.
    let mut early = String::new();
    for i in 0..10_200 {
        early.push_str(&format!("fn f{i}() {{ target(); }}\n"));
    }
    let e = env(
        &[
            ("a_early.rs", &early),
            ("z/late.rs", "fn late() { target(); }\n"),
        ],
        true,
    )
    .await;
    let r = run(&e, &RefsTool, json!({"name": "target", "path": "./z"})).await;
    assert!(!r.is_error, "{}", r.output);
    assert!(
        r.output.contains("z/late.rs:1: fn late() { target(); }"),
        "{}",
        r.output
    );
    assert!(!r.output.contains("a_early"), "{}", r.output);
    assert!(!r.output.contains("truncated"), "{}", r.output);
}

#[tokio::test]
async fn symbol_rejects_non_utf8_source() {
    let e = env(&[("a.rs", "fn lone() {}\n")], true).await;
    // Indexed as text, then replaced by bytes that are not UTF-8.
    e.sb.write_atomic(Path::new("a.rs"), b"fn lone() {}\n\xff\xfe\n", None)
        .await
        .unwrap();
    let r = run(&e, &SymbolTool, json!({"name": "lone"})).await;
    assert!(r.is_error, "{}", r.output);
    assert!(r.output.contains("not UTF-8"), "{}", r.output);
}
