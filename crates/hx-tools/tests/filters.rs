// Tests write a fake tool's output to a temp dir; the lint targets src.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::path::Path;

use hx_provider::EditFormat;
use hx_sandbox::{LocalSandbox, parse_commands};
use hx_tools::filters::{FilterKind, Item, Summary, apply, detect};
use hx_tools::{BashTool, Tool, ToolCx, ToolResult};
use serde_json::{Value, json};
use tempfile::TempDir;

const CARGO_TEST_FAIL: &str = include_str!("fixtures/filters/cargo_test_fail.txt");
const CARGO_BUILD_ERR: &str = include_str!("fixtures/filters/cargo_build_err.txt");
const CLIPPY_WARN: &str = include_str!("fixtures/filters/clippy_warn.txt");
const PYTEST_FAIL: &str = include_str!("fixtures/filters/pytest_fail.txt");
const PYTEST_QUIET_FAIL: &str = include_str!("fixtures/filters/pytest_quiet_fail.txt");
const JEST_FAIL: &str = include_str!("fixtures/filters/jest_fail.txt");
const VITEST_FAIL: &str = include_str!("fixtures/filters/vitest_fail.txt");
const GOTEST_FAIL: &str = include_str!("fixtures/filters/gotest_fail.txt");
const TSC_ERR: &str = include_str!("fixtures/filters/tsc_err.txt");
const ESLINT_ERR: &str = include_str!("fixtures/filters/eslint_err.txt");

fn kind(cmd: &str) -> Option<FilterKind> {
    detect(&parse_commands(cmd).unwrap_or_else(|e| panic!("{cmd}: {e}")))
}

#[test]
fn detects_common_invocations() {
    use FilterKind::*;
    let cases: &[(&str, Option<FilterKind>)] = &[
        ("cargo test -p x", Some(Cargo)),
        ("cargo build --release", Some(Cargo)),
        ("cargo check --workspace", Some(Cargo)),
        ("cargo clippy --all-targets -- -D warnings", Some(Cargo)),
        ("cargo +nightly test", Some(Cargo)),
        ("RUST_BACKTRACE=1 cargo test", Some(Cargo)),
        ("env RUST_LOG=debug cargo test", Some(Cargo)),
        ("python -m pytest -q", Some(Pytest)),
        ("python3 -m pytest tests/", Some(Pytest)),
        ("pytest", Some(Pytest)),
        (".venv/bin/pytest -x", Some(Pytest)),
        ("uv run pytest", Some(Pytest)),
        ("poetry run pytest -q", Some(Pytest)),
        ("npx jest", Some(Jest)),
        ("yarn jest --ci", Some(Jest)),
        ("pnpm vitest run", Some(Jest)),
        ("bunx vitest", Some(Jest)),
        ("pnpm exec vitest run", Some(Jest)),
        ("go test ./...", Some(GoTest)),
        ("npx tsc --noEmit", Some(Tsc)),
        ("./node_modules/.bin/tsc -p .", Some(Tsc)),
        ("eslint .", Some(Eslint)),
        ("pnpm eslint src", Some(Eslint)),
        // Unknown script contents or other subcommands.
        ("npm run lint", None),
        ("pnpm run test", None),
        ("make test", None),
        ("cargo fmt", None),
        ("cargo run", None),
        ("go build ./...", None),
        ("ls -la", None),
        // Only the last command of the line is filtered; a pipe hides the tool.
        ("cd web && npx jest", Some(Jest)),
        ("cargo test && cargo clippy", Some(Cargo)),
        // Output of different tools would be parsed by one parser: no filter.
        ("cargo test && pytest", None),
        ("pytest && cargo build", None),
        ("pytest; eslint .", None),
        ("cargo test; tsc", None),
        ("cargo test || npx tsc --noEmit", None),
        ("cargo test && cargo clippy", Some(Cargo)),
        ("cargo test 2>&1 | tail -n 20", None),
        ("cargo test | tee out.log", None),
        ("cargo test; echo done", None),
    ];
    for (cmd, want) in cases {
        assert_eq!(kind(cmd), *want, "{cmd}");
    }
}

fn first(s: &Summary) -> &Item {
    s.items.first().expect("at least one item")
}

