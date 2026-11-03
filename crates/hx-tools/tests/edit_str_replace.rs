use std::path::Path;

use hx_provider::{Capabilities, EditFormat};
use hx_sandbox::{Effect, LocalSandbox, Sandbox};
use hx_tools::edit::str_replace::{MatchError, apply, apply_staged};
use hx_tools::edit::tolerant::{RepairStage, locate};
use hx_tools::{AppliedEffect, EditTool, ReadTool, Tool, ToolCx, ToolResult};
use proptest::prelude::*;
use serde_json::json;
use tempfile::TempDir;

// ---------------------------------------------------------------- structural (stage 5)

const STRUCTURAL_SRC: &str = "fn keep() {}\n\nfn compute(a: i32, b: i32) -> i32 {\n    let sum = a + b;\n    let doubled = sum * 2;\n    doubled - 1\n}\n\nfn tail() {}\n";

#[tokio::test]
async fn structural_fallback_replaces_symbol_body_when_similar() {
    let e = env();
    put(&e, "a.rs", STRUCTURAL_SRC.as_bytes()).await;
    read(&e, "a.rs").await;
    // A misspelt identifier on two lines, so stages 1-4 all fail.
    let old = "fn compute(a: i32, b: i32) -> i32 {\n    let sum = a + b;\n    let doubld = sum * 2;\n    doubld - 1\n}";
    let new = "fn compute(a: i32, b: i32) -> i32 {\n    (a + b) * 3\n}";
    let r = edit(&e, "a.rs", old, new).await;
    assert!(!r.is_error, "{}", r.output);
    assert!(
        r.output.contains("applied via structural match"),
        "{}",
        r.output
    );
    let got = String::from_utf8(slurp(&e, "a.rs").await).unwrap();
    assert_eq!(
        got,
        "fn keep() {}\n\nfn compute(a: i32, b: i32) -> i32 {\n    (a + b) * 3\n}\n\nfn tail() {}\n"
    );
}

#[tokio::test]
async fn structural_fallback_reindents_a_method() {
    let e = env();
    let src =
        "impl S {\n    fn m(&self) -> u8 {\n        let v = self.base;\n        v + 1\n    }\n}\n";
    put(&e, "a.rs", src.as_bytes()).await;
    read(&e, "a.rs").await;
    let old = "fn m(&self) -> u8 {\n    let w = self.base;\n    w + 1\n}";
    let new = "fn m(&self) -> u8 {\n    self.base + 2\n}";
    let r = edit(&e, "a.rs", old, new).await;
    assert!(!r.is_error, "{}", r.output);
    assert!(r.output.contains("structural"), "{}", r.output);
    let got = String::from_utf8(slurp(&e, "a.rs").await).unwrap();
    assert_eq!(
        got,
        "impl S {\n    fn m(&self) -> u8 {\n        self.base + 2\n    }\n}\n"
    );
}

#[tokio::test]
async fn structural_fallback_rejected_below_0_8() {
    let e = env();
    put(&e, "a.rs", STRUCTURAL_SRC.as_bytes()).await;
    read(&e, "a.rs").await;
    let old = "fn compute(a: i32, b: i32) -> i32 {\n    println!(\"something else entirely\");\n    unreachable!()\n}";
    let r = edit(&e, "a.rs", old, "fn compute() {}").await;
    assert!(r.is_error);
    assert!(r.output.contains("not found"), "{}", r.output);
    assert_eq!(slurp(&e, "a.rs").await, STRUCTURAL_SRC.as_bytes());
}

#[tokio::test]
async fn structural_fallback_reverts_on_new_parse_error() {
    let e = env();
    put(&e, "a.rs", STRUCTURAL_SRC.as_bytes()).await;
    read(&e, "a.rs").await;
    let old = "fn compute(a: i32, b: i32) -> i32 {\n    let sum = a + b;\n    let doubld = sum * 2;\n    doubld - 1\n}";
    // The replacement leaves its brace open.
    let new = "fn compute(a: i32, b: i32) -> i32 {\n    let sum = a + b;\n";
    let r = edit(&e, "a.rs", old, new).await;
    assert!(r.is_error);
    assert!(r.output.contains("syntax errors"), "{}", r.output);
    assert!(r.output.contains("nothing was changed"), "{}", r.output);
    assert_eq!(slurp(&e, "a.rs").await, STRUCTURAL_SRC.as_bytes());
}

