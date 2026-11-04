//! What each tool reports in `ToolMeta.saved` (spec 7.11.1): the bytes a plain harness
//! with the same 30 KB cap would have returned, minus what was returned.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::LazyLock;

use common::{Built, env};
use serde_json::json;
use tautly_tools::{
    BashTool, FindTool, OutlineTool, ReadTool, RefsTool, SaveKind, Saved, SymbolTool,
};

/// The 30 KB output cap.
const CAP: u32 = 30 * 1024;

fn saved(kind: SaveKind, bytes: u32) -> Option<Saved> {
    Some(Saved { kind, bytes })
}

/// A Rust file of exactly `size` bytes: one function, then comment lines.
fn rust_file(size: usize) -> String {
    let mut text = String::from("fn target() -> u32 {\n    42\n}\n");
    while text.len() + 80 <= size {
        text.push_str(&format!("// {}\n", "x".repeat(76)));
    }
    text.push_str(&"/".repeat(size - text.len() - 1));
    text.push('\n');
    assert_eq!(text.len(), size);
    text
}

#[tokio::test]
async fn diff_reread_reports_full_minus_returned_bytes() {
    // 100 lines of 80 bytes.
    let text: String = (0..100).map(|i| format!("{i:<79}\n")).collect();
    assert_eq!(text.len(), 8000);
    let e = env(&[("a.txt", text.as_bytes())], Built::None).await;
    let first = e.run(&ReadTool, json!({"path": "a.txt"})).await;
    assert!(!first.is_error, "{}", first.output);
    assert_eq!(first.meta.saved, None, "a first read saves nothing");
    let second = e.run(&ReadTool, json!({"path": "a.txt"})).await;
    assert!(
        second.output.starts_with("unchanged since turn"),
        "{}",
        second.output
    );
    assert_eq!(
        second.meta.saved,
        saved(SaveKind::DiffRead, 8000 - second.output.len() as u32)
    );

    // A diff: one line changed, the size kept.
    e.put("a.txt", text.replacen("5  ", "fiv", 1).as_bytes())
        .await;
    let third = e.run(&ReadTool, json!({"path": "a.txt"})).await;
    assert!(
        third.output.starts_with("changed since turn"),
        "{}",
        third.output
    );
    assert_eq!(
        third.meta.saved,
        saved(SaveKind::DiffRead, 8000 - third.output.len() as u32)
    );
}

/// A fake `cargo` that prints `out.txt` from the working directory. Written once, before
/// the test that needs it runs any command, so it is never executed while being written.
#[allow(clippy::disallowed_methods)]
static FAKE_CARGO: LazyLock<PathBuf> = LazyLock::new(|| {
    let dir = std::env::temp_dir().join(format!("tautly-savings-fakes-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("cargo");
    std::fs::write(&path, "#!/bin/sh\ncat out.txt\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
});

/// `cargo test` through the fake.
fn fake_cargo() -> String {
    format!("'{}' test", FAKE_CARGO.display())
}

#[tokio::test]
async fn filtered_cargo_output_reports_capped_raw_minus_returned() {
    let mut out = String::from("running 3000 tests\n");
    let mut n = 0;
    while out.len() < 100 * 1024 {
        out.push_str(&format!("test module_{n}::passes_quietly ... ok\n"));
        n += 1;
    }
    out.push_str(&format!(
        "\ntest result: ok. {n} passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n"
    ));
    let command = fake_cargo();
    let e = env(&[("out.txt", out.as_bytes())], Built::None).await;
    let r = e.run(&BashTool, json!({ "command": command })).await;
    assert!(r.output.starts_with("cargo summary:"), "{}", r.output);
    assert_eq!(
        r.meta.saved,
        saved(SaveKind::Filter, CAP - r.output.len() as u32)
    );
}

#[tokio::test]
async fn capped_but_unfiltered_output_reports_nothing() {
    let command = fake_cargo();
    let out = "plain output line\n".repeat(6000);
    let e = env(&[("out.txt", out.as_bytes())], Built::None).await;
    let r = e.run(&BashTool, json!({"command": "cat out.txt"})).await;
    assert!(r.output.contains("bytes omitted"), "{}", r.output);
    assert_eq!(r.meta.saved, None);
    // `raw: true` is never filtered either.
    let mut cargo = String::new();
    while cargo.len() < 10 * 1024 {
        cargo.push_str("test a::b ... ok\n");
    }
    cargo.push_str("test result: ok. 1 passed; 0 failed; 0 ignored\n");
    e.put("out.txt", cargo.as_bytes()).await;
    let raw = e
        .run(&BashTool, json!({"command": command, "raw": true}))
        .await;
    assert_eq!(raw.meta.saved, None, "{}", raw.output);
}

#[tokio::test]
async fn outline_and_symbol_report_index_saving() {
    let text = rust_file(5000);
    let e = env(&[("lib.rs", text.as_bytes())], Built::Full).await;
    let outline = e.run(&OutlineTool, json!({"path": "lib.rs"})).await;
    assert!(!outline.is_error, "{}", outline.output);
    assert_eq!(
        outline.meta.saved,
        saved(SaveKind::Index, 5000 - outline.output.len() as u32)
    );
    let symbol = e.run(&SymbolTool, json!({"name": "target"})).await;
    assert!(symbol.output.contains("42"), "{}", symbol.output);
    assert_eq!(
        symbol.meta.saved,
        saved(SaveKind::Index, 5000 - symbol.output.len() as u32)
    );

    // A file over the cap counts as the cap.
    let big = rust_file(40 * 1024);
    e.put("lib.rs", big.as_bytes()).await;
    e.index.refresh().await.unwrap();
    let outline = e.run(&OutlineTool, json!({"path": "lib.rs"})).await;
    assert_eq!(
        outline.meta.saved,
        saved(SaveKind::Index, CAP - outline.output.len() as u32)
    );
}

#[tokio::test]
async fn batched_symbol_target_reports_index_saving() {
    let text = rust_file(6000);
    let e = env(&[("lib.rs", text.as_bytes())], Built::Full).await;
    let r = e
        .run(&ReadTool, json!({"targets": [{"symbol": "target"}]}))
        .await;
    assert!(!r.is_error, "{}", r.output);
    assert!(r.output.contains("42"), "{}", r.output);
    assert_eq!(
        r.meta.saved,
        saved(SaveKind::Index, 6000 - r.output.len() as u32)
    );
}

#[tokio::test]
async fn refs_and_find_report_none() {
    let text = rust_file(5000);
    let main = "fn main() { target(); }\n";
    let e = env(
        &[("lib.rs", text.as_bytes()), ("main.rs", main.as_bytes())],
        Built::Full,
    )
    .await;
    let refs = e.run(&RefsTool, json!({"name": "target"})).await;
    assert!(refs.output.contains("main.rs"), "{}", refs.output);
    assert_eq!(refs.meta.saved, None);
    let find = e.run(&FindTool, json!({"query": "target"})).await;
    assert!(!find.is_error, "{}", find.output);
    assert_eq!(find.meta.saved, None);
}
