//! Serde shapes of what tools report (checkpoint 3, I5): the session log stores these, so
//! the golden JSON below is a compatibility contract, and `PersistedEffect` must carry
//! file contents by reference only.

use std::collections::HashMap;
use std::convert::Infallible;
use std::path::{Path, PathBuf};

use serde_json::json;
use tautly_session::ConfineError;
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
fn a_shown_kind_from_a_newer_version_reads_as_unknown() {
    for v in [
        json!({"type": "folded", "lines": 3}),
        json!({"type": "folded"}),
    ] {
        assert_eq!(
            serde_json::from_value::<Shown>(v.clone()).unwrap(),
            Shown::Unknown,
            "{v}"
        );
    }
    for s in [
        Shown::Full,
        Shown::Partial,
        Shown::Unchanged { since: 2 },
        Shown::Diff { since: 3 },
        Shown::Unknown,
    ] {
        let v = serde_json::to_value(s).unwrap();
        assert_eq!(serde_json::from_value::<Shown>(v).unwrap(), s);
    }
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

/// Why reading an effect back failed.
#[derive(Debug, PartialEq)]
enum GetError {
    Missing(String),
    Confine(ConfineError),
}

impl From<ConfineError> for GetError {
    fn from(e: ConfineError) -> Self {
        GetError::Confine(e)
    }
}

impl FakeBlobs {
    fn put(&mut self, bytes: &[u8]) -> Result<String, Infallible> {
        let key = blake3::hash(bytes).to_hex().to_string();
        self.0.insert(key.clone(), bytes.to_vec());
        Ok(key)
    }

    fn get(&self, key: &str) -> Result<Vec<u8>, GetError> {
        self.0
            .get(key)
            .cloned()
            .ok_or(GetError::Missing(key.to_string()))
    }
}

/// The root the effects of [`effects`] are under.
const ROOT: &str = "/p";

fn persist(e: &AppliedEffect, blobs: &mut FakeBlobs) -> PersistedEffect {
    PersistedEffect::from_applied(e, Path::new(ROOT), &mut |b| blobs.put(b))
        .unwrap()
        .expect("below the root")
}

#[test]
fn persisted_effects_roundtrip_through_a_blob_store() {
    let mut blobs = FakeBlobs::default();
    for e in effects() {
        let p = persist(&e, &mut blobs);
        // Through JSON, as the session log does.
        let text = serde_json::to_string(&p).unwrap();
        let p: PersistedEffect = serde_json::from_str(&text).unwrap();
        let back = p
            .to_applied(Path::new(ROOT), &mut |k| blobs.get(k))
            .unwrap();
        assert_eq!(back, e, "{text}");
    }
}

#[test]
fn persisted_paths_are_relative_to_the_root_and_resolve_under_another() {
    let mut blobs = FakeBlobs::default();
    let e = &effects()[0];
    let p = persist(e, &mut blobs);
    assert_eq!(serde_json::to_value(&p).unwrap()["path"], "src/lib.rs");
    let AppliedEffect::Read { path, .. } = p
        .to_applied(Path::new("/elsewhere"), &mut |k| blobs.get(k))
        .unwrap()
    else {
        panic!("not a read")
    };
    assert_eq!(path, PathBuf::from("/elsewhere/src/lib.rs"));

    // A path outside the root, or not valid UTF-8, is not stored, and nothing is put.
    use std::os::unix::ffi::OsStrExt;
    let non_utf8 = PathBuf::from(std::ffi::OsStr::from_bytes(b"/p/caf\xe9.txt"));
    for path in [PathBuf::from("/q/outside.txt"), non_utf8] {
        let e = AppliedEffect::Deleted {
            path,
            pre_image: b"x".to_vec(),
            pre_mode: 0o644,
        };
        let mut stored = 0;
        let p = PersistedEffect::from_applied(&e, Path::new(ROOT), &mut |b| {
            stored += 1;
            blobs.put(b)
        })
        .unwrap();
        assert_eq!((p, stored), (None, 0));
    }
    let exec = AppliedEffect::Exec { may_write: true };
    assert_eq!(
        PersistedEffect::from_applied(&exec, Path::new(ROOT), &mut |b| blobs.put(b)).unwrap(),
        Some(PersistedEffect::Exec { may_write: true })
    );
}

#[test]
fn an_effect_type_from_a_newer_version_reads_as_unknown_and_counts_as_a_write() {
    let blobs = FakeBlobs::default();
    let p: PersistedEffect =
        serde_json::from_value(json!({"type": "renamed", "from": "a", "to": "b"})).unwrap();
    assert_eq!(p, PersistedEffect::Unknown);
    assert_eq!(p.path(), None);
    assert_eq!(
        p.to_applied(Path::new(ROOT), &mut |k| blobs.get(k))
            .unwrap(),
        AppliedEffect::Exec { may_write: true }
    );
}

#[test]
fn a_persisted_path_that_leaves_the_root_is_refused() {
    let blobs = FakeBlobs::default();
    for (bad, want) in [
        (
            "../etc/passwd",
            ConfineError::ParentDir("../etc/passwd".into()),
        ),
        ("/etc/passwd", ConfineError::Absolute("/etc/passwd".into())),
        ("", ConfineError::Empty),
    ] {
        let v = json!({"type": "read", "path": bad, "range": {"start": null, "end": null},
                       "file_hash": hash("f").to_hex().to_string(), "shown": {"type": "full"}});
        let p: PersistedEffect = serde_json::from_value(v).unwrap();
        let err = p
            .to_applied(Path::new(ROOT), &mut |k| blobs.get(k))
            .unwrap_err();
        assert_eq!(err, GetError::Confine(want), "{bad}");
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
    let p = persist(&e, &mut blobs);
    let blob = blake3::hash(b"hunter2 hunter2 hunter2")
        .to_hex()
        .to_string();
    assert_eq!(
        serde_json::to_value(&p).unwrap(),
        json!({
            "type": "deleted",
            "path": "secret.txt",
            "pre_image_blob": blob,
            "pre_mode": 384,
        })
    );
    assert_eq!(blobs.0.len(), 1);

    // A created file has no pre-image, so nothing is stored; the post-image is the
    // engine's to fill in.
    let created = AppliedEffect::Edited {
        path: PathBuf::from("/p/new.txt"),
        pre_image: None,
        pre_mode: None,
        post_hash: hash("x"),
    };
    let mut stored = 0;
    let p = PersistedEffect::from_applied(&created, Path::new(ROOT), &mut |b| {
        stored += 1;
        blobs.put(b)
    })
    .unwrap()
    .unwrap();
    assert_eq!(stored, 0);
    assert_eq!(
        serde_json::to_value(&p).unwrap(),
        json!({
            "type": "edited",
            "path": "new.txt",
            "pre_image_blob": null,
            "pre_mode": null,
            "post_hash": hash("x").to_hex().to_string(),
            "post_image_blob": null,
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
        PersistedEffect::from_applied(&e, Path::new(ROOT), &mut |_| Err::<String, _>("disk full"))
            .unwrap_err();
    assert_eq!(err, "disk full");

    let mut blobs = FakeBlobs::default();
    let p = persist(&e, &mut blobs);
    blobs.0.clear();
    let err = p
        .to_applied(Path::new(ROOT), &mut |k| blobs.get(k))
        .unwrap_err();
    assert!(matches!(err, GetError::Missing(_)), "{err:?}");
}

#[test]
fn tool_meta_counts_roundtrip_and_are_omitted_when_absent() {
    use tautly_tools::TestCounts;
    let mut meta = ToolMeta::default();
    meta.exit_code = Some(1);
    meta.counts = Some(TestCounts {
        passed: 7,
        failed: 2,
    });
    let v = serde_json::to_value(&meta).unwrap();
    assert_eq!(
        v,
        json!({"exit_code": 1, "error_kind": null, "counts": {"passed": 7, "failed": 2}})
    );
    assert_eq!(serde_json::from_value::<ToolMeta>(v).unwrap(), meta);
}