const PY_SRC: &str = "def helper():\n    return 0\n\ndef compute_everything(first_value, second_value):\n    total_amount = first_value + second_value\n    scaled_amount = total_amount * 2\n    adjusted_amount = scaled_amount - 1\n    logged_amount = adjusted_amount + 0\n    final_amount = logged_amount\n    return final_amount\n\ndef neighbour():\n    return 1\n";

#[tokio::test]
async fn structural_fallback_rejects_truncated_old_text() {
    let e = env();
    put(&e, "a.py", PY_SRC.as_bytes()).await;
    read(&e, "a.py").await;
    // The first 5 of 6 body lines (the last statement is missing) with a typo. It parses as
    // a complete shorter function, so replacing the whole symbol would silently drop the tail.
    let old = "def compute_everything(first_value, second_value):\n    total_amount = first_value + second_value\n    scaled_amount = total_amount * 2\n    adjusted_amount = scaled_amount - 1\n    logged_amount = adjusted_amount + 0\n    final_amount = logged_amnt";
    let r = edit(
        &e,
        "a.py",
        old,
        "def compute_everything(a, b):\n    return a + b",
    )
    .await;
    assert!(r.is_error, "{}", r.output);
    assert!(r.output.contains("not found"), "{}", r.output);
    assert_eq!(slurp(&e, "a.py").await, PY_SRC.as_bytes());
}

#[tokio::test]
async fn structural_fallback_rejects_old_text_that_includes_a_neighbour() {
    let e = env();
    put(&e, "a.py", PY_SRC.as_bytes()).await;
    read(&e, "a.py").await;
    // The target (with a typo) plus the next function: replacing only the first symbol
    // would duplicate the neighbour.
    let old = "def compute_everything(first_value, second_value):\n    total_amount = first_value + second_value\n    scaled_amount = total_amount * 2\n    adjusted_amount = scaled_amount - 1\n    logged_amount = adjusted_amount + 0\n    final_amount = logged_amnt\n    return final_amount\n\ndef neighbour():\n    return 1";
    let new = "def compute_everything(a, b):\n    return a + b\n\ndef neighbour():\n    return 2";
    let r = edit(&e, "a.py", old, new).await;
    assert!(r.is_error, "{}", r.output);
    assert!(r.output.contains("not found"), "{}", r.output);
    assert_eq!(slurp(&e, "a.py").await, PY_SRC.as_bytes());
}

#[tokio::test]
async fn structural_fallback_repairs_exact_extent_python() {
    let e = env();
    put(&e, "a.py", PY_SRC.as_bytes()).await;
    read(&e, "a.py").await;
    let old = "def compute_everything(first_value, second_value):\n    total_amount = first_value + second_value\n    scaled_amount = total_amount * 2\n    adjusted_amount = scaled_amount - 1\n    logged_amount = adjusted_amount + 0\n    final_amount = logged_amnt\n    return final_amount";
    let r = edit(
        &e,
        "a.py",
        old,
        "def compute_everything(a, b):\n    return a + b",
    )
    .await;
    assert!(!r.is_error, "{}", r.output);
    assert!(r.output.contains("structural"), "{}", r.output);
    let got = String::from_utf8(slurp(&e, "a.py").await).unwrap();
    assert_eq!(
        got,
        "def helper():\n    return 0\n\ndef compute_everything(a, b):\n    return a + b\n\ndef neighbour():\n    return 1\n"
    );
}

const PY_REPEAT: &str = "def compute_everything(first_value, second_value):\n    total_amount = first_value + second_value\n    scaled_amount = total_amount * 2\n    logged_amount = scaled_amount + 0\n    result = logged_amount\n    if result < 0:\n        return result\n    result = result + 1\n    return result\n\ndef neighbour():\n    return 1\n";