fn counts(s: &Summary) -> (Option<u32>, u32, u32, u32) {
    (s.passed, s.failed, s.errors, s.warnings)
}

#[test]
fn parses_cargo_test_fail() {
    let s = apply(FilterKind::Cargo, CARGO_TEST_FAIL).expect("parsed");
    assert_eq!(s.tool, "cargo");
    assert_eq!(counts(&s), (Some(10), 2, 0, 1));
    assert_eq!(s.items.len(), 3);
    let i = first(&s);
    assert_eq!(i.name, "budget::tests::rejects_over_budget");
    assert_eq!(
        i.location.as_deref(),
        Some("crates/hx-core/src/budget.rs:88:9")
    );
    assert_eq!(
        i.message,
        "assertion `left == right` failed: spending past the limit must fail"
    );
    assert!(
        i.detail.iter().any(|l| l.contains("left: Ok(())")),
        "{:?}",
        i.detail
    );
    assert!(
        !i.detail.iter().any(|l| l.contains("rust_begin_unwind")),
        "backtrace is noise: {:?}",
        i.detail
    );
    assert_eq!(s.items[1].name, "ledger::tests::sums_by_model");
    assert_eq!(
        s.items[1].location.as_deref(),
        Some("crates/hx-core/src/ledger.rs:121:14")
    );
    // Warnings come after failures.
    assert_eq!(s.items[2].name, "unused_variables");
    assert_eq!(
        s.items[2].location.as_deref(),
        Some("crates/hx-core/src/ledger.rs:42:9")
    );
    assert_eq!(s.items[2].message, "unused variable: `idx`");
}

#[test]
fn parses_cargo_build_err() {
    let s = apply(FilterKind::Cargo, CARGO_BUILD_ERR).expect("parsed");
    assert_eq!(counts(&s), (None, 0, 3, 1));
    assert_eq!(s.items.len(), 4, "{:?}", s.items);
    let i = first(&s);
    assert_eq!(i.name, "E0308");
    assert_eq!(
        i.location.as_deref(),
        Some("crates/hx-tools/src/bash.rs:212:34")
    );
    assert_eq!(i.message, "mismatched types");
    assert!(i.detail.iter().any(|l| l.contains("cap_output")));
    assert_eq!(s.items[3].name, "unused_imports");
}

#[test]
fn parses_clippy_warn() {
    let s = apply(FilterKind::Cargo, CLIPPY_WARN).expect("parsed");
    assert_eq!(counts(&s), (None, 0, 0, 4));
    let i = first(&s);
    assert_eq!(i.name, "clippy::needless_return");
    assert_eq!(
        i.location.as_deref(),
        Some("crates/hx-tools/src/find.rs:57:9")
    );
    assert_eq!(i.message, "unneeded `return` statement");
    let names: Vec<&str> = s.items.iter().map(|i| i.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "clippy::needless_return",
            "clippy::redundant_clone",
            "clippy::if_same_then_else",
            "clippy::manual_map"
        ]
    );
}

#[test]
fn parses_pytest_fail() {
    let s = apply(FilterKind::Pytest, PYTEST_FAIL).expect("parsed");
    assert_eq!(s.tool, "pytest");
    assert_eq!(counts(&s), (Some(11), 2, 1, 1));
    assert_eq!(s.items.len(), 3);
    let i = first(&s);
    assert_eq!(i.name, "tests/test_parse.py::test_parse_empty");
    assert_eq!(i.location.as_deref(), Some("tests/test_parse.py:12"));
    assert_eq!(i.message, "AssertionError: assert None == []");
    assert!(
        i.detail
            .iter()
            .any(|l| l.contains("assert parse(\"\") == []"))
    );
    assert_eq!(
        s.items[1].name,
        "tests/test_io.py::TestReader::test_reads_header"
    );
    assert_eq!(s.items[1].location.as_deref(), Some("tests/test_io.py:33"));
    assert_eq!(s.items[1].message, "KeyError: 'name'");
    assert_eq!(s.items[2].name, "tests/test_io.py::test_writes_file");
    assert_eq!(s.items[2].location.as_deref(), Some("tests/conftest.py:21"));
}

