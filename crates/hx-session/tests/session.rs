use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use hx_session::{BlobRef, Error, SCHEMA_VERSION, SessionReader, SessionWriter, list_in, paths};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Rec {
    Msg { text: String },
    Output { blob: BlobRef },
}

fn msg(t: &str) -> Rec {
    Rec::Msg { text: t.into() }
}

struct Env {
    data: tempfile::TempDir,
    project: tempfile::TempDir,
}

impl Env {
    fn new() -> Self {
        Self {
            data: tempfile::tempdir().unwrap(),
            project: tempfile::tempdir().unwrap(),
        }
    }
    fn writer(&self) -> SessionWriter<Rec> {
        SessionWriter::create_in(self.data.path(), self.project.path(), None).unwrap()
    }
    fn project_dir(&self) -> PathBuf {
        self.data
            .path()
            .join("sessions")
            .join(paths::project_hash(self.project.path()))
    }
}

fn read_all(path: &Path) -> (Vec<(u64, Rec)>, bool) {
    let r = SessionReader::<Rec>::open(path).unwrap();
    let recs = r.records().collect::<Result<Vec<_>, _>>().unwrap();
    (recs, r.truncated_tail())
}

#[test]
fn append_and_read_back_in_order() {
    let env = Env::new();
    let mut w = env.writer();
    let recs = [msg("one"), msg("two"), msg("three")];
    for (i, r) in recs.iter().enumerate() {
        assert_eq!(w.append(r).unwrap(), i as u64);
    }
    w.checkpoint().unwrap();

    let (got, truncated) = read_all(w.path());
    assert!(!truncated);
    assert_eq!(
        got,
        vec![(0, msg("one")), (1, msg("two")), (2, msg("three"))]
    );
}

#[test]
fn header_has_schema_version_1() {
    let env = Env::new();
    let w = env.writer();
    let path = w.path().to_path_buf();
    let id = w.session_id().to_string();
    drop(w);

    let r = SessionReader::<Rec>::open(&path).unwrap();
    assert_eq!(r.header().schema_version, 1);
    assert_eq!(SCHEMA_VERSION, 1);
    assert_eq!(r.header().session_id, id);
    assert_eq!(r.header().parent, None);
    assert_eq!(r.header().root, env.project.path().canonicalize().unwrap());
    assert_eq!(id.len(), 26, "ULID");
    assert_eq!(
        path.file_name().unwrap().to_str().unwrap(),
        format!("{id}.jsonl")
    );
    assert_eq!(path.parent().unwrap(), env.project_dir());

    let first = fs::read_to_string(&path).unwrap();
    let first = first.lines().next().unwrap();
    let v: serde_json::Value = serde_json::from_str(first).unwrap();
    assert_eq!(v["type"], "header");
    assert_eq!(v["schema_version"], 1);
}

#[test]
fn record_lines_wrap_the_record_with_type_and_offset() {
    let env = Env::new();
    let mut w = env.writer();
    w.append(&msg("hi")).unwrap();
    w.checkpoint().unwrap();
    let text = fs::read_to_string(w.path()).unwrap();
    let line: serde_json::Value = serde_json::from_str(text.lines().nth(1).unwrap()).unwrap();
    assert_eq!(line["type"], "record");
    assert_eq!(line["offset"], 0);
    assert_eq!(line["data"]["text"], "hi");
}