#[tokio::test]
async fn structural_fallback_rejects_truncation_at_a_repeated_line() {
    let e = env();
    put(&e, "a.py", PY_REPEAT.as_bytes()).await;
    read(&e, "a.py").await;
    // Cut after the inner `return result`, which equals the function's real last line once
    // trimmed. It parses as a complete shorter function; the missing tail must not be lost.
    let old = "def compute_everything(first_value, second_value):\n    total_amount = first_value + second_value\n    scaled_amount = total_amount * 2\n    logged_amount = scaled_amnt + 0\n    result = logged_amount\n    if result < 0:\n        return result";
    let r = edit(
        &e,
        "a.py",
        old,
        "def compute_everything(a, b):\n    return a + b",
    )
    .await;
    assert!(r.is_error, "{}", r.output);
    assert!(r.output.contains("not found"), "{}", r.output);
    assert_eq!(slurp(&e, "a.py").await, PY_REPEAT.as_bytes());
}

#[tokio::test]
async fn structural_fallback_rejects_an_extra_line() {
    let e = env();
    put(&e, "a.py", PY_REPEAT.as_bytes()).await;
    read(&e, "a.py").await;
    let old = "def compute_everything(first_value, second_value):\n    total_amount = first_value + second_value\n    scaled_amount = total_amount * 2\n    print(scaled_amount)\n    logged_amount = scaled_amnt + 0\n    result = logged_amount\n    if result < 0:\n        return result\n    result = result + 1\n    return result";
    let r = edit(
        &e,
        "a.py",
        old,
        "def compute_everything(a, b):\n    return a + b",
    )
    .await;
    assert!(r.is_error, "{}", r.output);
    assert_eq!(slurp(&e, "a.py").await, PY_REPEAT.as_bytes());
}

// ---------------------------------------------------------------- tool-level

struct Env {
    _dir: TempDir,
    sb: LocalSandbox,
    cx: ToolCx,
}

fn env() -> Env {
    let dir = TempDir::new().unwrap();
    let sb = LocalSandbox::new(dir.path().to_path_buf()).unwrap();
    let cx = ToolCx::new(EditFormat::StrReplace, dir.path().join(".spill"));
    Env { _dir: dir, sb, cx }
}

async fn put(e: &Env, path: &str, bytes: &[u8]) {
    e.sb.write_atomic(Path::new(path), bytes, None)
        .await
        .unwrap();
}

async fn slurp(e: &Env, path: &str) -> Vec<u8> {
    e.sb.read(Path::new(path)).await.unwrap()
}

async fn read(e: &Env, path: &str) {
    let r = ReadTool.run(json!({"path": path}), &e.sb, &e.cx).await;
    assert!(!r.is_error, "{}", r.output);
}

async fn edit(e: &Env, path: &str, old: &str, new: &str) -> ToolResult {
    EditTool
        .run(
            json!({"path": path, "old_text": old, "new_text": new}),
            &e.sb,
            &e.cx,
        )
        .await
}

#[tokio::test]
async fn rejects_edit_without_prior_read() {
    let e = env();
    put(&e, "a.rs", b"x").await;
    let r = edit(&e, "a.rs", "x", "y").await;
    assert!(r.is_error);
    assert!(r.output.contains("read the file first"), "{}", r.output);
    assert_eq!(slurp(&e, "a.rs").await, b"x");
}

#[tokio::test]
async fn rejects_edit_when_file_changed_since_read() {
    let e = env();
    put(&e, "a.rs", b"x").await;
    read(&e, "a.rs").await;
    put(&e, "a.rs", b"x changed").await;
    let r = edit(&e, "a.rs", "x", "y").await;
    assert!(r.is_error);
    assert!(r.output.contains("changed since last read"), "{}", r.output);
    assert_eq!(slurp(&e, "a.rs").await, b"x changed");
}

#[tokio::test]
async fn edits_a_file_that_was_read() {
    let e = env();
    put(&e, "a.rs", b"a\nfoo\nb\n").await;
    read(&e, "a.rs").await;
    let r = edit(&e, "a.rs", "foo", "bar").await;
    assert!(!r.is_error, "{}", r.output);
    assert!(!r.output.contains("applied via"), "{}", r.output);
    assert_eq!(slurp(&e, "a.rs").await, b"a\nbar\nb\n");
}

#[tokio::test]
async fn consecutive_edits_need_no_reread() {
    let e = env();
    put(&e, "a.rs", b"one\ntwo\n").await;
    read(&e, "a.rs").await;
    assert!(!edit(&e, "a.rs", "one", "1").await.is_error);
    let r = edit(&e, "a.rs", "two", "2").await;
    assert!(!r.is_error, "{}", r.output);
    assert_eq!(slurp(&e, "a.rs").await, b"1\n2\n");
}