#[test]
fn parses_pytest_quiet_fail() {
    let s = apply(FilterKind::Pytest, PYTEST_QUIET_FAIL).expect("parsed");
    assert_eq!(counts(&s), (Some(10), 3, 0, 0));
    let i = first(&s);
    assert_eq!(i.name, "tests/test_tokens.py::test_token_limit");
    assert_eq!(i.location.as_deref(), Some("tests/test_tokens.py:18"));
    assert_eq!(i.message, "assert 5000 <= 4096");
    assert_eq!(
        s.items[1].name,
        "tests/test_tokens.py::test_split_respects_boundaries[3-expected1]"
    );
    assert_eq!(
        s.items[1].location.as_deref(),
        Some("tests/test_tokens.py:31")
    );
    assert_eq!(
        s.items[2].location.as_deref(),
        Some("tests/test_report.py:44")
    );
}

#[test]
fn parses_jest_fail() {
    let s = apply(FilterKind::Jest, JEST_FAIL).expect("parsed");
    assert_eq!(s.tool, "jest");
    assert_eq!(counts(&s), (Some(14), 3, 1, 0));
    assert_eq!(s.items.len(), 4, "{:?}", s.items);
    let i = first(&s);
    assert_eq!(i.name, "parse › handles empty input");
    assert_eq!(i.location.as_deref(), Some("src/parse.test.ts:12:24"));
    assert_eq!(
        i.message,
        "expect(received).toEqual(expected) // deep equality"
    );
    assert!(i.detail.iter().any(|l| l.contains("Received: undefined")));
    // The second block's first `at` frame is the test file, not node_modules.
    assert_eq!(
        s.items[1].location.as_deref(),
        Some("src/parse.test.ts:16:32")
    );
    assert_eq!(s.items[3].name, "Test suite failed to run");
    assert_eq!(s.items[3].location.as_deref(), Some("src/reader.ts:9:1"));
    assert_eq!(
        s.items[3].message,
        "Cannot find module './header' from 'src/reader.ts'"
    );
}

#[test]
fn parses_vitest_fail() {
    let s = apply(FilterKind::Jest, VITEST_FAIL).expect("parsed");
    assert_eq!(s.tool, "vitest");
    assert_eq!(counts(&s), (Some(10), 3, 0, 0));
    assert_eq!(s.items.len(), 3);
    let i = first(&s);
    assert_eq!(i.name, "parse > handles empty input");
    assert_eq!(i.location.as_deref(), Some("src/parse.test.ts:12:24"));
    assert_eq!(
        i.message,
        "AssertionError: expected undefined to deeply equal []"
    );
    assert!(i.detail.iter().any(|l| l.contains("+ Received:")));
    assert!(
        !i.detail.iter().any(|l| l.contains('⎯')),
        "separator leaked: {:?}",
        i.detail
    );
    assert_eq!(s.items[2].name, "parse > reports line numbers");
}

#[test]
fn parses_gotest_fail() {
    let s = apply(FilterKind::GoTest, GOTEST_FAIL).expect("parsed");
    assert_eq!(s.tool, "go test");
    // Parent tests whose subtests failed are not listed separately.
    assert_eq!(counts(&s), (Some(15), 5, 0, 0));
    let names: Vec<&str> = s.items.iter().map(|i| i.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "TestParseEmpty",
            "TestReader/header",
            "TestWriterFlush",
            "TestIndexRebuild/single_file",
            "TestIndexRebuild/nested_dirs",
        ]
    );
    let i = first(&s);
    assert_eq!(i.location.as_deref(), Some("parse_test.go:14"));
    assert_eq!(i.message, "ParseEmpty() = <nil>, want []string{}");
    assert_eq!(s.items[1].location.as_deref(), Some("reader_test.go:31"));
    assert_eq!(s.items[1].message, "header name = \"\", want \"alpha\"");
    assert_eq!(s.items[1].detail.len(), 1, "{:?}", s.items[1].detail);
    assert_eq!(s.items[2].detail.len(), 2);
}

