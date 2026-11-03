use std::path::Path;

use hx_provider::{Capabilities, EditFormat};
use hx_sandbox::{Effect, LocalSandbox, Sandbox};
use hx_tools::{
    AppliedEffect, GlobTool, GrepTool, ReadTool, Tool, ToolCx, ToolResult, WriteTool, registry,
};
use serde_json::{Value, json};
use tempfile::TempDir;

struct Env {
    _dir: TempDir,
    sb: LocalSandbox,
    cx: ToolCx,
}

fn env(fmt: EditFormat) -> Env {
    let dir = TempDir::new().unwrap();
    let sb = LocalSandbox::new(dir.path().to_path_buf()).unwrap();
    let cx = ToolCx::new(fmt, dir.path().join(".spill"));
    Env { _dir: dir, sb, cx }
}

async fn put(e: &Env, path: &str, bytes: &[u8]) {
    e.sb.write_atomic(Path::new(path), bytes, None)
        .await
        .unwrap();
}

async fn run(e: &Env, tool: &dyn Tool, args: Value) -> ToolResult {
    tool.run(args, &e.sb, &e.cx).await
}

#[tokio::test]
async fn read_line_range_numbers_lines() {
    let e = env(EditFormat::StrReplace);
    put(&e, "f.txt", b"a\nb\nc\nd").await;
    let r = run(
        &e,
        &ReadTool,
        json!({"path":"f.txt","start_line":2,"end_line":3}),
    )
    .await;
    assert!(!r.is_error, "{}", r.output);
    assert_eq!(r.output, "2\tb\n3\tc");
    let full = e.sb.resolve(Path::new("f.txt")).unwrap();
    let (_, hash) = e.cx.reads().last(&full).expect("tracked");
    assert_eq!(hash, blake3::hash(b"a\nb\nc\nd"));
    assert_eq!(r.effects, vec![AppliedEffect::Read(full, hash)]);
}

#[tokio::test]
async fn read_emits_hashline_anchors_when_format_is_hashline() {
    let e = env(EditFormat::Hashline);
    put(&e, "f.txt", b"alpha\nbeta\n").await;
    let r = run(&e, &ReadTool, json!({"path":"f.txt"})).await;
    let tag = |s: &str| blake3::hash(s.as_bytes()).to_hex()[..4].to_string();
    assert_eq!(
        r.output,
        format!("1:{}|alpha\n2:{}|beta", tag("alpha"), tag("beta"))
    );
}

#[tokio::test]
async fn read_large_file_without_range_is_truncated_with_remainder_note() {
    let e = env(EditFormat::StrReplace);
    let body: String = (1..=2500).map(|i| format!("l{i}\n")).collect();
    put(&e, "big.txt", body.as_bytes()).await;
    let r = run(&e, &ReadTool, json!({"path":"big.txt"})).await;
    assert!(r.output.contains("2000\tl2000"));
    assert!(!r.output.contains("2001\tl2001"));
    assert!(r.output.contains("500 more lines"), "{}", r.output);
}

#[tokio::test]
async fn read_binary_file_is_error() {
    let e = env(EditFormat::StrReplace);
    put(&e, "bin.dat", b"ab\0cd").await;
    let r = run(&e, &ReadTool, json!({"path":"bin.dat"})).await;
    assert!(r.is_error && r.output.contains("binary"), "{}", r.output);
}

#[tokio::test]
async fn read_non_utf8_is_error() {
    let e = env(EditFormat::StrReplace);
    put(&e, "l1.txt", &[b'c', b'a', b'f', 0xE9, 0xFF, 0xFE]).await;
    let r = run(&e, &ReadTool, json!({"path":"l1.txt"})).await;
    assert!(r.is_error && r.output.contains("not UTF-8"), "{}", r.output);
}