#[tokio::test]
async fn preserves_crlf_bom_no_trailing_newline_and_mode() {
    let e = env();
    e.sb.write_atomic(Path::new("a.txt"), "\u{FEFF}a\r\nb".as_bytes(), Some(0o755))
        .await
        .unwrap();
    read(&e, "a.txt").await;
    let r = edit(&e, "a.txt", "b", "c\nd\n").await;
    assert!(!r.is_error, "{}", r.output);
    assert_eq!(slurp(&e, "a.txt").await, "\u{FEFF}a\r\nc\r\nd".as_bytes());
    let full = e.sb.resolve(Path::new("a.txt")).unwrap();
    assert_eq!(e.sb.metadata(&full).await.unwrap().mode & 0o7777, 0o755);
}

#[tokio::test]
async fn preserves_trailing_newline_state_both_ways() {
    let e = env();
    put(&e, "with.txt", b"a\nb\n").await;
    read(&e, "with.txt").await;
    assert!(!edit(&e, "with.txt", "b\n", "c").await.is_error);
    assert_eq!(slurp(&e, "with.txt").await, b"a\nc\n");

    put(&e, "without.txt", b"a\nb").await;
    read(&e, "without.txt").await;
    assert!(!edit(&e, "without.txt", "b", "c\n").await.is_error);
    assert_eq!(slurp(&e, "without.txt").await, b"a\nc");
}

#[tokio::test]
async fn edit_on_binary_file_is_error() {
    let e = env();
    put(&e, "b.bin", b"ab\0cd").await;
    let r = edit(&e, "b.bin", "ab", "xy").await;
    assert!(r.is_error);
    assert!(r.output.contains("binary"), "{}", r.output);
    assert_eq!(slurp(&e, "b.bin").await, b"ab\0cd");
}

#[tokio::test]
async fn result_carries_pre_image() {
    let e = env();
    put(&e, "a.rs", b"fn a() {}\n").await;
    read(&e, "a.rs").await;
    let r = edit(&e, "a.rs", "a()", "b()").await;
    assert!(!r.is_error, "{}", r.output);
    let full = e.sb.resolve(Path::new("a.rs")).unwrap();
    assert_eq!(
        r.effects,
        vec![AppliedEffect::Edited {
            path: full,
            pre_image: Some(b"fn a() {}\n".to_vec()),
            post_hash: blake3::hash(b"fn b() {}\n"),
        }]
    );
}

#[tokio::test]
async fn repaired_edit_names_the_stage_in_the_result() {
    let e = env();
    put(
        &e,
        "a.rs",
        b"fn f() {\n    if x {\n        y();\n    }\n}\n",
    )
    .await;
    read(&e, "a.rs").await;
    let r = edit(&e, "a.rs", "if x {\n    y();\n}", "if x {\n    z();\n}").await;
    assert!(!r.is_error, "{}", r.output);
    assert!(
        r.output.contains("applied via indentation-relative match"),
        "{}",
        r.output
    );
    assert_eq!(
        slurp(&e, "a.rs").await,
        b"fn f() {\n    if x {\n        z();\n    }\n}\n"
    );
}

#[tokio::test]
async fn crlf_in_new_text_never_produces_mixed_endings() {
    let e = env();
    put(&e, "a.txt", b"a\nb\n").await;
    read(&e, "a.txt").await;
    let r = edit(&e, "a.txt", "b", "x\r\ny").await;
    assert!(!r.is_error, "{}", r.output);
    assert_eq!(slurp(&e, "a.txt").await, b"a\nx\ny\n");
}

#[tokio::test]
async fn not_found_shows_nearest_window_with_line_numbers() {
    let e = env();
    put(
        &e,
        "a.rs",
        b"fn a() {}\nfn target() {\n    1\n}\nfn z() {}\n",
    )
    .await;
    read(&e, "a.rs").await;
    let r = edit(&e, "a.rs", "fn targat() {\n    1\n}", "x").await;
    assert!(r.is_error);
    assert!(r.output.contains("2\tfn target() {"), "{}", r.output);
    assert!(r.output.contains("3\t    1"), "{}", r.output);
}