#[test]
fn parses_gotest_build_errors_and_panics() {
    let build = "# example.com/proj/parse\n./parse.go:12:5: undefined: foo\n./parse.go:20:9: cannot use x (variable of type int) as string value in argument to f\nFAIL\texample.com/proj/parse [build failed]\nFAIL\n";
    let s = apply(FilterKind::GoTest, build).expect("parsed");
    assert_eq!(counts(&s), (None, 0, 2, 0));
    assert_eq!(first(&s).location.as_deref(), Some("./parse.go:12:5"));
    assert_eq!(first(&s).message, "undefined: foo");

    let panicked = "--- FAIL: TestIndex (0.00s)\npanic: runtime error: index out of range [3] with length 3 [recovered]\n\tpanic: runtime error: index out of range [3] with length 3\n\ngoroutine 7 [running]:\ntesting.tRunner.func1.2({0x1, 0x2})\nFAIL\texample.com/proj/parse\t0.004s\nFAIL\n";
    let s = apply(FilterKind::GoTest, panicked).expect("parsed");
    assert_eq!(s.failed, 1);
    assert_eq!(s.errors, 1);
    let p = s
        .items
        .iter()
        .find(|i| i.name == "panic")
        .expect("panic item");
    assert!(p.message.starts_with("runtime error: index out of range"));
}

#[test]
fn parses_tsc_err() {
    let s = apply(FilterKind::Tsc, TSC_ERR).expect("parsed");
    assert_eq!(s.tool, "tsc");
    assert_eq!(counts(&s), (None, 0, 20, 0));
    assert_eq!(s.items.len(), 20);
    let i = first(&s);
    assert_eq!(i.name, "TS2322");
    assert_eq!(i.location.as_deref(), Some("src/parse.ts:12:5"));
    assert_eq!(
        i.message,
        "Type 'string' is not assignable to type 'number'."
    );
    assert!(i.detail.is_empty());
    // Continuation lines belong to the error above them, the summary table to nobody.
    assert_eq!(s.items[1].detail.len(), 2, "{:?}", s.items[1].detail);
    assert!(s.items.last().unwrap().detail.is_empty());
}

#[test]
fn parses_eslint_err() {
    let s = apply(FilterKind::Eslint, ESLINT_ERR).expect("parsed");
    assert_eq!(s.tool, "eslint");
    assert_eq!(counts(&s), (None, 0, 12, 7));
    assert_eq!(s.items.len(), 19);
    let i = first(&s);
    assert_eq!(i.name, "@typescript-eslint/no-unused-vars");
    assert_eq!(
        i.location.as_deref(),
        Some("/home/dev/proj/src/parse.ts:12:5")
    );
    assert_eq!(i.message, "'idx' is assigned a value but never used");
    let parse_err = s
        .items
        .iter()
        .find(|i| i.message.starts_with("Parsing error"))
        .expect("parse error item");
    assert_eq!(parse_err.name, "error");
    assert_eq!(
        parse_err.location.as_deref(),
        Some("/home/dev/proj/src/util/strings.ts:1:1")
    );
}

#[test]
fn unrecognized_output_yields_no_summary() {
    let junk = "line of noise\n".repeat(500);
    for kind in [
        FilterKind::Cargo,
        FilterKind::Pytest,
        FilterKind::Jest,
        FilterKind::GoTest,
        FilterKind::Tsc,
        FilterKind::Eslint,
    ] {
        assert_eq!(apply(kind, &junk), None, "{kind:?}");
    }
}

#[test]
fn ansi_colors_are_ignored() {
    let colored = TSC_ERR.replace("error TS", "\x1b[31merror\x1b[0m TS");
    // The marker is split by an escape, so only stripping makes this parse.
    let s = apply(FilterKind::Tsc, &colored);
    assert_eq!(s.map(|s| s.errors), Some(20));
}

#[test]
fn detail_capped_at_20_lines() {
    let mut out = String::from(
        "running 1 test\ntest a::t ... FAILED\n\nfailures:\n\n---- a::t stdout ----\n\
         thread 'a::t' panicked at src/lib.rs:3:5:\nboom\n",
    );
    for n in 0..50 {
        out.push_str(&format!("context line {n}\n"));
    }
    out.push_str("\nfailures:\n    a::t\n\ntest result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n");
    let s = apply(FilterKind::Cargo, &out).expect("parsed");
    assert_eq!(first(&s).detail.len(), 20);
    assert_eq!(first(&s).detail[0], "context line 0");
    let rendered = s.render(Path::new(".hx/tmp/spill/x.txt"));
    let detail_lines = rendered.lines().filter(|l| l.starts_with("    ")).count();
    assert_eq!(detail_lines, 20, "{rendered}");

    // The cap also holds for an item built with more lines than that.
    let big = Summary {
        tool: "pytest",
        passed: None,
        failed: 1,
        errors: 0,
        warnings: 0,
        items: vec![Item {
            name: "t".into(),
            location: None,
            message: "m".into(),
            detail: (0..40).map(|n| format!("d{n}")).collect(),
        }],
    };
    let rendered = big.render(Path::new("s.txt"));
    assert_eq!(
        rendered.lines().filter(|l| l.starts_with("    ")).count(),
        20
    );
}