#[tokio::test]
async fn read_bad_args_and_outside_root_are_errors_not_panics() {
    let e = env(EditFormat::StrReplace);
    let r = run(&e, &ReadTool, json!({"nope": 1})).await;
    assert!(r.is_error);
    let r = run(&e, &ReadTool, json!({"path":"../../etc/passwd"})).await;
    assert!(r.is_error);
    assert!(ReadTool.effect(&json!({})).is_err());
}

#[tokio::test]
async fn write_effect_lists_path() {
    assert_eq!(
        WriteTool
            .effect(&json!({"path":"a.txt","content":"x"}))
            .unwrap(),
        Effect::Write {
            paths: vec!["a.txt".into()]
        }
    );
}

#[tokio::test]
async fn write_records_pre_image_of_existing_file() {
    let e = env(EditFormat::StrReplace);
    put(&e, "a.txt", b"old").await;
    let r = run(&e, &WriteTool, json!({"path":"a.txt","content":"new"})).await;
    assert!(!r.is_error, "{}", r.output);
    let full = e.sb.resolve(Path::new("a.txt")).unwrap();
    assert_eq!(
        r.effects,
        vec![AppliedEffect::Edited {
            path: full.clone(),
            pre_image: Some(b"old".to_vec()),
            post_hash: blake3::hash(b"new"),
        }]
    );
    assert_eq!(e.sb.read(Path::new("a.txt")).await.unwrap(), b"new");
    assert_eq!(e.cx.reads().last(&full).unwrap().1, blake3::hash(b"new"));
}

#[tokio::test]
async fn write_new_file_has_no_pre_image() {
    let e = env(EditFormat::StrReplace);
    let r = run(&e, &WriteTool, json!({"path":"d/n.txt","content":"x"})).await;
    assert!(!r.is_error, "{}", r.output);
    assert!(matches!(
        &r.effects[0],
        AppliedEffect::Edited {
            pre_image: None,
            ..
        }
    ));
}

#[tokio::test]
async fn grep_respects_gitignore() {
    let e = env(EditFormat::StrReplace);
    put(&e, ".gitignore", b"target/\n").await;
    put(&e, "src/a.rs", b"fn needle() {}\n").await;
    put(&e, "target/b.rs", b"fn needle() {}\n").await;
    let r = run(&e, &GrepTool, json!({"pattern":"needle"})).await;
    assert!(!r.is_error, "{}", r.output);
    assert_eq!(r.output, "src/a.rs:1:fn needle() {}");
}

#[tokio::test]
async fn grep_options_and_no_match() {
    let e = env(EditFormat::StrReplace);
    put(&e, "a.rs", b"Hello\n").await;
    put(&e, "b.txt", b"hello\n").await;
    let r = run(&e, &GrepTool, json!({"pattern":"hello"})).await;
    assert_eq!(r.output, "b.txt:1:hello");
    let r = run(
        &e,
        &GrepTool,
        json!({"pattern":"hello","case_insensitive":true,"glob":"*.rs"}),
    )
    .await;
    assert_eq!(r.output, "a.rs:1:Hello");
    let r = run(&e, &GrepTool, json!({"pattern":"zzz"})).await;
    assert!(!r.is_error);
    assert!(r.output.contains("no matches"));
    let r = run(&e, &GrepTool, json!({"pattern":"("})).await;
    assert!(r.is_error);
}

#[tokio::test]
async fn grep_caps_match_count() {
    let e = env(EditFormat::StrReplace);
    let body = "x\n".repeat(600);
    put(&e, "a.txt", body.as_bytes()).await;
    let r = run(&e, &GrepTool, json!({"pattern":"x"})).await;
    assert!(
        r.output.lines().count() <= 501,
        "{}",
        r.output.lines().count()
    );
    assert!(r.output.contains("truncated"));
}

