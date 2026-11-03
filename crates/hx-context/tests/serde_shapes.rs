//! Serde shapes of the context types the engine emits and persists (checkpoint 3, I5):
//! `ContextStatus`, `ContextCollected`/`Compacted` reasons, `GcApplied`, compaction records.

use std::path::PathBuf;

use hx_context::{Breakdown, CompactionRecord, ContextStatus, GcRecord, MutationReason, Turn};
use hx_provider::{Content, Message, Role};
use serde_json::json;

fn status() -> ContextStatus {
    ContextStatus {
        used: 1234,
        budget: 200_000,
        breakdown: Breakdown {
            system: 500,
            tools: 300,
            agents_md: 40,
            memory: 30,
            repo_map: 20,
            pinned: 10,
            state: 4,
            history: 330,
        },
        warnings: vec!["AGENTS.md was cut".into()],
        flat_tokens: 1000,
        cached_prefix: 900,
    }
}

#[test]
fn context_status_json_shape_is_stable() {
    assert_eq!(
        serde_json::to_value(status()).unwrap(),
        json!({
            "used": 1234,
            "budget": 200_000,
            "breakdown": {
                "system": 500, "tools": 300, "agents_md": 40, "memory": 30,
                "repo_map": 20, "pinned": 10, "state": 4, "history": 330,
            },
            "warnings": ["AGENTS.md was cut"],
            "flat_tokens": 1000,
            "cached_prefix": 900,
        })
    );
}

#[test]
fn context_status_roundtrips() {
    let s = status();
    let back: ContextStatus = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
    assert_eq!(back, s);
}

#[test]
fn a_status_from_an_older_log_without_newer_fields_still_reads() {
    let old = json!({"used": 5, "budget": 10, "breakdown": {"system": 5}, "warnings": []});
    let s: ContextStatus = serde_json::from_value(old).unwrap();
    assert_eq!(s.used, 5);
    assert_eq!(s.breakdown.system, 5);
    assert_eq!(s.cached_prefix, 0);
}

#[test]
fn mutation_reasons_use_snake_case() {
    for (reason, name) in [
        (MutationReason::ColdCache, "cold_cache"),
        (MutationReason::CostBenefit, "cost_benefit"),
        (MutationReason::Forced, "forced"),
        (MutationReason::Manual, "manual"),
    ] {
        assert_eq!(serde_json::to_value(reason).unwrap(), json!(name));
        assert_eq!(
            serde_json::from_value::<MutationReason>(json!(name)).unwrap(),
            reason
        );
    }
}

fn turn(text: &str) -> Turn {
    Turn {
        messages: vec![
            Message {
                role: Role::User,
                content: vec![Content::Text(text.into())],
            },
            Message {
                role: Role::Assistant,
                content: vec![Content::ToolUse {
                    id: "c1".into(),
                    name: "read".into(),
                    input: json!({"path": "a.rs"}),
                }],
            },
        ],
    }
}

#[test]
fn gc_record_roundtrips_and_has_a_stable_shape() {
    let rec = GcRecord {
        elided: vec![("c1".into(), "old output".into())],
        tokens_freed: 77,
        reason: MutationReason::CostBenefit,
        forgotten_paths: vec![PathBuf::from("/p/a.rs")],
    };
    let v = serde_json::to_value(&rec).unwrap();
    assert_eq!(
        v,
        json!({
            "elided": [["c1", "old output"]],
            "tokens_freed": 77,
            "reason": "cost_benefit",
            "forgotten_paths": ["/p/a.rs"],
        })
    );
    assert_eq!(serde_json::from_value::<GcRecord>(v).unwrap(), rec);
}

#[test]
fn compaction_record_roundtrips() {
    let rec = CompactionRecord {
        removed_turns: vec![turn("first"), turn("second")],
        summary: "did things".into(),
        first_removed_turn: 1,
        last_removed_turn: 2,
        forgotten_paths: vec![PathBuf::from("/p/a.rs"), PathBuf::from("/p/b.rs")],
    };
    let text = serde_json::to_string(&rec).unwrap();
    assert_eq!(
        serde_json::from_str::<CompactionRecord>(&text).unwrap(),
        rec
    );
}

#[test]
fn turn_roundtrips_with_image_bytes() {
    let t = Turn {
        messages: vec![Message {
            role: Role::User,
            content: vec![Content::Image {
                media_type: "image/png".into(),
                data: vec![0, 1, 2, 255],
            }],
        }],
    };
    let text = serde_json::to_string(&t).unwrap();
    assert_eq!(serde_json::from_str::<Turn>(&text).unwrap(), t);
}