#[test]
fn render_includes_spill_path_and_counts() {
    let s = Summary {
        tool: "cargo",
        passed: Some(10),
        failed: 2,
        errors: 1,
        warnings: 3,
        items: vec![
            Item {
                name: "budget::rejects".into(),
                location: Some("src/budget.rs:88:9".into()),
                message: "boom".into(),
                detail: vec!["  left: Ok(())".into(), " right: Err(OverBudget)".into()],
            },
            Item {
                name: "E0308".into(),
                location: None,
                message: "mismatched types".into(),
                detail: Vec::new(),
            },
        ],
    };
    let out = s.render(Path::new(".hx/tmp/spill/abc.txt"));
    // Detail lines keep their own indentation under a 4-space margin.
    assert_eq!(
        out,
        [
            "cargo summary: 10 passed, 2 failed, 1 errors, 3 warnings",
            "- budget::rejects (src/budget.rs:88:9): boom",
            "      left: Ok(())",
            "     right: Err(OverBudget)",
            "- E0308: mismatched types",
            "[full output saved to .hx/tmp/spill/abc.txt]",
        ]
        .join("\n")
    );
    let unknown = Summary {
        passed: None,
        items: Vec::new(),
        ..s
    };
    let out = unknown.render(Path::new("s.txt"));
    assert_eq!(
        out,
        "cargo summary: 2 failed, 1 errors, 3 warnings\n[full output saved to s.txt]"
    );
}

// ---- bash integration --------------------------------------------------------

/// Run `command` in a fresh sandbox whose root holds `out.txt` with `content`. The
/// tool under test is a shell function that prints it, so no toolchain is needed.
async fn run(content: &str, command: &str, raw: Option<bool>) -> (TempDir, ToolResult) {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("out.txt"), content).unwrap();
    let sb = LocalSandbox::new(dir.path().to_path_buf()).unwrap();
    let cx = ToolCx::new(EditFormat::StrReplace, dir.path().join(".spill"));
    let mut args: Value = json!({ "command": command });
    if let Some(raw) = raw {
        args["raw"] = json!(raw);
    }
    let r = BashTool.run(args, &sb, &cx).await;
    (dir, r)
}

fn spill_path(output: &str) -> &str {
    let line = output
        .lines()
        .find(|l| l.starts_with("[full output saved to "))
        .unwrap_or_else(|| panic!("no spill line in {output}"));
    line.trim_start_matches("[full output saved to ")
        .trim_end_matches(']')
}

#[tokio::test]
async fn large_output_is_summarized_with_spill() {
    let (dir, r) = run(
        CARGO_TEST_FAIL,
        "cargo() { cat out.txt; }; cargo test",
        None,
    )
    .await;
    assert!(!r.is_error, "{}", r.output);
    assert!(
        r.output
            .starts_with("cargo summary: 10 passed, 2 failed, 0 errors, 1 warnings\n"),
        "{}",
        r.output
    );
    assert!(
        r.output
            .contains("- budget::tests::rejects_over_budget (crates/hx-core/src/budget.rs:88:9):")
    );
    assert!(!r.output.contains("rust_begin_unwind"), "{}", r.output);
    assert!(
        r.output.trim_end().ends_with("exit code: 0"),
        "{}",
        r.output
    );
    let spilled = std::fs::read_to_string(dir.path().join(spill_path(&r.output))).unwrap();
    assert_eq!(spilled, CARGO_TEST_FAIL);
}

