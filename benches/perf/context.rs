//! The compaction cut search (`compaction_request`, which settles the cut with
//! `splits_tool_pair`) on a few thousand step-turns: its time must grow about linearly
//! with the history, not quadratically (CP3 review, T20).
//!
//! `cargo test --release -p tautly-perf --test context -- --ignored`

use std::time::{Duration, Instant};

use serde_json::json;
use tautly_context::{Context, Turn};
use tautly_perf::{median, ms, report};
use tautly_provider::{
    CacheStyle, Capabilities, Content, EditFormat, Message, ModelId, ReasoningStyle, Role,
};

/// A history of `n` step-turns: a tool call and its result each, about 150 tokens.
fn context(n: usize) -> Context {
    let history = (0..n)
        .map(|i| Turn {
            messages: vec![
                Message {
                    role: Role::Assistant,
                    content: vec![Content::ToolUse {
                        id: format!("c{i}"),
                        name: "read".into(),
                        input: json!({"path": format!("src/f{i}.rs")}),
                    }],
                },
                Message {
                    role: Role::User,
                    content: vec![Content::ToolResult {
                        id: format!("c{i}"),
                        output: format!("{i}\tpub fn f{i}() {{}}\n").repeat(12),
                        is_error: false,
                    }],
                },
            ],
        })
        .collect();
    Context {
        history,
        ..Default::default()
    }
}

/// Median time of the cut search over `n` step-turns, with a window that holds far fewer
/// of them, so the search has thousands of cuts to pass over.
fn cut_search(n: usize) -> Duration {
    let c = context(n);
    let caps = Capabilities {
        context_window: 100_000,
        max_output: 8_000,
        parallel_tools: true,
        reasoning: ReasoningStyle::None,
        cache: CacheStyle::Explicit,
        images: false,
        edit_format: EditFormat::StrReplace,
    };
    let model = ModelId::from("mock/test");
    let mut times = Vec::new();
    for _ in 0..5 {
        let t = Instant::now();
        let (_, removed) = c.compaction_request(&model, &caps, 4);
        times.push(t.elapsed());
        assert!(removed > 0 && removed < n - 4, "{removed}");
    }
    median(times)
}

#[test]
#[ignore = "perf gate"]
fn compaction_cut_search_grows_linearly() {
    let (small, large) = (cut_search(2_000), cut_search(5_000));
    let ratio = large.as_secs_f64() / small.as_secs_f64().max(1e-9);
    report(&format!(
        "context: compaction cut search {} at 2,000 step-turns, {} at 5,000 (x{ratio:.1}; linear 2.5, quadratic 6.25)",
        ms(small),
        ms(large)
    ));
    // An absolute bound, not the ratio of two small times: the quadratic search took 3.4 s
    // at 5,000 step-turns, the linear one 5 ms (both on an M-series Mac).
    assert!(large < Duration::from_millis(250), "{large:?}");
}
