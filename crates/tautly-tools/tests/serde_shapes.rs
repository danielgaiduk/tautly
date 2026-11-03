//! Serde shapes of what tools report (checkpoint 3, I5): the session log stores these, so
//! the golden JSON below is a compatibility contract, and `PersistedEffect` must carry
//! file contents by reference only.

use std::collections::HashMap;
use std::convert::Infallible;
use std::path::PathBuf;

use serde_json::json;
use tautly_tools::{AppliedEffect, LineRange, PersistedEffect, Shown, ToolMeta};

fn hash(s: &str) -> blake3::Hash {
    blake3::hash(s.as_bytes())
}

fn effects() -> Vec<AppliedEffect> {
    vec![
        AppliedEffect::Read {
            path: PathBuf::from("/p/src/lib.rs"),
            range: LineRange {
                start: Some(10),
                end: None,
            },
            file_hash: hash("file"),
            shown: Shown::Full,
        },
        AppliedEffect::Read {
            path: PathBuf::from("/p/src/lib.rs"),
            range: LineRange::ALL,
            file_hash: hash("file"),
            shown: Shown::Partial,
        },
        AppliedEffect::Read {
            path: PathBuf::from("/p/src/lib.rs"),
            range: LineRange {
                start: Some(1),
                end: Some(5),
            },
            file_hash: hash("file"),
            shown: Shown::Unchanged { since: 3 },
        },
        AppliedEffect::Read {
            path: PathBuf::from("/p/src/lib.rs"),
            range: LineRange::ALL,
            file_hash: hash("file"),
            shown: Shown::Diff { since: 4 },
        },
        AppliedEffect::Edited {
            path: PathBuf::from("/p/a.txt"),
            pre_image: Some(b"before\n".to_vec()),
            pre_mode: Some(0o644),
            post_hash: hash("after"),
        },
        AppliedEffect::Edited {
            path: PathBuf::from("/p/new.txt"),
            pre_image: None,
            pre_mode: None,
            post_hash: hash("created"),
        },
        AppliedEffect::Deleted {
            path: PathBuf::from("/p/gone.txt"),
            pre_image: vec![0, 159, 255, 10],
            pre_mode: 0o600,
        },
        AppliedEffect::Exec { may_write: true },
        AppliedEffect::Exec { may_write: false },
    ]
}

#[test]
fn applied_effects_roundtrip_through_json() {
    for e in effects() {
        let text = serde_json::to_string(&e).unwrap();
        let back: AppliedEffect = serde_json::from_str(&text).unwrap();
        assert_eq!(back, e, "{text}");
    }
}

#[test]
fn applied_effect_json_shape_is_stable() {
    let read = AppliedEffect::Read {
        path: PathBuf::from("/p/src/lib.rs"),
        range: LineRange {
            start: Some(1),
            end: None,
        },
        file_hash: hash("file"),
        shown: Shown::Unchanged { since: 3 },
    };
    assert_eq!(
        serde_json::to_value(&read).unwrap(),
        json!({
            "type": "read",
            "path": "/p/src/lib.rs",
            "range": {"start": 1, "end": null},
            "file_hash": hash("file").to_hex().to_string(),
            "shown": {"type": "unchanged", "since": 3},
        })
    );
    let edited = AppliedEffect::Edited {
        path: PathBuf::from("/p/a.txt"),
        pre_image: Some(b"before\n".to_vec()),
        pre_mode: Some(0o644),
        post_hash: hash("after"),
    };
    assert_eq!(
        serde_json::to_value(&edited).unwrap(),
        json!({
            "type": "edited",
            "path": "/p/a.txt",
            "pre_image": "YmVmb3JlCg==",
            "pre_mode": 420,
            "post_hash": hash("after").to_hex().to_string(),
        })
    );
    assert_eq!(
        serde_json::to_value(AppliedEffect::Exec { may_write: true }).unwrap(),
        json!({"type": "exec", "may_write": true})
    );
    assert_eq!(
        serde_json::to_value(Shown::Full).unwrap(),
        json!({"type": "full"})
    );
}

#[test]
fn hashes_serialize_as_lowercase_hex() {
    let v = serde_json::to_value(&effects()[0]).unwrap();
    let h = v["file_hash"].as_str().unwrap();
    assert_eq!(h.len(), 64);
    assert!(
        h.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
        "{h}"
    );
}

#[test]
fn a_malformed_hash_is_rejected() {
    for bad in ["", "abc", &"g".repeat(64), &"a".repeat(63)] {
        let v = json!({"type": "read", "path": "/p", "range": {"start": null, "end": null},
                       "file_hash": bad, "shown": {"type": "full"}});
        assert!(serde_json::from_value::<AppliedEffect>(v).is_err(), "{bad}");
    }
}