#[tokio::test]
async fn output_under_2kb_not_filtered() {
    let small = "running 1 test\ntest a::t ... FAILED\n\nfailures:\n\n---- a::t stdout ----\nthread 'a::t' panicked at src/lib.rs:3:5:\nboom\n\ntest result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n";
    assert!(small.len() < 2048);
    let (_dir, r) = run(small, "cargo() { cat out.txt; }; cargo test", None).await;
    assert!(r.output.starts_with(small), "{}", r.output);
    assert!(!r.output.contains("summary:"), "{}", r.output);
    assert!(!r.output.contains("[full output saved"), "{}", r.output);
}

#[tokio::test]
async fn unparseable_output_falls_back_to_head_tail_cap() {
    let junk = "noise noise noise noise\n".repeat(3000);
    let (dir, r) = run(&junk, "pytest() { cat out.txt; }; pytest -q", None).await;
    assert!(!r.output.contains("summary:"), "{}", &r.output[..200]);
    assert!(r.output.contains("bytes omitted"), "{}", &r.output[..200]);
    let spilled = std::fs::read_to_string(dir.path().join(spill_path(&r.output))).unwrap();
    assert_eq!(spilled, junk);
    assert!(r.output.trim_end().ends_with("exit code: 0"));
}

#[tokio::test]
async fn raw_true_disables_filtering() {
    let cmd = "cargo() { cat out.txt; }; cargo test";
    let (_dir, r) = run(CARGO_TEST_FAIL, cmd, Some(true)).await;
    assert!(!r.output.contains("summary:"), "{}", r.output);
    assert!(r.output.starts_with(CARGO_TEST_FAIL), "{}", r.output);
    let (_dir, r) = run(CARGO_TEST_FAIL, cmd, Some(false)).await;
    assert!(r.output.starts_with("cargo summary:"), "{}", r.output);
}

#[tokio::test]
async fn piped_output_is_not_filtered() {
    let (_dir, r) = run(
        CARGO_TEST_FAIL,
        "cargo() { cat out.txt; }; cargo test 2>&1 | cat",
        None,
    )
    .await;
    assert!(r.output.starts_with(CARGO_TEST_FAIL), "{}", r.output);
}

// ---- a filter must never hide a failure ---------------------------------------

#[test]
fn cargo_aborted_test_binary_counts_as_error() {
    let out = "     Running unittests src/lib.rs (target/debug/deps/x-1)\n\nrunning 3 tests\ntest a ... ok\ntest b ... ok\ntest c ... ok\n\ntest result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n\n     Running tests/db.rs (target/debug/deps/db-2)\n\nrunning 2 tests\ntest db::connects ... ok\nerror: test failed, to rerun pass `-p x --test db`\n\nCaused by:\n  process didn't exit successfully: `/w/target/debug/deps/db-2` (signal: 6, SIGABRT: process abort signal)\n";
    let s = apply(FilterKind::Cargo, out).expect("parsed");
    assert_eq!(counts(&s), (Some(3), 0, 1, 0), "{s:?}");
    assert!(first(&s).message.contains("SIGABRT"), "{s:?}");
    // Older cargo prints it as an `error:` header; still one error.
    let old = out.replace("Caused by:\n  process", "error: process");
    assert_eq!(apply(FilterKind::Cargo, &old).unwrap().errors, 1);
}

#[test]
fn go_package_failure_without_failing_test_counts_as_error() {
    let out = "ok  \texample.com/proj/lex\t0.002s\n2024/05/01 10:00:00 fatal: cannot open db\nexit status 1\nFAIL\texample.com/proj/db\t0.012s\nFAIL\n";
    let s = apply(FilterKind::GoTest, out).expect("parsed");
    assert_eq!((s.failed, s.errors), (0, 1), "{s:?}");
    assert!(first(&s).message.contains("example.com/proj/db"));
    assert!(
        first(&s)
            .detail
            .iter()
            .any(|l| l.contains("cannot open db"))
    );
    // A package whose failure is explained by a failing test is not double counted.
    let s = apply(FilterKind::GoTest, GOTEST_FAIL).unwrap();
    assert_eq!((s.failed, s.errors), (5, 0));
}

