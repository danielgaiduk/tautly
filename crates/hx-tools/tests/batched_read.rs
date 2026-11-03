mod common;

use std::path::Path;

use common::{Built, env};
use hx_sandbox::Sandbox;
use hx_tools::{AppliedEffect, ReadTool};
use serde_json::json;

const LIB: &str = "fn alpha() {\n    1\n}\nfn beta() {\n    2\n}\n";

#[tokio::test]
async fn batched_read_returns_each_target_with_header_and_shares_30kb_cap() {
    let e = env(
        &[
            ("a.txt", b"one\ntwo\n" as &[u8]),
            ("b.txt", b"x1\nx2\nx3\nx4\n"),
        ],
        Built::None,
    )
    .await;
    let r = e
        .run(
            &ReadTool,
            json!({"targets": [{"path": "a.txt"}, {"path": "b.txt", "start_line": 2, "end_line": 3}]}),
        )
        .await;
    assert!(!r.is_error, "{}", r.output);
    assert_eq!(
        r.output,
        "== a.txt ==\n1\tone\n2\ttwo\n\n== b.txt:2-3 ==\n2\tx2\n3\tx3"
    );

    // Two files that would each fill the cap on their own share it.
    let big = |tag: &str| -> String {
        (0..1000)
            .map(|i| format!("{tag} line {i:04} padding\n"))
            .collect()
    };
    e.put("big1.txt", big("one").as_bytes()).await;
    e.put("big2.txt", big("two").as_bytes()).await;
    e.put("small.txt", b"tiny\n").await;
    let r = e
        .run(
            &ReadTool,
            json!({"targets": [{"path": "big1.txt"}, {"path": "big2.txt"}, {"path": "small.txt"}]}),
        )
        .await;
    assert!(!r.is_error);
    assert!(r.output.len() <= 31 * 1024, "{} bytes", r.output.len());
    for header in ["== big1.txt ==", "== big2.txt ==", "== small.txt =="] {
        assert!(r.output.contains(header), "missing {header}");
    }
    assert!(
        r.output.contains("1\ttiny"),
        "the small file is shown whole"
    );
    assert!(r.output.contains("one line 0000") && r.output.contains("two line 0000"));
    assert!(r.output.contains("truncated"), "{}", r.output.len());
}

#[tokio::test]
async fn batched_read_symbol_target_uses_index() {
    let e = env(
        &[
            ("lib.rs", LIB.as_bytes()),
            ("a.rs", b"fn dup() {}\n"),
            ("b.rs", b"fn dup() {}\n"),
        ],
        Built::Refreshed,
    )
    .await;
    let r = e
        .run(
            &ReadTool,
            json!({"targets": [{"symbol": "beta"}, {"path": "lib.rs", "start_line": 1, "end_line": 1}]}),
        )
        .await;
    assert!(!r.is_error, "{}", r.output);
    assert_eq!(
        r.output,
        "== symbol beta (lib.rs:4-6) ==\n4\tfn beta() {\n5\t    2\n6\t}\n\n== lib.rs:1-1 ==\n1\tfn alpha() {"
    );
    // Per-target failures are reported in place and do not hide the other targets.
    let r = e
        .run(
            &ReadTool,
            json!({"targets": [{"symbol": "nosuch"}, {"symbol": "dup"}, {"symbol": "alpha", "kind": "function"}]}),
        )
        .await;
    assert!(!r.is_error, "{}", r.output);
    assert!(r.output.contains("== symbol nosuch =="), "{}", r.output);
    assert!(r.output.contains("no definition of `nosuch`"));
    assert!(r.output.contains("2 definitions of `dup`"), "{}", r.output);
    assert!(r.output.contains("a.rs:1-1") && r.output.contains("b.rs:1-1"));
    assert!(r.output.contains("== symbol alpha (lib.rs:1-3) =="));
    // Without a ready index a symbol target says so.
    let e = env(&[("lib.rs", LIB.as_bytes())], Built::None).await;
    let r = e
        .run(&ReadTool, json!({"targets": [{"symbol": "beta"}]}))
        .await;
    assert!(r.is_error, "{}", r.output);
    assert!(r.output.contains("index"), "{}", r.output);
}