#[tokio::test]
async fn edit_of_bom_file_accepts_line_one_copied_from_read() {
    let e = env();
    put(&e, "a.txt", "\u{FEFF}a\r\nb".as_bytes()).await;
    read(&e, "a.txt").await;
    let r = edit(&e, "a.txt", "\u{FEFF}a", "\u{FEFF}x").await;
    assert!(!r.is_error, "{}", r.output);
    assert_eq!(slurp(&e, "a.txt").await, "\u{FEFF}x\r\nb".as_bytes());
}

#[tokio::test]
async fn ambiguous_edit_lists_lines() {
    let e = env();
    put(&e, "a.rs", b"x\ny\nx\n").await;
    read(&e, "a.rs").await;
    let r = edit(&e, "a.rs", "x", "z").await;
    assert!(r.is_error);
    assert!(r.output.contains("1, 3"), "{}", r.output);
    assert_eq!(slurp(&e, "a.rs").await, b"x\ny\nx\n");
}

#[tokio::test]
async fn bad_arguments_are_errors_not_panics() {
    let e = env();
    put(&e, "a.rs", b"x").await;
    read(&e, "a.rs").await;
    for args in [
        json!({"path": "a.rs"}),
        json!({"path": "a.rs", "old_text": "x", "new_text": "y", "bogus": 1}),
        json!({"path": "a.rs", "old_text": "", "new_text": "y"}),
        json!({"path": "a.rs", "old_text": "x", "new_text": "x"}),
        json!({"path": "missing.rs", "old_text": "x", "new_text": "y"}),
        json!({"path": "../escape.rs", "old_text": "x", "new_text": "y"}),
    ] {
        let r = EditTool.run(args.clone(), &e.sb, &e.cx).await;
        assert!(r.is_error, "{args}: {}", r.output);
    }
    assert_eq!(slurp(&e, "a.rs").await, b"x");
}

#[test]
fn effect_is_a_write_to_the_path() {
    let eff = EditTool
        .effect(&json!({"path": "a.rs", "old_text": "x", "new_text": "y"}))
        .unwrap();
    assert_eq!(
        eff,
        Effect::Write {
            paths: vec!["a.rs".into()]
        }
    );
    assert!(EditTool.effect(&json!({"path": "a.rs"})).is_err());
}

#[test]
fn spec_follows_edit_format() {
    for fmt in [
        EditFormat::StrReplace,
        EditFormat::Patch,
        EditFormat::Hashline,
    ] {
        let caps = Capabilities {
            edit_format: fmt,
            ..Capabilities::conservative()
        };
        let s = EditTool.spec(&caps);
        assert_eq!(s.name, "edit");
        assert_eq!(s.schema["type"], json!("object"));
        assert_eq!(s.schema["additionalProperties"], json!(false));
        let required = match fmt {
            EditFormat::StrReplace => json!(["path", "old_text", "new_text"]),
            EditFormat::Patch => json!(["patch"]),
            EditFormat::Hashline => json!(["path", "ops"]),
        };
        assert_eq!(s.schema["required"], required, "{fmt:?}");
    }
}

// ------------------------------------------------------------ str_replace::apply

#[test]
fn exact_match_replaces() {
    assert_eq!(apply("a\nfoo\nb", "foo", "bar").unwrap(), "a\nbar\nb");
}

#[test]
fn ambiguous_exact_match_fails_with_lines() {
    assert_eq!(
        apply("x\nx", "x", "y"),
        Err(MatchError::Ambiguous { lines: vec![1, 2] })
    );
}

#[test]
fn overlapping_exact_matches_are_ambiguous() {
    assert_eq!(
        apply("aaa", "aa", "b"),
        Err(MatchError::Ambiguous { lines: vec![1, 1] })
    );
}

#[test]
fn whitespace_fallback_only_when_unique() {
    assert_eq!(
        apply("fn a() {\n    b();\n}", "fn a() {\n  b();\n}", "fn a() {}").unwrap(),
        "fn a() {}"
    );
    let twice = "fn a() {\n    b();\n}\nfn a() {\n\tb();\n}";
    assert_eq!(
        apply(twice, "fn a() {\n  b();\n}", "z"),
        Err(MatchError::Ambiguous { lines: vec![1, 4] })
    );
}