#[test]
fn vitest_unhandled_errors_count_as_errors() {
    let out = "\n RUN  v2.1.8 /home/dev/proj\n\n \u{2713} src/a.test.ts (3 tests) 4ms\n\n\
         \u{23af}\u{23af}\u{23af}\u{23af}\u{23af}\u{23af} Unhandled Errors \u{23af}\u{23af}\u{23af}\u{23af}\u{23af}\u{23af}\n\n\
         Vitest caught 1 unhandled error during the test run.\n\n\
         \u{23af}\u{23af}\u{23af}\u{23af}\u{23af}\u{23af} Unhandled Rejection \u{23af}\u{23af}\u{23af}\u{23af}\u{23af}\u{23af}\n\
         Error: boom\n \u{276f} src/x.ts:3:9\n\
         This error originated in \"src/a.test.ts\" test file.\n\
         \u{23af}\u{23af}\u{23af}\u{23af}\u{23af}\u{23af}\u{23af}\u{23af}\u{23af}\u{23af}\u{23af}\u{23af}\u{23af}\u{23af}\u{23af}\n\n\
          Test Files  1 passed (1)\n      Tests  3 passed (3)\n     Errors  1 error\n   Duration  412ms\n";
    let s = apply(FilterKind::Jest, out).expect("parsed");
    assert_eq!(s.tool, "vitest");
    assert_eq!(counts(&s), (Some(3), 0, 1, 0), "{s:?}");
    assert_eq!(first(&s).message, "Error: boom");
    assert_eq!(first(&s).location.as_deref(), Some("src/x.ts:3:9"));
}

#[test]
fn pytest_ids_with_spaces_are_kept() {
    let out = "=========================== short test summary info ============================\n\
               FAILED tests/t.py::test_p[hello world] - assert 1 == 2\n\
               FAILED tests/t.py::test_q[a - b] - KeyError: 'x'\n\
               ERROR tests/t.py::test_r[x y]\n\
               ============= 2 failed, 1 passed, 1 error in 0.10s =============\n";
    let s = apply(FilterKind::Pytest, out).expect("parsed");
    assert_eq!(counts(&s), (Some(1), 2, 1, 0));
    assert_eq!(s.items.len(), 3);
    assert_eq!(first(&s).name, "tests/t.py::test_p[hello world]");
    assert_eq!(first(&s).message, "assert 1 == 2");
}

#[test]
fn fewer_items_than_reported_failures_gives_no_summary() {
    let out = "=========================== short test summary info ============================\n\
               FAILED tests/t.py::test_a - assert 1 == 2\n\
               FAILED tests/t.py::test_b - assert 2 == 3\n\
               ============= 3 failed, 1 passed in 0.10s =============\n";
    assert_eq!(apply(FilterKind::Pytest, out), None);
}

#[tokio::test]
async fn failed_command_with_clean_summary_keeps_plain_output() {
    // Only warnings parsed, but the command failed: something the parser missed.
    let cmd = "cargo() { cat out.txt; return 101; }; cargo clippy";
    let (_dir, r) = run(CLIPPY_WARN, cmd, None).await;
    assert!(r.is_error);
    assert!(r.output.starts_with(CLIPPY_WARN), "{}", r.output);
    assert!(!r.output.contains("summary:"), "{}", r.output);
    // The same output from a successful command is summarized.
    let (_dir, r) = run(CLIPPY_WARN, "cargo() { cat out.txt; }; cargo clippy", None).await;
    assert!(
        r.output
            .starts_with("cargo summary: 0 failed, 0 errors, 4 warnings")
    );
    // A failed command whose summary shows the failure is still summarized.
    let cmd = "cargo() { cat out.txt; return 101; }; cargo test";
    let (_dir, r) = run(CARGO_TEST_FAIL, cmd, None).await;
    assert!(r.output.starts_with("cargo summary:"), "{}", r.output);
}

#[tokio::test]
async fn timed_out_command_is_not_filtered() {
    let cmd = "cargo() { cat out.txt; sleep 30; }; cargo test";
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("out.txt"), CARGO_TEST_FAIL).unwrap();
    let sb = LocalSandbox::new(dir.path().to_path_buf()).unwrap();
    let cx = ToolCx::new(EditFormat::StrReplace, dir.path().join(".spill"));
    let r = BashTool
        .run(json!({ "command": cmd, "timeout_s": 1 }), &sb, &cx)
        .await;
    assert!(r.is_error);
    assert!(!r.output.contains("summary:"), "{}", r.output);
    assert!(r.output.contains("rust_begin_unwind"), "{}", r.output);
    assert!(r.output.contains("timed out"), "{}", r.output);
}
