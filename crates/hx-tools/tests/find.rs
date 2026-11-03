mod common;

use common::{Built, Env, env};
use hx_tools::FindTool;
use serde_json::{Value, json};

const ENGINE: &str = "fn backoff(attempt: u32) -> u64 {\n    attempt as u64 * 100\n}\n";

fn fixture() -> Vec<(&'static str, &'static [u8])> {
    vec![
        ("src/engine.rs", ENGINE.as_bytes()),
        (
            "docs/backoff.md",
            b"# Backoff policy\nwe use exponential backoff everywhere\n",
        ),
        ("notes.txt", b"remember the backoff strategy\n"),
        ("src/payment/retry.rs", b"pub fn run() {}\n"),
        ("src/payment/mod.rs", b"pub mod retry;\n"),
    ]
}

/// `(location, kind, snippet)` of every result row; header and note lines are skipped.
fn parse_rows(output: &str) -> Vec<(String, String, String)> {
    output
        .lines()
        .filter_map(|l| {
            let mut parts = l.splitn(3, "  ");
            let (loc, kind, snippet) = (parts.next()?, parts.next()?, parts.next()?);
            let (path, line) = loc.rsplit_once(':')?;
            line.parse::<usize>().ok()?;
            assert!(!path.is_empty(), "{l}");
            Some((loc.to_string(), kind.to_string(), snippet.to_string()))
        })
        .collect()
}

async fn find(e: &Env, args: Value) -> String {
    let r = e.run(&FindTool, args).await;
    assert!(!r.is_error, "{}", r.output);
    r.output
}

#[tokio::test]
async fn find_ranks_symbol_definition_over_path_over_text() {
    let e = env(&fixture(), Built::Full).await;
    let out = find(&e, json!({"query": "backoff"})).await;
    let rows = parse_rows(&out);
    let first = |kind: &str| rows.iter().position(|r| r.1 == kind);
    let (sym, path, text) = (first("function"), first("path"), first("text"));
    assert!(
        sym == Some(0) && path > sym && text > path,
        "symbol, then path, then text expected:\n{out}"
    );
    assert_eq!(
        rows[0],
        (
            "src/engine.rs:1".into(),
            "function".into(),
            "fn backoff(attempt: u32) -> u64".into()
        ),
        "{out}"
    );
    assert_eq!(rows[path.unwrap()].0, "docs/backoff.md:1", "{out}");
    // The definition line is reported once, as the symbol, not again as text.
    assert_eq!(
        rows.iter().filter(|r| r.0 == "src/engine.rs:1").count(),
        1,
        "{out}"
    );
    assert!(out.contains("docs/backoff.md:2  text  we use exponential backoff everywhere"));
    assert!(out.contains("notes.txt:1  text  remember the backoff strategy"));
}

#[tokio::test]
async fn find_fuzzy_path_match() {
    let e = env(&fixture(), Built::Full).await;
    let out = find(&e, json!({"query": "pymtretry"})).await;
    let rows = parse_rows(&out);
    assert_eq!(rows.len(), 1, "{out}");
    assert_eq!(rows[0].0, "src/payment/retry.rs:1", "{out}");
    assert_eq!(rows[0].1, "path");
    // Characters must appear in order.
    let out = find(&e, json!({"query": "yrtertmyp"})).await;
    assert!(parse_rows(&out).is_empty(), "{out}");
}

#[tokio::test]
async fn find_fuzzy_prefers_consecutive_and_segment_start_matches() {
    let e = env(
        &[
            ("src/a_long_directory/xretryx.rs", b"" as &[u8]),
            ("src/retry.rs", b""),
            ("r/e/t/r/y.rs", b""),
        ],
        Built::Refreshed,
    )
    .await;
    let out = find(&e, json!({"query": "retry", "kind": "path"})).await;
    let paths: Vec<String> = parse_rows(&out).into_iter().map(|r| r.0).collect();
    assert_eq!(paths[0], "src/retry.rs:1", "{out}");
    assert_eq!(paths.len(), 3, "{out}");
}

