use std::path::Path;

use hx_provider::EditFormat;
use hx_sandbox::{LocalSandbox, Sandbox};
use hx_tools::edit::syntax::new_errors;
use hx_tools::{EditTool, ReadTool, Tool, ToolCx, ToolResult};
use serde_json::json;
use tempfile::TempDir;

// ---------------------------------------------------------------- new_errors

const RS_OK: &str = "fn main() {\n    let x = 1;\n    run(x);\n}\n";
const RS_NO_BRACE: &str = "fn main() {\n    let x = 1;\n    run(x);\n";

#[test]
fn introduced_parse_error_reported() {
    let errs = new_errors(Path::new("a.rs"), RS_OK, RS_NO_BRACE).expect("rust is supported");
    assert!(!errs.is_empty());
    for (line, msg) in &errs {
        assert!((1..=5).contains(line), "line {line}");
        assert!(!msg.is_empty());
    }
}

#[test]
fn preexisting_error_not_reported() {
    let before = "fn broken( {\n    let x = 1;\n}\n\nfn ok() {}\n";
    // An unrelated edit further down leaves the broken line where it was.
    let after = "fn broken( {\n    let x = 1;\n}\n\nfn ok() { let y = 2; }\n";
    assert_eq!(
        new_errors(Path::new("a.rs"), before, after),
        Some(Vec::new())
    );
}

#[test]
fn clean_edit_reports_nothing() {
    assert_eq!(
        new_errors(Path::new("a.rs"), RS_OK, "fn main() {\n    run(2);\n}\n"),
        Some(Vec::new())
    );
}

#[test]
fn unsupported_extension_returns_none() {
    assert_eq!(new_errors(Path::new("a.xyz"), "", "}{"), None);
    assert_eq!(new_errors(Path::new("Makefile"), "", "}{"), None);
}

#[test]
fn supported_set_is_exactly_the_listed_extensions() {
    let supported = [
        ("a.rs", "fn a() {}\n"),
        ("a.ts", "const a: number = 1;\n"),
        ("a.tsx", "const a = <div>hi</div>;\n"),
        ("a.js", "const a = 1;\n"),
        ("a.py", "x = 1\n"),
        ("a.go", "package main\n"),
        ("a.json", "{\"a\": 1}\n"),
        ("a.toml", "a = 1\n"),
        ("a.yaml", "a: 1\n"),
        ("a.yml", "a: 1\n"),
    ];
    for (path, good) in supported {
        assert_eq!(
            new_errors(Path::new(path), "", good),
            Some(Vec::new()),
            "{path}"
        );
    }
    for path in [
        "a.md", "a.txt", "a.c", "a.cpp", "a.java", "a.jsx", "a.mjs", "a.html", "a",
    ] {
        assert_eq!(new_errors(Path::new(path), "", "x"), None, "{path}");
    }
}

#[test]
fn every_supported_language_detects_a_broken_file() {
    let broken = [
        ("a.rs", "fn a() {\n"),
        ("a.ts", "const a: number = ;\nfunction (\n"),
        ("a.tsx", "const a = <div>;\n"),
        ("a.js", "function (\n"),
        ("a.py", "def f(:\n    pass\n"),
        ("a.go", "package main\nfunc main( {\n"),
        ("a.json", "{\"a\": }\n"),
        ("a.toml", "a = \n[b\n"),
        ("a.yaml", "a: [1, 2\nb: {\n"),
    ];
    for (path, bad) in broken {
        let errs = new_errors(Path::new(path), "", bad).expect(path);
        assert!(!errs.is_empty(), "{path} should report an error");
    }
}

#[test]
fn extension_match_is_case_insensitive_and_uses_the_last_component() {
    assert!(new_errors(Path::new("dir.rs/a.RS"), "", "fn a() {}\n").is_some());
    assert_eq!(new_errors(Path::new("dir.rs/a"), "", "x"), None);
}

// ---------------------------------------------------------------- wired into edit

struct Env {
    dir: TempDir,
    sb: LocalSandbox,
    cx: ToolCx,
}

fn env(format: EditFormat) -> Env {
    let dir = TempDir::new().unwrap();
    let sb = LocalSandbox::new(dir.path().to_path_buf()).unwrap();
    let cx = ToolCx::new(format, dir.path().join(".spill")).with_turn(1);
    Env { dir, sb, cx }
}

async fn put(e: &Env, path: &str, text: &str) {
    e.sb.write_atomic(Path::new(path), text.as_bytes(), None)
        .await
        .unwrap();
}

async fn slurp(e: &Env, path: &str) -> String {
    String::from_utf8(e.sb.read(Path::new(path)).await.unwrap()).unwrap()
}

async fn read(e: &Env, path: &str) -> String {
    let r = ReadTool.run(json!({"path": path}), &e.sb, &e.cx).await;
    assert!(!r.is_error, "{}", r.output);
    r.output
}

