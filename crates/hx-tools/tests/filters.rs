// Tests write a fake tool's output to a temp dir; the lint targets src.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use hx_provider::EditFormat;
use hx_sandbox::{LocalSandbox, parse_commands};
use hx_tools::filters::{
    FilterKind, Item, Summary, apply, detect, detect_command, has_unparsed_crash,
};
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

/// The production path (`bash` uses `detect_command`).
fn kind(cmd: &str) -> Option<FilterKind> {
    detect_command(cmd)
}

/// `detect` alone, which cannot see operators and so allows only harmless prefixes.
fn kind_strict(cmd: &str) -> Option<FilterKind> {
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

/// Fake tools: scripts that print `out.txt` from the working directory, then sleep for
/// `$SLEEP` seconds if set and exit with `$EXIT` (default 0). Written once, before any
/// test runs a command, so a script is never executed while another is being written.
static FAKE_TOOLS: LazyLock<PathBuf> = LazyLock::new(|| {
    let dir = std::env::temp_dir().join(format!("hx-filter-fakes-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    for tool in ["cargo", "pytest", "ruff"] {
        let path = dir.join(tool);
        std::fs::write(
            &path,
            "#!/bin/sh\ncat out.txt\n[ -n \"$SLEEP\" ] && sleep \"$SLEEP\"\nexit \"${EXIT:-0}\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    dir
});

/// A command line running fake `tool` with `args`, after the `env` assignments.
fn fake(env: &str, tool: &str, args: &str) -> String {
    format!("{env} '{}' {args}", FAKE_TOOLS.join(tool).display())
}

/// Run `command` in a fresh sandbox whose root holds `out.txt` with `content`.
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
    let (dir, r) = run(CARGO_TEST_FAIL, &fake("", "cargo", "test"), None).await;
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
    let (_dir, r) = run(small, &fake("", "cargo", "test"), None).await;
    assert!(r.output.starts_with(small), "{}", r.output);
    assert!(!r.output.contains("summary:"), "{}", r.output);
    assert!(!r.output.contains("[full output saved"), "{}", r.output);
}

#[tokio::test]
async fn unparseable_output_falls_back_to_head_tail_cap() {
    let junk = "noise noise noise noise\n".repeat(3000);
    let (dir, r) = run(&junk, &fake("", "pytest", "-q"), None).await;
    assert!(!r.output.contains("summary:"), "{}", &r.output[..200]);
    assert!(r.output.contains("bytes omitted"), "{}", &r.output[..200]);
    let spilled = std::fs::read_to_string(dir.path().join(spill_path(&r.output))).unwrap();
    assert_eq!(spilled, junk);
    assert!(r.output.trim_end().ends_with("exit code: 0"));
}

#[tokio::test]
async fn raw_true_disables_filtering() {
    let cmd = fake("", "cargo", "test");
    let (_dir, r) = run(CARGO_TEST_FAIL, &cmd, Some(true)).await;
    assert!(!r.output.contains("summary:"), "{}", r.output);
    assert!(r.output.starts_with(CARGO_TEST_FAIL), "{}", r.output);
    let (_dir, r) = run(CARGO_TEST_FAIL, &cmd, Some(false)).await;
    assert!(r.output.starts_with("cargo summary:"), "{}", r.output);
}

#[tokio::test]
async fn piped_output_is_not_filtered() {
    let (_dir, r) = run(CARGO_TEST_FAIL, &fake("", "cargo", "test 2>&1 | cat"), None).await;
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
    let cmd = fake("EXIT=101", "cargo", "clippy");
    let (_dir, r) = run(CLIPPY_WARN, &cmd, None).await;
    assert!(r.is_error);
    assert!(r.output.starts_with(CLIPPY_WARN), "{}", r.output);
    assert!(!r.output.contains("summary:"), "{}", r.output);
    // The same output from a successful command is summarized.
    let (_dir, r) = run(CLIPPY_WARN, &fake("", "cargo", "clippy"), None).await;
    assert!(
        r.output
            .starts_with("cargo summary: 0 failed, 0 errors, 4 warnings")
    );
    // A failed command whose summary shows the failure is still summarized.
    let cmd = fake("EXIT=101", "cargo", "test");
    let (_dir, r) = run(CARGO_TEST_FAIL, &cmd, None).await;
    assert!(r.output.starts_with("cargo summary:"), "{}", r.output);
}

#[tokio::test]
async fn timed_out_command_is_not_filtered() {
    let cmd = fake("SLEEP=30", "cargo", "test");
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

// ---- fix round 2 ---------------------------------------------------------------

const CARGO_QUIET_WARN: &str = include_str!("fixtures/filters/cargo_quiet_warn.txt");
const CARGO_DUP: &str = include_str!("fixtures/filters/cargo_dup.txt");
const TSC_CRASH: &str = include_str!("fixtures/filters/tsc_crash.txt");
const GO_TWO_PANICS: &str = include_str!("fixtures/filters/go_two_panics.txt");
const PYTEST_INTERNAL: &str = include_str!("fixtures/filters/pytest_internal.txt");

#[test]
fn cargo_quiet_mode_recovers_failures_from_blocks() {
    let s = apply(FilterKind::Cargo, CARGO_QUIET_WARN).expect("parsed");
    assert_eq!(counts(&s), (Some(1), 1, 0, 1), "{s:?}");
    assert_eq!(first(&s).name, "b");
    assert_eq!(first(&s).location.as_deref(), Some("src/lib.rs:9:5"));
    assert_eq!(first(&s).message, "boom");
    assert_eq!(s.items[1].name, "unused_variables");
}

#[test]
fn cargo_same_test_name_in_two_binaries_gives_two_items() {
    let s = apply(FilterKind::Cargo, CARGO_DUP).expect("parsed");
    assert_eq!(counts(&s), (Some(0), 2, 0, 1), "{s:?}");
    assert_eq!(s.items.len(), 3);
    assert_eq!(s.items[0].location.as_deref(), Some("src/lib.rs:9:5"));
    assert_eq!(s.items[1].location.as_deref(), Some("tests/b.rs:4:5"));
    assert_eq!(s.items[1].message, "other failure");
}

#[test]
fn warnings_do_not_stand_in_for_missing_failures() {
    // One failure counted, none recoverable: only the warning item exists.
    let out = "warning: unused variable: `x`\n --> src/lib.rs:2:9\n  |\n2 |     let x = 1;\n  |         ^ help: prefix\n  |\n  = note: `#[warn(unused_variables)]` on by default\n\ntest result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n";
    assert_eq!(apply(FilterKind::Cargo, out), None);
    // The same holds for linters: every counted problem needs an item.
    let eslint = "/w/a.js\n  1:1  error  Unexpected console statement  no-console\n\n\u{2716} 3 problems (2 errors, 1 warning)\n";
    assert_eq!(apply(FilterKind::Eslint, eslint), None);
}

#[test]
fn go_each_panic_is_its_own_item() {
    let s = apply(FilterKind::GoTest, GO_TWO_PANICS).expect("parsed");
    assert_eq!(counts(&s), (None, 0, 2, 0), "{s:?}");
    assert_eq!(
        first(&s).message,
        "runtime error: index out of range [3] with length 3"
    );
    assert_eq!(s.items[1].message, "nil map assignment");
}

#[test]
fn detect_requires_harmless_prefixes_around_other_separators() {
    use FilterKind::*;
    let cases: &[(&str, Option<FilterKind>)] = &[
        ("ruff check .; pytest", None),
        ("lint || echo; pytest", None),
        ("go vet ./...; go test ./...", None),
        ("cargo fmt --check; cargo clippy", None),
        ("make build\npytest", None),
        ("ruff check . & pytest", None),
        ("cd web; npx jest", Some(Jest)),
        ("export CI=1; pytest", Some(Pytest)),
        // An earlier run of a tool can fail where the last one passes.
        ("pytest a; pytest b", None),
        ("pytest a || pytest b", None),
        ("cargo test; cargo clippy", None),
        // Separators are invisible here, so no earlier tool is allowed at all.
        ("cargo test && cargo clippy", None),
        ("cd web && npx jest", Some(Jest)),
    ];
    for (cmd, want) in cases {
        assert_eq!(kind_strict(cmd), *want, "{cmd}");
        if *cmd == "cargo test && cargo clippy" {
            continue;
        }
        assert_eq!(kind(cmd), *want, "{cmd}");
    }
}

#[test]
fn detect_command_allows_any_prefix_only_with_and_chains() {
    use FilterKind::*;
    let cases: &[(&str, Option<FilterKind>)] = &[
        ("cargo fmt --check && cargo clippy", Some(Cargo)),
        ("ruff check . && pytest", Some(Pytest)),
        ("cd web && npx jest", Some(Jest)),
        ("pytest 2>&1", Some(Pytest)),
        ("echo 'a;b' && pytest", Some(Pytest)),
        ("cargo test -- --nocapture 2>&1", Some(Cargo)),
        ("ruff check .; pytest", None),
        ("ruff check . || pytest", None),
        ("ruff check . | pytest", None),
        ("ruff check . & pytest", None),
        ("make lint\npytest", None),
        ("(ruff check .) && pytest", None),
        ("cargo test && pytest", None),
        ("cargo test 2>&1 | tail", None),
        ("cd web; npx jest", Some(Jest)),
        ("cargo test", Some(Cargo)),
        ("pytest a && pytest b", Some(Pytest)),
        ("pytest a; pytest b", None),
        ("pytest a || pytest b", None),
        ("cargo test; cargo clippy", None),
    ];
    for (cmd, want) in cases {
        assert_eq!(detect_command(cmd), *want, "{cmd}");
    }
}

#[test]
fn crash_markers_outside_items_are_found() {
    let s = apply(FilterKind::Tsc, TSC_CRASH).expect("parsed");
    assert_eq!(s.errors, 2);
    assert!(has_unparsed_crash(TSC_CRASH, &s));
    let s = apply(FilterKind::Pytest, PYTEST_INTERNAL).expect("parsed");
    assert!(has_unparsed_crash(PYTEST_INTERNAL, &s));
    // A parsed go panic is an item, not a hidden crash.
    let s = apply(FilterKind::GoTest, GO_TWO_PANICS).unwrap();
    assert!(!has_unparsed_crash(GO_TWO_PANICS, &s));
    // A traceback that is part of a failure's own detail is not hidden either.
    let s = apply(FilterKind::Pytest, PYTEST_FAIL).unwrap();
    assert!(!has_unparsed_crash(PYTEST_FAIL, &s));
}

#[tokio::test]
async fn crash_after_parsed_failures_keeps_plain_output_on_nonzero_exit() {
    // pytest's INTERNALERROR output (an exit code 3 crash) through the fake pytest.
    let padded = format!("{PYTEST_INTERNAL}{}", "   \n".repeat(600));
    let (_d, r) = run(&padded, &fake("EXIT=3", "pytest", "-q"), None).await;
    assert!(r.output.contains("INTERNALERROR> KeyError"), "{}", r.output);
    assert!(!r.output.contains("summary:"), "{}", r.output);
    // Same output with a zero exit (contrived) is summarized: the check is for failures.
    let (_d, r) = run(&padded, &fake("", "pytest", "-q"), None).await;
    assert!(r.output.starts_with("pytest summary:"), "{}", r.output);
}

#[tokio::test]
async fn failing_unrecognized_command_before_the_tool_is_not_hidden() {
    let cmd = format!(
        "{}; {}",
        fake("EXIT=1", "ruff", "check ."),
        fake("", "pytest", "")
    );
    let (_d, r) = run(PYTEST_FAIL, &cmd, None).await;
    assert!(!r.output.contains("summary:"), "{}", r.output);
    let cmd = format!(
        "{} && {}",
        fake("", "ruff", "check ."),
        fake("", "pytest", "")
    );
    let (_d, r) = run(PYTEST_FAIL, &cmd, None).await;
    assert!(r.output.contains("pytest summary:"), "{}", r.output);
}

const CARGO_RUNNING: &str = include_str!("fixtures/filters/cargo_running.txt");

#[test]
fn captured_output_that_looks_like_a_running_line_does_not_split_a_binary() {
    let s = apply(FilterKind::Cargo, CARGO_RUNNING).expect("parsed");
    assert_eq!(counts(&s), (Some(0), 2, 0, 0), "{s:?}");
    assert_eq!(s.items.len(), 2, "{:?}", s.items);
    assert_eq!(first(&s).name, "a");
    assert_eq!(first(&s).location.as_deref(), Some("src/lib.rs:9:5"));
    assert_eq!(first(&s).message, "real cause of a");
    // Cargo's own headers still end a binary.
    let two = "     Running unittests src/lib.rs (target/debug/deps/x-1)\n\n---- t stdout ----\nthread 't' panicked at a.rs:1:1:\nfirst\n\nfailures:\n    t\n\nerror: test failed, to rerun pass `--lib`\n     Running tests/b.rs (target/debug/deps/b-2)\n\n---- t stdout ----\nthread 't' panicked at b.rs:2:2:\nsecond\n\nfailures:\n    t\n\ntest result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n";
    let s = apply(FilterKind::Cargo, two).expect("parsed");
    let msgs: Vec<&str> = s.items.iter().map(|i| i.message.as_str()).collect();
    assert_eq!(msgs, ["first", "second"]);
}

#[test]
fn more_crash_markers_are_recognized() {
    let summary = apply(FilterKind::Pytest, PYTEST_QUIET_FAIL).unwrap();
    for marker in [
        "!!!!!!!!!!!!!!!!!!!! KeyboardInterrupt !!!!!!!!!!!!!!!!!!!!",
        "!!! stopping after 1 failures !!!",
        "Fatal Python error: Segmentation fault",
        "Segmentation fault (core dumped)",
        "RangeError: Maximum call stack size exceeded",
    ] {
        let out = format!("{PYTEST_QUIET_FAIL}{marker}\n");
        assert!(has_unparsed_crash(&out, &summary), "{marker}");
    }
    // Plain text with exclamation marks is not a banner.
    let out = format!("{PYTEST_QUIET_FAIL}!! not a banner\nWow!!!\n");
    assert!(!has_unparsed_crash(&out, &summary));
}