#[tokio::test]
async fn glob_returns_sorted_relative_paths_and_terminates_on_symlink_cycle() {
    let e = env(EditFormat::StrReplace);
    put(&e, "b/z.rs", b"").await;
    put(&e, "a/y.rs", b"").await;
    put(&e, "a/n.txt", b"").await;
    let root = e.sb.root().to_path_buf();
    std::os::unix::fs::symlink(&root, root.join("a/loop")).unwrap();
    let r = run(&e, &GlobTool, json!({"pattern":"**/*.rs"})).await;
    assert!(!r.is_error, "{}", r.output);
    assert_eq!(r.output, "a/y.rs\nb/z.rs");
    let r = run(&e, &GlobTool, json!({"pattern":"*.rs","path":"b"})).await;
    assert_eq!(r.output, "b/z.rs");
}

#[test]
fn registry_order_is_stable() {
    let caps = Capabilities::conservative();
    let tools = registry(&caps);
    let names: Vec<_> = tools.iter().map(|t| t.name()).collect();
    assert_eq!(
        names,
        [
            "read",
            "edit",
            "write",
            "bash",
            "grep",
            "glob",
            "outline",
            "symbol",
            "refs",
            "web_fetch",
            "pin",
            "state"
        ]
    );
    for t in &tools {
        let s = t.spec(&caps);
        assert_eq!(s.name, t.name());
        assert_eq!(s.schema["additionalProperties"], json!(false));
        assert_eq!(s.schema["type"], json!("object"));
    }
}

const CAP: usize = 31 * 1024;

#[tokio::test]
async fn read_single_huge_line_is_capped_with_marker() {
    let e = env(EditFormat::StrReplace);
    put(&e, "min.js", "x".repeat(100 * 1024).as_bytes()).await;
    let r = run(&e, &ReadTool, json!({"path":"min.js"})).await;
    assert!(!r.is_error);
    assert!(r.output.len() <= CAP, "{}", r.output.len());
    assert!(r.output.contains("bytes omitted"));
    assert!(r.output.contains("request a narrower line range"));
    let r = run(
        &e,
        &ReadTool,
        json!({"path":"min.js","start_line":1,"end_line":1}),
    )
    .await;
    assert!(r.output.len() <= CAP && r.output.contains("bytes omitted"));
}

#[tokio::test]
async fn glob_output_is_capped() {
    let e = env(EditFormat::StrReplace);
    let long = "d".repeat(100);
    for i in 0..600 {
        put(&e, &format!("{long}/{i}_{long}.rs"), b"").await;
    }
    let r = run(&e, &GlobTool, json!({"pattern":"**/*.rs"})).await;
    assert!(r.output.len() <= CAP, "{}", r.output.len());
    assert!(r.output.contains("narrow the pattern"));
}

#[tokio::test]
async fn grep_byte_cap_applies_below_match_cap() {
    let e = env(EditFormat::StrReplace);
    let body = format!("{}\n", "y".repeat(1000)).repeat(100);
    put(&e, "a.txt", body.as_bytes()).await;
    let r = run(&e, &GrepTool, json!({"pattern":"y"})).await;
    assert!(r.output.len() <= CAP, "{}", r.output.len());
    assert!(r.output.contains("truncated") || r.output.contains("bytes omitted"));
}

#[tokio::test]
async fn symlink_escape_is_neither_globbed_nor_grepped() {
    let e = env(EditFormat::StrReplace);
    let outside_dir = TempDir::new().unwrap();
    let outside = LocalSandbox::new(outside_dir.path().to_path_buf()).unwrap();
    outside
        .write_atomic(Path::new("secret.rs"), b"needle\n", None)
        .await
        .unwrap();
    put(&e, "a/ok.rs", b"needle\n").await;
    let root = e.sb.root().to_path_buf();
    std::os::unix::fs::symlink(outside_dir.path(), root.join("a/escape")).unwrap();
    std::os::unix::fs::symlink(outside_dir.path().join("secret.rs"), root.join("a/s.rs")).unwrap();
    let r = run(&e, &GlobTool, json!({"pattern":"**/*.rs"})).await;
    assert_eq!(r.output, "a/ok.rs");
    let r = run(&e, &GrepTool, json!({"pattern":"needle"})).await;
    assert_eq!(r.output, "a/ok.rs:1:needle");
}