#[test]
fn not_found_reports_candidates() {
    let err = apply("alpha\nbeta", "betx", "z").unwrap_err();
    match err {
        MatchError::NotFound { candidates, .. } => assert_eq!(candidates, vec![2]),
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        apply("alpha\nbeta", "zzzzzzzz", "z"),
        Err(MatchError::NotFound { .. })
    ));
}

// --------------------------------------------------------------------- tolerant

#[test]
fn normalizes_smart_quotes_crlf_and_trailing_ws() {
    let text = "let s = \"hi\"; // it's\nlet t = a - b;\nend\n";
    let old = "let s = \u{201C}hi\u{201D}; // it\u{2019}s  \r\nlet t = a \u{2013} b;\t\r\n";
    let loc = locate(text, old).unwrap();
    assert_eq!(loc.stage, RepairStage::Normalized);
    assert_eq!(
        &text[loc.range.clone()],
        "let s = \"hi\"; // it's\nlet t = a - b;\n"
    );
    assert_eq!(loc.reindent, None);

    // NBSP becomes a plain space.
    let loc = locate("a b c\n", "a\u{00A0}b c").unwrap();
    assert_eq!(loc.stage, RepairStage::Normalized);

    // Typographic characters the model pasted into new_text are not written into a file
    // that uses ASCII.
    let (out, stage) = apply_staged(
        text,
        "let s = \u{201C}hi\u{201D};",
        "let s = \u{201C}yo\u{201D};",
    )
    .unwrap();
    assert_eq!(stage, RepairStage::Normalized);
    assert!(out.starts_with("let s = \"yo\";"), "{out}");
}

#[test]
fn strips_copied_line_numbers_and_hashline_anchors() {
    let text = "fn a() {\n    b();\n}\n";
    let loc = locate(text, "2\t    b();\n3\t}").unwrap();
    assert_eq!(loc.stage, RepairStage::Normalized);
    assert_eq!(&text[loc.range], "    b();\n}");

    let loc = locate(text, "1:ab12|fn a() {\n2:cd34|    b();").unwrap();
    assert_eq!(loc.stage, RepairStage::Normalized);
    assert_eq!(&text[loc.range], "fn a() {\n    b();");

    // A blank line copied from `read` is "<n>\t" and survives trailing-ws stripping.
    let text = "a\n\nb\n";
    let loc = locate(text, "1\ta\n2\t\n3\tb").unwrap();
    assert_eq!(&text[loc.range], "a\n\nb");

    // Not every line is numbered: leave the text alone (and so fail to match).
    assert!(locate("1\tfoo\nbar\n", "1\tzzz\nbar").is_err());

    // The model's new_text carries the same artifacts and loses them too.
    let (out, _) = apply_staged(text, "1\ta\n2\t", "1\tx\n2\t").unwrap();
    assert_eq!(out, "x\n\nb\n");
}

#[test]
fn indentation_relative_match_reindents_replacement() {
    let text = "fn f() {\n    if x {\n        y();\n    }\n}\n";
    let old = "if x {\n    y();\n}";
    let loc = locate(text, old).unwrap();
    assert_eq!(loc.stage, RepairStage::Indentation);
    assert_eq!(loc.reindent, Some(4));
    assert_eq!(&text[loc.range], "    if x {\n        y();\n    }");
    assert_eq!(
        apply(text, old, "if x {\n    z();\n\n    w();\n}").unwrap(),
        "fn f() {\n    if x {\n        z();\n\n        w();\n    }\n}\n"
    );

    // Model over-indented instead: negative offset.
    let text = "if x {\n    y();\n}\n";
    let old = "        if x {\n            y();\n        }";
    let loc = locate(text, old).unwrap();
    assert_eq!(loc.stage, RepairStage::Indentation);
    assert_eq!(loc.reindent, Some(-8));
    assert_eq!(
        apply(text, old, "        if x {\n            z();\n        }").unwrap(),
        "if x {\n    z();\n}\n"
    );

    // Tabs in the file: the replacement is padded with tabs.
    let text = "fn f() {\n\tif x {\n\t\ty();\n\t}\n}\n";
    let (out, stage) = apply_staged(text, "if x {\n\ty();\n}", "if x {\n\tz();\n}").unwrap();
    assert_eq!(stage, RepairStage::Indentation);
    assert_eq!(out, "fn f() {\n\tif x {\n\t\tz();\n\t}\n}\n");
}