#[tokio::test]
async fn batched_read_rejects_more_than_10_targets() {
    let e = env(&[("a.txt", b"x\n" as &[u8])], Built::None).await;
    let eleven: Vec<_> = (0..11).map(|_| json!({"path": "a.txt"})).collect();
    let r = e.run(&ReadTool, json!({"targets": eleven})).await;
    assert!(r.is_error);
    assert!(r.output.contains("at most 10"), "{}", r.output);
    let ten: Vec<_> = (0..10).map(|_| json!({"path": "a.txt"})).collect();
    let r = e.run(&ReadTool, json!({"targets": ten})).await;
    assert!(!r.is_error, "{}", r.output);
}

#[tokio::test]
async fn batched_read_rejects_path_with_targets_and_empty_requests() {
    let e = env(&[("a.txt", b"x\n" as &[u8])], Built::None).await;
    for args in [
        json!({"path": "a.txt", "targets": [{"path": "a.txt"}]}),
        json!({"targets": []}),
        json!({}),
        json!({"targets": [{"path": "a.txt", "symbol": "x"}]}),
    ] {
        let r = e.run(&ReadTool, args.clone()).await;
        assert!(r.is_error, "{args}: {}", r.output);
    }
    let r = e.run(&ReadTool, json!({"path": "a.txt"})).await;
    assert_eq!(r.output, "1\tx");
}

#[tokio::test]
async fn batched_read_records_each_file_in_tracker() {
    let e = env(
        &[("a.txt", b"one\n" as &[u8]), ("d/b.txt", b"two\n")],
        Built::None,
    )
    .await;
    let r = e
        .run(
            &ReadTool,
            json!({"targets": [{"path": "a.txt"}, {"path": "d/b.txt"}, {"path": "missing.txt"}]}),
        )
        .await;
    assert!(!r.is_error, "{}", r.output);
    assert!(r.output.contains("== missing.txt =="), "{}", r.output);
    for rel in ["a.txt", "d/b.txt"] {
        let full = e.sb.resolve(Path::new(rel)).unwrap();
        let (_, hash) = e.cx.reads().last(&full).expect("tracked");
        assert_eq!(
            hash,
            blake3::hash(&e.sb.read(&full).await.unwrap()),
            "{rel}"
        );
        assert!(
            r.effects
                .iter()
                .any(|f| matches!(f, AppliedEffect::Read(p, h) if *p == full && *h == hash)),
            "{rel}"
        );
    }
    let missing = e.sb.resolve(Path::new("missing.txt")).unwrap();
    assert!(e.cx.reads().last(&missing).is_none());
}

#[tokio::test]
async fn batched_read_gives_small_targets_their_full_size_before_sharing_the_rest() {
    // About 20.8 KB and 2.5 KB: together under the 30 KB cap, though the big one is over an
    // equal share of it.
    let big: String = (0..520)
        .map(|i| format!("big {i:04} {}\n", "x".repeat(30)))
        .collect();
    let small: String = (0..60)
        .map(|i| format!("s{i:02} {}\n", "y".repeat(35)))
        .collect();
    let e = env(
        &[("big.txt", big.as_bytes()), ("small.txt", small.as_bytes())],
        Built::None,
    )
    .await;
    let env_ref = &e;
    // `full`: these are repeated reads of the same files, which would otherwise be diffed.
    let run = |targets: serde_json::Value| async move {
        let r = env_ref
            .run(&ReadTool, json!({ "targets": targets, "full": true }))
            .await;
        assert!(!r.is_error, "{}", r.output);
        r.output
    };
    let large_first = run(json!([{"path": "big.txt"}, {"path": "small.txt"}])).await;
    let small_first = run(json!([{"path": "small.txt"}, {"path": "big.txt"}])).await;
    for out in [&large_first, &small_first] {
        assert!(
            !out.contains("truncated") && !out.contains("omitted"),
            "{}",
            out.len()
        );
        assert!(out.contains("big 0519") && out.contains("s59"));
    }
    assert_eq!(large_first.len(), small_first.len());

    // Over the cap in total, the small target is still whole and the big ones share the rest.
    e.put("big2.txt", big.as_bytes()).await;
    let out = run(json!([{"path": "big.txt"}, {"path": "big2.txt"}, {"path": "small.txt"}])).await;
    assert!(out.len() <= 31 * 1024, "{}", out.len());
    assert!(out.contains("s59") && !out.contains("== small.txt ==\n1\t[error]"));
    assert_eq!(out.matches("truncated").count(), 2, "{out}");
}