#[test]
fn tool_meta_roundtrips_and_has_a_stable_shape() {
    let mut meta = ToolMeta::default();
    meta.exit_code = Some(137);
    meta.error_kind = Some("not_found");
    let v = serde_json::to_value(&meta).unwrap();
    assert_eq!(v, json!({"exit_code": 137, "error_kind": "not_found"}));
    assert_eq!(serde_json::from_value::<ToolMeta>(v).unwrap(), meta);

    let empty = serde_json::to_value(ToolMeta::default()).unwrap();
    assert_eq!(empty, json!({"exit_code": null, "error_kind": null}));
    assert_eq!(
        serde_json::from_value::<ToolMeta>(empty).unwrap(),
        ToolMeta::default()
    );
    // Absent fields read as None (older logs, future trimming).
    assert_eq!(
        serde_json::from_value::<ToolMeta>(json!({})).unwrap(),
        ToolMeta::default()
    );
}

#[test]
fn every_error_kind_survives_a_round_trip_and_an_unknown_one_is_named_unknown() {
    for kind in [
        "bad_args",
        "io",
        "undecodable",
        "not_read",
        "stale",
        "not_found",
        "ambiguous",
        "mixed_artifacts",
        "syntax",
        "stale_anchor",
        "invalid_ops",
        "invalid_patch",
        "invalid_memory",
    ] {
        let v = json!({"exit_code": null, "error_kind": kind});
        let meta: ToolMeta = serde_json::from_value(v).unwrap();
        assert_eq!(meta.error_kind, Some(kind));
    }
    let meta: ToolMeta =
        serde_json::from_value(json!({"exit_code": null, "error_kind": "from_the_future"}))
            .unwrap();
    assert_eq!(meta.error_kind, Some("unknown"));
}

/// A blob store in a map, keyed by the blake3 hex of the content (like tautly-session's).
#[derive(Default)]
struct FakeBlobs(HashMap<String, Vec<u8>>);

impl FakeBlobs {
    fn put(&mut self, bytes: &[u8]) -> Result<String, Infallible> {
        let key = blake3::hash(bytes).to_hex().to_string();
        self.0.insert(key.clone(), bytes.to_vec());
        Ok(key)
    }

    fn get(&self, key: &str) -> Result<Vec<u8>, String> {
        self.0.get(key).cloned().ok_or(format!("no blob {key}"))
    }
}

#[test]
fn persisted_effects_roundtrip_through_a_blob_store() {
    let mut blobs = FakeBlobs::default();
    for e in effects() {
        let p = PersistedEffect::from_applied(&e, &mut |b| blobs.put(b)).unwrap();
        // Through JSON, as the session log does.
        let text = serde_json::to_string(&p).unwrap();
        let p: PersistedEffect = serde_json::from_str(&text).unwrap();
        let back = p.to_applied(&mut |k| blobs.get(k)).unwrap();
        assert_eq!(back, e, "{text}");
    }
}

#[test]
fn persisted_effects_hold_no_file_contents() {
    let mut blobs = FakeBlobs::default();
    let e = AppliedEffect::Deleted {
        path: PathBuf::from("/p/secret.txt"),
        pre_image: b"hunter2 hunter2 hunter2".to_vec(),
        pre_mode: 0o600,
    };
    let p = PersistedEffect::from_applied(&e, &mut |b| blobs.put(b)).unwrap();
    let blob = blake3::hash(b"hunter2 hunter2 hunter2")
        .to_hex()
        .to_string();
    assert_eq!(
        serde_json::to_value(&p).unwrap(),
        json!({
            "type": "deleted",
            "path": "/p/secret.txt",
            "pre_image_blob": blob,
            "pre_mode": 384,
        })
    );
    assert_eq!(blobs.0.len(), 1);

    // A created file has no pre-image, so nothing is stored.
    let created = AppliedEffect::Edited {
        path: PathBuf::from("/p/new.txt"),
        pre_image: None,
        pre_mode: None,
        post_hash: hash("x"),
    };
    let mut stored = 0;
    let p = PersistedEffect::from_applied(&created, &mut |b| {
        stored += 1;
        blobs.put(b)
    })
    .unwrap();
    assert_eq!(stored, 0);
    assert_eq!(
        serde_json::to_value(&p).unwrap(),
        json!({
            "type": "edited",
            "path": "/p/new.txt",
            "pre_image_blob": null,
            "pre_mode": null,
            "post_hash": hash("x").to_hex().to_string(),
        })
    );
}

#[test]
fn blob_errors_abort_the_conversion() {
    let e = AppliedEffect::Edited {
        path: PathBuf::from("/p/a.txt"),
        pre_image: Some(b"x".to_vec()),
        pre_mode: None,
        post_hash: hash("y"),
    };
    let err =
        PersistedEffect::from_applied(&e, &mut |_| Err::<String, _>("disk full")).unwrap_err();
    assert_eq!(err, "disk full");

    let mut blobs = FakeBlobs::default();
    let p = PersistedEffect::from_applied(&e, &mut |b| blobs.put(b)).unwrap();
    blobs.0.clear();
    let err = p.to_applied(&mut |k| blobs.get(k)).unwrap_err();
    assert!(err.starts_with("no blob "), "{err}");
}
