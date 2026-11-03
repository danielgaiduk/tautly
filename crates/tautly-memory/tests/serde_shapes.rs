//! The serde shapes of the types the engine persists and emits (`MemoryProposed`,
//! `SessionRecord::MemoryProposal`). Pinned, because session logs outlive the code.

use std::path::PathBuf;
use std::time::{Duration, UNIX_EPOCH};

use serde_json::json;
use tautly_memory::{MemAnchor, MemoryChange, MemoryEntry, MemoryKind};

fn entry(stale: bool) -> MemoryEntry {
    MemoryEntry {
        id: "build-cmd".into(),
        title: "How to build".into(),
        kind: MemoryKind::Command,
        anchors: vec![
            MemAnchor {
                path: PathBuf::from("Cargo.toml"),
                symbol: None,
                hash: "abc123".into(),
            },
            MemAnchor {
                path: PathBuf::from("src/lib.rs"),
                symbol: Some("run".into()),
                hash: "def456".into(),
            },
        ],
        created: UNIX_EPOCH + Duration::new(1_700_000_000, 5),
        verified: Some(UNIX_EPOCH + Duration::from_secs(1_700_000_100)),
        body: "cargo build --workspace".into(),
        stale,
    }
}

#[test]
fn anchor_roundtrips() {
    let a = entry(false).anchors[1].clone();
    let text = serde_json::to_string(&a).unwrap();
    assert_eq!(serde_json::from_str::<MemAnchor>(&text).unwrap(), a);
}

#[test]
fn entry_roundtrips_and_never_stores_stale() {
    let e = entry(true);
    let value = serde_json::to_value(&e).unwrap();
    assert!(
        value.get("stale").is_none(),
        "stale is derived, not stored: {value}"
    );
    let back: MemoryEntry = serde_json::from_value(value).unwrap();
    assert_eq!(back, MemoryEntry { stale: false, ..e });
}

#[test]
fn entry_json_shape_is_stable() {
    assert_eq!(
        serde_json::to_value(entry(false)).unwrap(),
        json!({
            "id": "build-cmd",
            "title": "How to build",
            "kind": "command",
            "anchors": [
                {"path": "Cargo.toml", "symbol": null, "hash": "abc123"},
                {"path": "src/lib.rs", "symbol": "run", "hash": "def456"},
            ],
            "created": {"secs_since_epoch": 1_700_000_000u64, "nanos_since_epoch": 5},
            "verified": {"secs_since_epoch": 1_700_000_100u64, "nanos_since_epoch": 0},
            "body": "cargo build --workspace",
        })
    );
}

#[test]
fn change_roundtrips() {
    for change in [
        MemoryChange {
            upsert: Some(entry(false)),
            delete: vec!["old-one".into()],
        },
        MemoryChange {
            upsert: None,
            delete: vec!["a".into(), "b".into()],
        },
    ] {
        let text = serde_json::to_string(&change).unwrap();
        assert_eq!(serde_json::from_str::<MemoryChange>(&text).unwrap(), change);
    }
}

#[test]
fn entry_deserializes_without_stale_even_when_a_stored_form_has_it() {
    // A stored `stale: true` (hand-edited or from a future version) must not leak in.
    let mut value = serde_json::to_value(entry(false)).unwrap();
    value["stale"] = json!(true);
    let back: MemoryEntry = serde_json::from_value(value).unwrap();
    assert!(!back.stale);
}