#[test]
fn whitespace_stage_when_indentation_is_not_a_uniform_offset() {
    let text = "fn a() {\n    b();\n}";
    let loc = locate(text, "fn a() {\n  b();\n}").unwrap();
    assert_eq!(loc.stage, RepairStage::Whitespace);
    assert_eq!(loc.reindent, None);
}

#[test]
fn whitespace_repair_maps_relative_indentation_onto_the_files_first_line() {
    // The model's block starts at column 2, the file's at 4: every replacement line keeps
    // its offset from the first line, applied to the file's indentation.
    let text = "fn f() {\n    foo(\n      b);\n}\n";
    let (out, stage) = apply_staged(text, "  foo(\n b);", "  bar(\n    b);").unwrap();
    assert_eq!(stage, RepairStage::Whitespace);
    assert_eq!(out, "fn f() {\n    bar(\n      b);\n}\n");
}

#[test]
fn whitespace_repair_converts_indent_style_to_the_files() {
    // Tab-indented file, model wrote the same block with 4-space indentation.
    let text = "fn f() {\n\tif x {\n\t\ty();\n\t}\n}\n";
    let old = "    if x {\n        y();\n    }";
    let new = "    if x {\n        z();\n    }";
    let (out, stage) = apply_staged(text, old, new).unwrap();
    assert_eq!(stage, RepairStage::Whitespace);
    assert_eq!(out, "fn f() {\n\tif x {\n\t\tz();\n\t}\n}\n");

    // And the other way: space-indented file, model used tabs.
    let text = "fn f() {\n    if x {\n        y();\n    }\n}\n";
    let old = "\tif x {\n\t\ty();\n\t}";
    let new = "\tif x {\n\t\tz();\n\t}";
    let (out, _) = apply_staged(text, old, new).unwrap();
    assert_eq!(out, "fn f() {\n    if x {\n        z();\n    }\n}\n");
}

#[test]
fn anchor_repair_reindents_replacement_to_the_files_indentation() {
    let steps: Vec<String> = (0..11).map(|i| format!("step_{i}();")).collect();
    let file_body: String = steps.iter().map(|l| format!("        {l}\n")).collect();
    let text = format!("    fn a() {{\n{file_body}    }}\n");
    // Model block at column 0 with one typo in the interior.
    let mut typo = steps.clone();
    typo[4] = "stepp_4();".to_string();
    let old_body: String = typo.iter().map(|l| format!("    {l}\n")).collect();
    let old = format!("fn a() {{\n{old_body}}}");
    let (out, stage) = apply_staged(&text, &old, "fn a() {\n    new();\n}").unwrap();
    assert_eq!(stage, RepairStage::Anchor);
    assert_eq!(out, "    fn a() {\n        new();\n    }\n");
}

#[test]
fn strips_bom_from_the_models_text() {
    let loc = locate("a\nb\n", "\u{FEFF}a").unwrap();
    assert_eq!(loc.stage, RepairStage::Normalized);
    assert_eq!(loc.range, 0..1);
    // The BOM the model copied into new_text is not written into the file body.
    let (out, _) = apply_staged("a\nb\n", "\u{FEFF}a", "\u{FEFF}x").unwrap();
    assert_eq!(out, "x\nb\n");
}

#[test]
fn anchor_match_requires_unique_first_last_and_0_9_interior() {
    // 11 interior lines; the file's copy has one extra line (line ratio 2*11/23 = 0.96).
    let interior: Vec<String> = (0..11).map(|i| format!("    step_{i}();")).collect();
    let file_interior = {
        let mut v = interior.clone();
        v.insert(5, "    extra();".to_string());
        v
    };
    let build = |mid: &[String]| {
        format!(
            "// head\nfn start() {{\n{}\n}} // end\n// tail\n",
            mid.join("\n")
        )
    };
    let text = build(&file_interior);
    let old = build(&interior);
    let old = old
        .trim_start_matches("// head\n")
        .trim_end_matches("// tail\n");
    let loc = locate(&text, old).unwrap();
    assert_eq!(loc.stage, RepairStage::Anchor);
    assert_eq!(
        &text[loc.range],
        text.trim_start_matches("// head\n")
            .trim_end_matches("// tail\n")
    );

    // Interior too different: no anchor match.
    let mut far = file_interior.clone();
    for (i, l) in far.iter_mut().enumerate() {
        if i % 2 == 0 {
            *l = format!("    other_{i}();");
        }
    }
    assert!(matches!(
        locate(&build(&far), old),
        Err(MatchError::NotFound { .. })
    ));

    // First line not unique: no anchor match.
    let dup = format!("fn start() {{\nzzz\n{text}");
    assert!(matches!(
        locate(&dup, old),
        Err(MatchError::NotFound { .. })
    ));

    // Last line not unique: no anchor match.
    let dup = format!("{text}x\n}} // end\n");
    assert!(matches!(
        locate(&dup, old),
        Err(MatchError::NotFound { .. })
    ));
}