async fn anchor(e: &Env, path: &str, n: usize) -> String {
    let out = read(e, path).await;
    let prefix = format!("{n}:");
    out.lines()
        .find(|l| l.starts_with(&prefix))
        .and_then(|l| l.split('|').next())
        .unwrap_or_else(|| panic!("no anchor for line {n} in:\n{out}"))
        .to_string()
}

fn assert_warns_and_keeps_edit(r: &ToolResult) {
    assert!(!r.is_error, "{}", r.output);
    assert!(
        r.output.contains("syntax warning: new parse errors"),
        "{}",
        r.output
    );
    assert!(r.output.contains("a.rs"), "{}", r.output);
    assert_eq!(r.effects.len(), 1);
}

#[tokio::test]
async fn edit_result_includes_syntax_warning_and_keeps_edit() {
    let e = env(EditFormat::StrReplace);
    put(&e, "a.rs", RS_OK).await;
    read(&e, "a.rs").await;
    let r = EditTool
        .run(
            json!({"path": "a.rs", "old_text": "    run(x);\n}\n", "new_text": "    run(x);\n"}),
            &e.sb,
            &e.cx,
        )
        .await;
    assert_warns_and_keeps_edit(&r);
    assert_eq!(slurp(&e, "a.rs").await, RS_NO_BRACE);
}

#[tokio::test]
async fn str_replace_clean_edit_has_no_warning() {
    let e = env(EditFormat::StrReplace);
    put(&e, "a.rs", RS_OK).await;
    read(&e, "a.rs").await;
    let r = EditTool
        .run(
            json!({"path": "a.rs", "old_text": "run(x)", "new_text": "run(x + 1)"}),
            &e.sb,
            &e.cx,
        )
        .await;
    assert!(!r.is_error);
    assert!(!r.output.contains("syntax warning"), "{}", r.output);
}

#[tokio::test]
async fn str_replace_unsupported_extension_has_no_warning() {
    let e = env(EditFormat::StrReplace);
    put(&e, "notes.txt", "a {\n").await;
    read(&e, "notes.txt").await;
    let r = EditTool
        .run(
            json!({"path": "notes.txt", "old_text": "a {", "new_text": "b {{"}),
            &e.sb,
            &e.cx,
        )
        .await;
    assert!(!r.is_error);
    assert!(!r.output.contains("syntax warning"), "{}", r.output);
}

#[tokio::test]
async fn patch_edit_includes_syntax_warning_and_keeps_edit() {
    let e = env(EditFormat::Patch);
    put(&e, "a.rs", RS_OK).await;
    read(&e, "a.rs").await;
    let patch = [
        "*** Begin Patch",
        "*** Update File: a.rs",
        "@@ fn main() {",
        "     run(x);",
        "-}",
        "*** End Patch",
        "",
    ]
    .join("\n");
    let r = EditTool.run(json!({ "patch": patch }), &e.sb, &e.cx).await;
    assert_warns_and_keeps_edit(&r);
    assert_eq!(slurp(&e, "a.rs").await, RS_NO_BRACE);
}

#[tokio::test]
async fn patch_added_file_with_errors_warns() {
    let e = env(EditFormat::Patch);
    let patch = [
        "*** Begin Patch",
        "*** Add File: a.rs",
        "+fn main() {",
        "*** End Patch",
        "",
    ]
    .join("\n");
    let r = EditTool.run(json!({ "patch": patch }), &e.sb, &e.cx).await;
    assert_warns_and_keeps_edit(&r);
    assert!(e.dir.path().join("a.rs").exists());
}

#[tokio::test]
async fn hashline_edit_includes_syntax_warning_and_keeps_edit() {
    let e = env(EditFormat::Hashline);
    put(&e, "a.rs", RS_OK).await;
    let a = anchor(&e, "a.rs", 4).await;
    let r = EditTool
        .run(
            json!({"path": "a.rs", "ops": [{"op": "delete", "start": a, "end": a}]}),
            &e.sb,
            &e.cx,
        )
        .await;
    assert_warns_and_keeps_edit(&r);
    assert_eq!(slurp(&e, "a.rs").await, RS_NO_BRACE);
}

#[tokio::test]
async fn warning_does_not_repeat_preexisting_errors() {
    let e = env(EditFormat::StrReplace);
    let broken = "fn broken( {\n    1;\n}\n\nfn ok() {}\n";
    put(&e, "a.rs", broken).await;
    read(&e, "a.rs").await;
    let r = EditTool
        .run(
            json!({"path": "a.rs", "old_text": "fn ok() {}", "new_text": "fn ok() { 2; }"}),
            &e.sb,
            &e.cx,
        )
        .await;
    assert!(!r.is_error);
    assert!(!r.output.contains("syntax warning"), "{}", r.output);
}