#[tokio::test]
async fn grep_terminates_on_symlink_loop() {
    let e = env(EditFormat::StrReplace);
    put(&e, "a/f.txt", b"needle\n").await;
    let root = e.sb.root().to_path_buf();
    std::os::unix::fs::symlink(&root, root.join("a/loop")).unwrap();
    let r = run(&e, &GrepTool, json!({"pattern":"needle"})).await;
    assert!(!r.is_error);
    assert_eq!(r.output, "a/f.txt:1:needle");
}

#[tokio::test]
async fn cancelled_grep_and_glob_return_error() {
    let mut e = env(EditFormat::StrReplace);
    put(&e, "a.txt", b"x\n").await;
    let tok = tokio_util::sync::CancellationToken::new();
    tok.cancel();
    e.cx = ToolCx::new(EditFormat::StrReplace, ".spill".into()).with_cancel(tok);
    let r = run(&e, &GrepTool, json!({"pattern":"x"})).await;
    assert!(r.is_error && r.output.contains("cancelled"), "{}", r.output);
    let r = run(&e, &GlobTool, json!({"pattern":"*.txt"})).await;
    assert!(r.is_error && r.output.contains("cancelled"), "{}", r.output);
}

#[tokio::test]
async fn grep_reports_skipped_files() {
    let e = env(EditFormat::StrReplace);
    put(&e, "ok.txt", b"needle\n").await;
    put(&e, "bin.dat", b"needle\0").await;
    put(&e, "huge.txt", &vec![b'a'; 10 * 1024 * 1024 + 1]).await;
    let r = run(&e, &GrepTool, json!({"pattern":"needle"})).await;
    assert!(r.output.starts_with("ok.txt:1:needle"), "{}", r.output);
    assert!(
        r.output
            .contains("skipped 2 files: binary/unreadable/too large"),
        "{}",
        r.output
    );
}

#[tokio::test]
async fn glob_star_does_not_cross_directories() {
    let e = env(EditFormat::StrReplace);
    put(&e, "src/a.rs", b"").await;
    put(&e, "src/a/b.rs", b"").await;
    let r = run(&e, &GlobTool, json!({"pattern":"src/*.rs"})).await;
    assert_eq!(r.output, "src/a.rs");
    let r = run(&e, &GrepTool, json!({"pattern":"", "glob":"src/*.rs"})).await;
    assert!(!r.output.contains("src/a/b.rs"));
}

// ---- checkpoint fix: I3 model-supplied line numbers never overflow ----
#[tokio::test]
async fn huge_line_numbers_do_not_panic() {
    let e = env(EditFormat::StrReplace);
    put(&e, "f.txt", b"a\nb\n").await;
    put(&e, "empty.txt", b"").await;
    let max = u64::MAX;
    for (path, args) in [
        ("f.txt", json!({"start_line": max})),
        ("f.txt", json!({"start_line": max, "end_line": max})),
        ("f.txt", json!({"start_line": 1, "end_line": max})),
        ("f.txt", json!({"start_line": max - 1})),
        ("empty.txt", json!({"start_line": max})),
        ("empty.txt", json!({"start_line": max - 2000})),
    ] {
        let mut args = args;
        args["path"] = json!(path);
        // Reaching here without a panic is the point; past-the-end ranges are errors.
        let r = run(&e, &ReadTool, args.clone()).await;
        if path == "f.txt" && args["start_line"] != json!(1) {
            assert!(r.is_error, "{args}: {}", r.output);
        }
    }
    let r = run(
        &e,
        &ReadTool,
        json!({"path": "f.txt", "start_line": 1, "end_line": max}),
    )
    .await;
    assert!(!r.is_error, "{}", r.output);
    assert_eq!(r.output, "1\ta\n2\tb");
}