#[test]
fn ambiguous_tolerant_match_is_error() {
    // Two windows equal after trimming: ambiguity at the whitespace stage is an error,
    // even though the anchor stage could not tell them apart either.
    let text = "  a\n  b\nmid\n\ta\n\tb\n";
    assert_eq!(
        locate(text, "a\nb"),
        Err(MatchError::Ambiguous { lines: vec![1, 4] })
    );
    // Same at the normalization stage.
    let text = "x = 1\nx = 1\n";
    assert_eq!(
        locate(text, "x = 1  \r\n"),
        Err(MatchError::Ambiguous { lines: vec![1, 2] })
    );
}

#[test]
fn not_found_includes_nearest_window_max_20_lines() {
    let mut text = String::new();
    for i in 1..=60 {
        text.push_str(&format!("line number {i}\n"));
    }
    // 30-line old text that is close to lines 11..=40 with every line mangled slightly.
    let old: Vec<String> = (11..=40).map(|i| format!("line numbr {i}")).collect();
    let err = locate(&text, &old.join("\n")).unwrap_err();
    let MatchError::NotFound { nearest, .. } = err else {
        panic!("{err:?}")
    };
    let (start, shown) = nearest.expect("nearest window");
    assert_eq!(start, 11);
    let lines: Vec<&str> = shown.lines().collect();
    assert_eq!(lines.len(), 20);
    assert_eq!(lines[0], "11\tline number 11");
    assert_eq!(lines[19], "30\tline number 30");

    // Small old text: window as long as the old text.
    let err = locate("a\nb\nc\nd\n", "c\nx").unwrap_err();
    let MatchError::NotFound { nearest, .. } = err else {
        panic!("{err:?}")
    };
    assert_eq!(nearest, Some((3, "3\tc\n4\td".to_string())));

    // Nothing resembling the old text: no candidates, but the nearest window is still shown.
    let err = locate("aaaa\nbbbb\n", "zzzzzzzz\nqqqqqqqq").unwrap_err();
    assert_eq!(
        err,
        MatchError::NotFound {
            candidates: vec![],
            nearest: Some((1, "1\taaaa\n2\tbbbb".to_string()))
        }
    );
}

#[test]
fn result_names_repair_stage() {
    assert_eq!(
        apply_staged("a foo b", "foo", "x").unwrap().1,
        RepairStage::Exact
    );
    assert_eq!(
        apply_staged("a\nfoo\nb", "foo \n", "x").unwrap().1,
        RepairStage::Normalized
    );
    assert_eq!(
        apply_staged("a\n    foo(\n      b)\nc", "foo(\n b)", "x")
            .unwrap()
            .1,
        RepairStage::Whitespace
    );
    assert_eq!(
        apply_staged("a\n    foo(\n    b)\nc", "foo(\nb)", "x")
            .unwrap()
            .1,
        RepairStage::Indentation
    );
}

// --------------------------------------------------------------------- proptest

fn occurrences(text: &str, pat: &str) -> usize {
    (0..=text.len() - pat.len())
        .filter(|&i| text[i..].starts_with(pat))
        .count()
}

proptest! {
    #[test]
    fn apply_then_inverse_restores(
        text in "[a-z\\n]{1,200}",
        a in 0usize..200,
        len in 1usize..40,
        new in "[A-Z]{1,5}",
    ) {
        let start = a % text.len();
        let end = (start + len).min(text.len());
        let old = &text[start..end];
        prop_assume!(occurrences(&text, old) == 1);
        let edited = apply(&text, old, &new).unwrap();
        prop_assert_eq!(apply(&edited, &new, old).unwrap(), text);
    }
}