#[test]
fn truncated_last_line_is_skipped_and_flagged() {
    let env = Env::new();
    let mut w = env.writer();
    w.append(&msg("kept")).unwrap();
    w.checkpoint().unwrap();
    let path = w.path().to_path_buf();
    // Half of a record line, no newline: a crash mid-write.
    let mut f = OpenOptions::new().append(true).open(&path).unwrap();
    f.write_all(br#"{"type":"record","offset":1,"data":{"kind":"msg","te"#)
        .unwrap();
    drop(f);

    let (got, truncated) = read_all(&path);
    assert!(truncated);
    assert_eq!(got, vec![(0, msg("kept"))]);
}

#[test]
fn unparseable_final_line_with_newline_is_also_a_truncated_tail() {
    let env = Env::new();
    let mut w = env.writer();
    w.append(&msg("kept")).unwrap();
    w.checkpoint().unwrap();
    let path = w.path().to_path_buf();
    let mut f = OpenOptions::new().append(true).open(&path).unwrap();
    f.write_all(b"{\"type\":\"record\",\"off\n").unwrap();
    drop(f);

    let (got, truncated) = read_all(&path);
    assert!(truncated);
    assert_eq!(got, vec![(0, msg("kept"))]);
}

#[test]
fn a_complete_log_is_not_flagged() {
    let env = Env::new();
    let w = env.writer();
    let path = w.path().to_path_buf();
    drop(w);
    let (got, truncated) = read_all(&path);
    assert!(got.is_empty());
    assert!(!truncated);
}

#[test]
fn malformed_line_in_the_middle_is_an_error_not_skipped() {
    let env = Env::new();
    let mut w = env.writer();
    w.append(&msg("a")).unwrap();
    w.checkpoint().unwrap();
    let path = w.path().to_path_buf();
    let mut f = OpenOptions::new().append(true).open(&path).unwrap();
    f.write_all(b"this is not json\n").unwrap();
    f.write_all(br#"{"type":"record","offset":2,"data":{"kind":"msg","text":"c"}}"#)
        .unwrap();
    f.write_all(b"\n").unwrap();
    drop(f);

    let r = SessionReader::<Rec>::open(&path).unwrap();
    assert!(!r.truncated_tail());
    let items: Vec<_> = r.records().collect();
    assert_eq!(
        items.len(),
        2,
        "first record, then the error ends iteration"
    );
    assert_eq!(items[0].as_ref().unwrap(), &(0, msg("a")));
    match &items[1] {
        Err(Error::Corrupt { line, .. }) => assert_eq!(*line, 3),
        other => panic!("expected Corrupt, got {other:?}"),
    }
}

#[test]
fn unknown_line_types_are_skipped() {
    let env = Env::new();
    let mut w = env.writer();
    w.append(&msg("a")).unwrap();
    w.checkpoint().unwrap();
    let path = w.path().to_path_buf();
    let mut f = OpenOptions::new().append(true).open(&path).unwrap();
    f.write_all(b"{\"type\":\"evidence\",\"x\":1}\n").unwrap();
    f.write_all(br#"{"type":"record","offset":1,"data":{"kind":"msg","text":"b"}}"#)
        .unwrap();
    f.write_all(b"\n").unwrap();
    drop(f);

    let (got, truncated) = read_all(&path);
    assert!(!truncated);
    assert_eq!(got, vec![(0, msg("a")), (1, msg("b"))]);
}

#[test]
fn dropping_the_writer_flushes_buffered_records() {
    let env = Env::new();
    let mut w = env.writer();
    w.append(&msg("buffered")).unwrap();
    let path = w.path().to_path_buf();
    drop(w);
    let (got, _) = read_all(&path);
    assert_eq!(got, vec![(0, msg("buffered"))]);
}

#[test]
fn blobs_deduplicate() {
    let env = Env::new();
    let mut w = env.writer();
    let a = w.blob(b"same bytes").unwrap();
    let b = w.blob(b"same bytes").unwrap();
    let c = w.blob(b"other bytes").unwrap();
    assert_eq!(a, b);
    assert_ne!(a, c);
    assert_eq!(a.as_str(), blake3::hash(b"same bytes").to_hex().as_str());

    let (head, rest) = a.as_str().split_at(2);
    let blob_path = env.project_dir().join("blobs").join(head).join(rest);
    assert_eq!(fs::read(&blob_path).unwrap(), b"same bytes");
    let files: Vec<_> = walk_files(&env.project_dir().join("blobs"));
    assert_eq!(
        files.len(),
        2,
        "one file per distinct content, no temp files left: {files:?}"
    );

    w.append(&Rec::Output { blob: a.clone() }).unwrap();
    w.checkpoint().unwrap();
    let r = SessionReader::<Rec>::open(w.path()).unwrap();
    let (_, rec) = r.records().next().unwrap().unwrap();
    let Rec::Output { blob } = rec else { panic!() };
    assert_eq!(r.read_blob(&blob).unwrap(), b"same bytes");
}

fn walk_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for e in fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            out.extend(walk_files(&p));
        } else {
            out.push(p);
        }
    }
    out
}

#[test]
fn read_blob_rejects_bad_refs_and_reports_missing() {
    let env = Env::new();
    let w = env.writer();
    let path = w.path().to_path_buf();
    drop(w);
    let r = SessionReader::<Rec>::open(&path).unwrap();

    let traversal: BlobRef = serde_json::from_str("\"../../../../etc/passwd\"").unwrap();
    assert!(matches!(
        r.read_blob(&traversal),
        Err(Error::InvalidBlobRef(_))
    ));
    assert!(BlobRef::parse("abc").is_err());

    let missing = BlobRef::parse(&"0".repeat(64)).unwrap();
    assert!(matches!(r.read_blob(&missing), Err(Error::BlobNotFound(_))));
}