#[tokio::test]
async fn find_kind_filter() {
    let e = env(&fixture(), Built::Full).await;
    let kinds = |out: &str| -> Vec<String> {
        let mut k: Vec<String> = parse_rows(out).into_iter().map(|r| r.1).collect();
        k.dedup();
        k
    };
    let out = find(&e, json!({"query": "backoff", "kind": "symbol"})).await;
    assert_eq!(kinds(&out), ["function"], "{out}");
    assert_eq!(parse_rows(&out).len(), 1);
    let out = find(&e, json!({"query": "backoff", "kind": "path"})).await;
    assert_eq!(kinds(&out), ["path"], "{out}");
    assert_eq!(parse_rows(&out)[0].0, "docs/backoff.md:1");
    let out = find(&e, json!({"query": "backoff", "kind": "text"})).await;
    assert_eq!(kinds(&out), ["text"], "{out}");
    assert!(out.contains("src/engine.rs:1  text  fn backoff"), "{out}");
}

#[tokio::test]
async fn find_limit_and_snippet_format() {
    let many: String = (0..8).map(|i| format!("    needle {i}\n")).collect();
    let e = env(
        &[
            ("many.txt", many.as_bytes()),
            ("other.txt", b"a needle here\n"),
        ],
        Built::Full,
    )
    .await;
    // At most five text matches per file.
    let out = find(&e, json!({"query": "needle"})).await;
    let rows = parse_rows(&out);
    assert_eq!(
        rows.iter().filter(|r| r.0.starts_with("many.txt:")).count(),
        5,
        "{out}"
    );
    assert_eq!(rows.len(), 6, "{out}");
    // The format is exactly `path:line  kind  snippet`, snippet trimmed.
    assert_eq!(
        out.lines().next().unwrap(),
        "many.txt:1  text  needle 0",
        "{out}"
    );
    let out = find(&e, json!({"query": "needle", "limit": 3})).await;
    assert_eq!(parse_rows(&out).len(), 3, "{out}");
    assert!(out.contains("more results"), "{out}");
}

#[tokio::test]
async fn find_without_index_falls_back_to_grep_and_glob() {
    let e = env(&fixture(), Built::None).await;
    let out = find(&e, json!({"query": "backoff"})).await;
    assert_eq!(out.lines().next().unwrap(), "(index unavailable)", "{out}");
    let rows = parse_rows(&out);
    assert!(
        rows.iter()
            .any(|r| r.0 == "docs/backoff.md:1" && r.1 == "path"),
        "{out}"
    );
    assert!(
        rows.iter().any(|r| r.0 == "notes.txt:1"
            && r.1 == "text"
            && r.2 == "remember the backoff strategy"),
        "{out}"
    );
    // Symbol search degrades to a whole-word grep.
    let out = find(&e, json!({"query": "backoff", "kind": "symbol"})).await;
    assert!(out.starts_with("(index unavailable)"), "{out}");
    assert!(
        out.contains("src/engine.rs:1  mention  fn backoff"),
        "{out}"
    );
    // `backoff` also appears as a whole word in the notes and docs, which are mentions too.
    assert!(
        out.contains("notes.txt:1  mention  remember the backoff strategy"),
        "{out}"
    );
    assert!(
        !out.contains("  path  ") && !out.contains("  text  "),
        "{out}"
    );
}

#[tokio::test]
async fn find_works_on_a_refreshed_index_without_trigrams() {
    let e = env(&fixture(), Built::Refreshed).await;
    let out = find(&e, json!({"query": "strategy", "kind": "text"})).await;
    assert_eq!(parse_rows(&out)[0].0, "notes.txt:1", "{out}");
    assert!(!out.contains("index unavailable") && !out.contains("index building"));
}

#[tokio::test]
async fn find_rejects_bad_arguments() {
    let e = env(&fixture(), Built::Full).await;
    for args in [
        json!({"query": ""}),
        json!({}),
        json!({"query": "x", "kind": "everything"}),
        json!({"query": "x", "extra": 1}),
    ] {
        let r = e.run(&FindTool, args.clone()).await;
        assert!(r.is_error, "{args}: {}", r.output);
    }
    let r = e.run(&FindTool, json!({"query": "zzzzqqq"})).await;
    assert!(!r.is_error);
    assert!(r.output.contains("no matches"), "{}", r.output);
}
