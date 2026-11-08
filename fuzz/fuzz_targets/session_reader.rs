//! The session log reader on arbitrary bytes: opening and reading never panics, record
//! offsets count up from 0, unknown record types read as `Unknown` and unreadable ones as
//! `Unreadable` (never as an error mid-log for a known type), and every path a record
//! holds resolves only to a path under the root (`..`, absolute paths and NUL refused).
#![no_main]

use std::io::Write;
use std::path::{Component, Path};

use libfuzzer_sys::fuzz_target;
use tautly_core::SessionRecord;
use tautly_session::{RelPath, SessionReader};

const ROOT: &str = "/fuzz/root";

fn check_path(p: &RelPath) {
    let root = Path::new(ROOT);
    if let Ok(full) = p.resolve(root) {
        let s = p.as_str();
        assert!(!s.contains('\0'), "{s:?} resolved with a NUL");
        let rest = full.strip_prefix(root).expect("resolved outside the root");
        assert!(
            rest.components().all(|c| matches!(c, Component::Normal(_))),
            "{s:?} resolved to {full:?}"
        );
        assert!(
            !rest.as_os_str().is_empty(),
            "{s:?} resolved to the root itself"
        );
    }
}

fn paths(rec: &SessionRecord) {
    // Every record kind that carries a path read back from the log.
    if let Ok(v) = serde_json::to_value(rec) {
        walk_strings(&v);
    }
}

/// Every string of a record, tried as a `RelPath`: whatever field it sits in, a path
/// must not resolve outside the root.
fn walk_strings(v: &serde_json::Value) {
    match v {
        serde_json::Value::String(s) => check_path(&rel(s)),
        serde_json::Value::Array(a) => a.iter().for_each(walk_strings),
        serde_json::Value::Object(o) => o.values().for_each(walk_strings),
        _ => {}
    }
}

/// Any string, as a record holds it.
fn rel(s: &str) -> RelPath {
    serde_json::from_value(serde_json::Value::String(s.to_string())).expect("any string")
}

fuzz_target!(|data: &[u8]| {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.jsonl");
    std::fs::File::create(&path)
        .unwrap()
        .write_all(data)
        .unwrap();

    // Typed records.
    if let Ok(reader) = SessionReader::<SessionRecord>::open(&path) {
        let _ = reader.header();
        for (want, e) in (0..).zip(reader.entries()) {
            let Ok(e) = e else { break };
            assert_eq!(e.offset, want, "offsets must count up from 0");
            paths(&e.rec);
        }
        // The walks over the same log.
        let _ = tautly_core::timetravel::step_at_offset(&reader, u64::MAX);
        let _ = tautly_core::timetravel::state_at(&reader, 1);
        let _ = tautly_core::resume::rebuild(&reader, None);
    }

    // Untyped: any JSON payload is a record.
    if let Ok(reader) = SessionReader::<serde_json::Value>::open(&path) {
        let typed = SessionReader::<SessionRecord>::open(&path).unwrap();
        let n_any = reader.records().take_while(Result::is_ok).count();
        let n_typed = typed.records().take_while(Result::is_ok).count();
        // A known record type whose payload does not read is `Unreadable`, an unknown
        // one `Unknown`: neither ends the log early.
        assert_eq!(n_any, n_typed, "typed reading stopped early");
    }
});