#[test]
fn fork_header_references_parent_offset() {
    let env = Env::new();
    let mut parent = env.writer();
    for t in ["a", "b", "c"] {
        parent.append(&msg(t)).unwrap();
    }
    parent.checkpoint().unwrap();

    let fork = SessionWriter::<Rec>::create_in(
        env.data.path(),
        env.project.path(),
        Some((parent.session_id().to_string(), 1)),
    )
    .unwrap();
    let r = SessionReader::<Rec>::open(fork.path()).unwrap();
    assert_eq!(
        r.header().parent,
        Some((parent.session_id().to_string(), 1))
    );
    assert_ne!(fork.session_id(), parent.session_id());
}

#[test]
fn list_sorted_newest_first() {
    let env = Env::new();
    let mut ids = Vec::new();
    for _ in 0..3 {
        ids.push(env.writer().session_id().to_string());
        std::thread::sleep(Duration::from_millis(3));
    }
    let listed = list_in(env.data.path(), env.project.path()).unwrap();
    let got: Vec<_> = listed.iter().map(|s| s.id.clone()).collect();
    ids.reverse();
    assert_eq!(got, ids);
    assert!(listed[0].created >= listed[1].created);
    assert!(listed[0].path.ends_with(format!("{}.jsonl", ids[0])));
    assert!(listed[0].last_modified >= listed[0].created - Duration::from_secs(1));
}

#[test]
fn list_tolerates_partial_and_unrelated_files_and_empty_projects() {
    let env = Env::new();
    assert!(
        list_in(env.data.path(), env.project.path())
            .unwrap()
            .is_empty()
    );

    let good = env.writer().session_id().to_string();
    let dir = env.project_dir();
    fs::write(dir.join("EMPTY.jsonl"), b"").unwrap();
    fs::write(
        dir.join("PARTIAL.jsonl"),
        br#"{"type":"header","schema_ver"#,
    )
    .unwrap();
    fs::write(dir.join("notes.txt"), b"hello").unwrap();

    let listed = list_in(env.data.path(), env.project.path()).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, good);
}

#[test]
fn two_writers_same_project_do_not_collide() {
    let env = Env::new();
    let data = env.data.path().to_path_buf();
    let project = env.project.path().to_path_buf();
    let handles: Vec<_> = (0..2)
        .map(|n| {
            let (data, project) = (data.clone(), project.clone());
            std::thread::spawn(move || {
                let mut w = SessionWriter::<Rec>::create_in(&data, &project, None).unwrap();
                for i in 0..50 {
                    w.append(&msg(&format!("w{n}-{i}"))).unwrap();
                }
                w.checkpoint().unwrap();
                (w.path().to_path_buf(), n)
            })
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_ne!(results[0].0, results[1].0);
    for (path, n) in results {
        let (got, truncated) = read_all(&path);
        assert!(!truncated);
        assert_eq!(got.len(), 50);
        assert_eq!(got[49].1, msg(&format!("w{n}-49")));
    }
    assert_eq!(list_in(&data, &project).unwrap().len(), 2);
}

static ENV_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn data_home_precedence_and_default_create_uses_it() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let saved: Vec<_> = ["HX_HOME", "XDG_DATA_HOME", "HOME"]
        .iter()
        .map(|k| (*k, std::env::var_os(k)))
        .collect();
    let set = |k: &str, v: Option<&Path>| unsafe {
        match v {
            Some(v) => std::env::set_var(k, v),
            None => std::env::remove_var(k),
        }
    };

    let hx = tempfile::tempdir().unwrap();
    let xdg = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    set("HOME", Some(home.path()));
    set("XDG_DATA_HOME", None);
    set("HX_HOME", None);
    assert_eq!(paths::data_home(), home.path().join(".local/share/hx"));
    set("XDG_DATA_HOME", Some(xdg.path()));
    assert_eq!(paths::data_home(), xdg.path().join("hx"));
    set("HX_HOME", Some(hx.path()));
    assert_eq!(paths::data_home(), hx.path().join("hx"));

    // The default constructors follow HX_HOME.
    let project = tempfile::tempdir().unwrap();
    let w = SessionWriter::<Rec>::create(project.path(), None).unwrap();
    assert!(w.path().starts_with(hx.path().join("hx").join("sessions")));
    assert_eq!(hx_session::list(project.path()).unwrap().len(), 1);
    drop(w);

    for (k, v) in saved {
        set(k, v.as_deref().map(Path::new));
    }
}

#[test]
fn project_hash_is_16_hex_stable_and_path_specific() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let h = paths::project_hash(a.path());
    assert_eq!(h.len(), 16);
    assert!(h.bytes().all(|c| c.is_ascii_hexdigit()));
    assert_eq!(h, paths::project_hash(a.path()));
    assert_ne!(h, paths::project_hash(b.path()));
    // Canonicalized: a non-normalized spelling hashes the same.
    assert_eq!(h, paths::project_hash(&a.path().join(".")));
}
