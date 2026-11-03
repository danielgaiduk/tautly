//! Time-travel hooks in the session format (spec 6.5 H1-H3, H10): record timestamps,
//! header fields, root-relative paths and the project id shared by git worktrees.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tautly_session::{
    ConfineError, GitState, HeaderInit, Origin, RelPath, SessionReader, SessionWriter, git_head,
    list_in, paths,
};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Msg {
    text: String,
}

fn msg(t: &str) -> Msg {
    Msg { text: t.into() }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn lines(path: &Path) -> Vec<serde_json::Value> {
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

const SHA_A: &str = "1111111111111111111111111111111111111111";
const SHA_B: &str = "2222222222222222222222222222222222222222";
const SHA_C: &str = "3333333333333333333333333333333333333333";
const SHA_D: &str = "4444444444444444444444444444444444444444";
const SHA_E: &str = "5555555555555555555555555555555555555555";

/// A repository made by hand: `.git/HEAD` naming branch `main` at `sha`.
fn fake_repo(dir: &Path, sha: &str) {
    write(&dir.join(".git/HEAD"), "ref: refs/heads/main\n");
    write(&dir.join(".git/refs/heads/main"), &format!("{sha}\n"));
}

#[test]
fn every_record_line_has_unix_ms_timestamp() {
    let (data, project) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let before = now_ms();
    let mut w = SessionWriter::<Msg>::create_in(data.path(), project.path(), HeaderInit::default())
        .unwrap();
    for t in ["a", "b", "c"] {
        w.append(&msg(t)).unwrap();
    }
    w.checkpoint().unwrap();
    let after = now_ms();

    let all = lines(w.path());
    assert_eq!(all.len(), 4);
    let raw = fs::read_to_string(w.path()).unwrap();
    let mut last = 0;
    for (i, (line, text)) in all[1..].iter().zip(raw.lines().skip(1)).enumerate() {
        let envelope = format!(
            r#"{{"type":"record","offset":{i},"t":{},"data":"#,
            line["t"]
        );
        assert!(text.starts_with(&envelope), "{text}");
        assert_eq!(line["offset"], i as u64);
        let t = line["t"].as_u64().expect("t is a number");
        assert!(before <= t && t <= after, "{before} <= {t} <= {after}");
        assert!(t >= last);
        last = t;
    }

    let reader = SessionReader::<Msg>::open(w.path()).unwrap();
    let entries: Vec<_> = reader.entries().map(Result::unwrap).collect();
    assert_eq!(entries.len(), 3);
    for (e, line) in entries.iter().zip(&all[1..]) {
        assert_eq!(e.t_ms, line["t"].as_u64().unwrap());
    }
    assert_eq!(entries[2].offset, 2);
    assert_eq!(entries[2].rec, msg("c"));
    // `records()` still gives (offset, record).
    let recs: Vec<_> = reader.records().map(Result::unwrap).collect();
    assert_eq!(recs[1], (1, msg("b")));
}

#[test]
fn line_without_t_reads_as_zero() {
    let (data, project) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut w = SessionWriter::<Msg>::create_in(data.path(), project.path(), HeaderInit::default())
        .unwrap();
    w.append(&msg("new")).unwrap();
    w.checkpoint().unwrap();
    let path = w.path().to_path_buf();
    drop(w);
    OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"{\"type\":\"record\",\"offset\":1,\"data\":{\"text\":\"old\"}}\n")
        .unwrap();

    let reader = SessionReader::<Msg>::open(&path).unwrap();
    let entries: Vec<_> = reader.entries().map(Result::unwrap).collect();
    assert_eq!(entries.len(), 2);
    assert!(entries[0].t_ms > 0);
    assert_eq!((entries[1].offset, entries[1].t_ms), (1, 0));
    assert_eq!(entries[1].rec, msg("old"));
}

#[test]
fn header_carries_project_label_origin_git_version_model_config_hash_tags() {
    let data = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    fake_repo(project.path(), SHA_A);
    let init = HeaderInit {
        parent: Some(("01PARENT".into(), 7)),
        label: Some("try-a".into()),
        origin: Origin::WhatIf,
        model: Some("anthropic/claude-x".into()),
        config_hash: Some("c0ffee".into()),
        tags: vec!["eval".into(), "nightly".into()],
    };
    let w = SessionWriter::<Msg>::create_in(data.path(), project.path(), init).unwrap();
    let path = w.path().to_path_buf();
    drop(w);

    let reader = SessionReader::<Msg>::open(&path).unwrap();
    let h = reader.header();
    assert_eq!(h.project, paths::project_id(project.path()));
    assert_eq!(h.project.len(), 16);
    assert_eq!(h.parent, Some(("01PARENT".into(), 7)));
    assert_eq!(h.label.as_deref(), Some("try-a"));
    assert_eq!(h.origin, Origin::WhatIf);
    assert_eq!(
        h.git,
        Some(GitState {
            head: Some(SHA_A.into()),
            dirty: None,
        })
    );
    assert_eq!(h.tautly_version, env!("CARGO_PKG_VERSION"));
    assert!(!h.tautly_version.is_empty());
    assert_eq!(h.model.as_deref(), Some("anthropic/claude-x"));
    assert_eq!(h.config_hash.as_deref(), Some("c0ffee"));
    assert_eq!(h.tags, ["eval", "nightly"]);
    assert_eq!(h.root, project.path().canonicalize().unwrap());
    let raw = &lines(&path)[0];
    assert_eq!(raw["origin"], "what_if");

    // Outside git there is no git state.
    let plain = tempfile::tempdir().unwrap();
    let w =
        SessionWriter::<Msg>::create_in(data.path(), plain.path(), HeaderInit::default()).unwrap();
    assert_eq!(w.header().git, None);
    assert_eq!(w.header().origin, Origin::User);
}

#[test]
fn old_header_without_new_fields_reads_with_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("old.jsonl");
    write(
        &path,
        concat!(
            r#"{"type":"header","schema_version":1,"session_id":"01OLD","#,
            r#""created":{"secs_since_epoch":1700000000,"nanos_since_epoch":0},"#,
            r#""root":"/r","parent":null}"#,
            "\n",
            r#"{"type":"record","offset":0,"data":{"text":"x"}}"#,
            "\n"
        ),
    );
    let reader = SessionReader::<Msg>::open(&path).unwrap();
    let h = reader.header();
    assert_eq!(h.session_id, "01OLD");
    assert_eq!(h.project, "");
    assert_eq!(h.label, None);
    assert_eq!(h.origin, Origin::User);
    assert_eq!(h.git, None);
    assert_eq!(h.tautly_version, "");
    assert_eq!(h.model, None);
    assert_eq!(h.config_hash, None);
    assert!(h.tags.is_empty());
    assert_eq!(reader.records().count(), 1);

    // An origin from a newer version reads as Unknown.
    let newer = dir.path().join("newer.jsonl");
    write(
        &newer,
        concat!(
            r#"{"type":"header","schema_version":1,"session_id":"01NEW","#,
            r#""created":{"secs_since_epoch":1700000000,"nanos_since_epoch":0},"#,
            r#""root":"/r","parent":null,"origin":"best_of_n"}"#,
            "\n"
        ),
    );
    let reader = SessionReader::<Msg>::open(&newer).unwrap();
    assert_eq!(reader.header().origin, Origin::Unknown);
}

fn rel(s: &str) -> RelPath {
    serde_json::from_value(serde_json::Value::String(s.into())).unwrap()
}

#[test]
fn relpath_rejects_absolute_dotdot_nul_and_empty() {
    let root = Path::new("/work/repo");
    assert!(matches!(
        rel("/etc/passwd").resolve(root),
        Err(ConfineError::Absolute(_))
    ));
    for p in ["..", "../x", "a/../../x", "a/.."] {
        assert!(
            matches!(rel(p).resolve(root), Err(ConfineError::ParentDir(_))),
            "{p}"
        );
    }
    assert_eq!(rel("a\0b").resolve(root), Err(ConfineError::Nul));
    assert_eq!(rel("").resolve(root), Err(ConfineError::Empty));
    assert!(matches!(
        rel("./a").resolve(root),
        Err(ConfineError::CurDir(_))
    ));
    assert_eq!(
        rel("src/lib.rs").resolve(root).unwrap(),
        PathBuf::from("/work/repo/src/lib.rs")
    );

    // From absolute paths: only paths strictly below the root.
    assert_eq!(
        RelPath::from_abs(root, Path::new("/work/repo/src/lib.rs"))
            .unwrap()
            .as_str(),
        "src/lib.rs"
    );
    for p in [
        "/work/other/a",
        "/work/repo",
        "/work/repo/../x",
        "relative/a",
    ] {
        assert_eq!(RelPath::from_abs(root, Path::new(p)), None, "{p}");
    }
    use std::os::unix::ffi::OsStrExt;
    let bad = Path::new(std::ffi::OsStr::from_bytes(b"/work/repo/caf\xe9.txt"));
    assert_eq!(RelPath::from_abs(root, bad), None);
}

#[test]
fn relpath_written_under_one_root_resolves_under_another() {
    let a = Path::new("/home/me/checkout");
    let b = Path::new("/tmp/worktrees/fork-1");
    let p = RelPath::from_abs(a, &a.join("src/deep/mod.rs")).unwrap();
    let text = serde_json::to_string(&p).unwrap();
    assert_eq!(text, r#""src/deep/mod.rs""#);
    let back: RelPath = serde_json::from_str(&text).unwrap();
    assert_eq!(back.resolve(b).unwrap(), b.join("src/deep/mod.rs"));
}

/// Run git with no user or system configuration and no hooks.
fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.invalid",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

#[test]
fn worktree_sessions_share_project_id_and_blob_dir() {
    let data = tempfile::tempdir().unwrap();
    let top = tempfile::tempdir().unwrap();
    let main = top.path().join("main");
    let wt = top.path().join("wt");
    fs::create_dir(&main).unwrap();
    git(&main, &["init", "-q"]);
    write(&main.join("sub/a.txt"), "a\n");
    git(&main, &["add", "-A"]);
    git(&main, &["commit", "-q", "-m", "one"]);
    git(
        &main,
        &["worktree", "add", "-q", "-b", "side", wt.to_str().unwrap()],
    );
    write(&wt.join("b.txt"), "b\n");
    git(&wt, &["add", "-A"]);
    git(&wt, &["commit", "-q", "-m", "two"]);

    let id = paths::project_id(&main);
    assert_eq!(paths::project_id(&wt), id);
    assert_eq!(paths::project_id(&main.join("sub")), id);
    // The code index stays per worktree.
    assert_ne!(paths::project_hash(&main), paths::project_hash(&wt));
    assert_eq!(git_head(&main), Some(git(&main, &["rev-parse", "HEAD"])));
    assert_eq!(git_head(&wt), Some(git(&wt, &["rev-parse", "HEAD"])));
    assert_ne!(git_head(&main), git_head(&wt));

    let mut a = SessionWriter::<Msg>::create_in(data.path(), &main, HeaderInit::default()).unwrap();
    let b = SessionWriter::<Msg>::create_in(data.path(), &wt, HeaderInit::default()).unwrap();
    assert_eq!(a.path().parent(), b.path().parent());
    assert_eq!(
        a.path().parent().unwrap(),
        data.path().join("sessions").join(&id)
    );
    assert_eq!(b.header().root, wt.canonicalize().unwrap());
    assert_eq!(a.header().project, id);
    assert_eq!(b.header().project, id);
    let blob = a.blob(b"shared bytes").unwrap();
    a.append(&msg("x")).unwrap();
    a.checkpoint().unwrap();
    let reader = SessionReader::<Msg>::open(b.path()).unwrap();
    assert_eq!(reader.read_blob(&blob).unwrap(), b"shared bytes");
    assert_eq!(list_in(data.path(), &wt).unwrap().len(), 2);
}

#[test]
fn non_git_project_id_is_root_hash() {
    let data = tempfile::tempdir().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let id = paths::project_id(dir.path());
    assert_eq!(id, paths::project_hash(dir.path()));
    assert_eq!(id.len(), 16);
    assert!(id.bytes().all(|b| b.is_ascii_hexdigit()));
    let w =
        SessionWriter::<Msg>::create_in(data.path(), dir.path(), HeaderInit::default()).unwrap();
    assert_eq!(
        w.path().parent().unwrap(),
        data.path().join("sessions").join(&id)
    );
    assert_eq!(git_head(dir.path()), None);
}

const NO_GIT_TEST: &str = "git_head_read_from_files_for_branch_detached_packed_refs_and_worktree";

#[test]
fn git_head_read_from_files_for_branch_detached_packed_refs_and_worktree() {
    // Runs again in a child process with an empty PATH, so no git can be started.
    if std::env::var_os("TAUTLY_HOOKS_NO_GIT").is_none() {
        let out = Command::new(std::env::current_exe().unwrap())
            .args([NO_GIT_TEST, "--exact", "--nocapture"])
            .env("PATH", "")
            .env("TAUTLY_HOOKS_NO_GIT", "1")
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout).to_string()
            + &String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{text}");
        assert!(text.contains("1 passed"), "the child ran no test: {text}");
        return;
    }
    assert!(Command::new("git").arg("--version").output().is_err());
    let top = tempfile::tempdir().unwrap();
    let t = top.path();

    // A branch with a loose ref.
    let loose = t.join("loose");
    fake_repo(&loose, SHA_A);
    assert_eq!(git_head(&loose).as_deref(), Some(SHA_A));
    assert_eq!(git_head(&loose.join("nested/dir")).as_deref(), Some(SHA_A));

    // Detached.
    let detached = t.join("detached");
    write(&detached.join(".git/HEAD"), &format!("{SHA_B}\n"));
    assert_eq!(git_head(&detached).as_deref(), Some(SHA_B));

    // A branch only in packed-refs (with a peeled tag line).
    let packed = t.join("packed");
    write(&packed.join(".git/HEAD"), "ref: refs/heads/packed\n");
    write(
        &packed.join(".git/packed-refs"),
        &format!(
            "# pack-refs with: peeled fully-peeled sorted \n{SHA_D} refs/heads/other\n\
             {SHA_C} refs/heads/packed\n{SHA_D} refs/tags/v1\n^{SHA_A}\n"
        ),
    );
    assert_eq!(git_head(&packed).as_deref(), Some(SHA_C));

    // Unborn branch and garbage.
    let unborn = t.join("unborn");
    write(&unborn.join(".git/HEAD"), "ref: refs/heads/main\n");
    assert_eq!(git_head(&unborn), None);
    let junk = t.join("junk");
    write(&junk.join(".git/HEAD"), "ref: ../../../etc/passwd\n");
    assert_eq!(git_head(&junk), None);

    // A worktree: a `.git` file names its git dir, whose `commondir` leads to the
    // branches; once with an absolute and once with a relative `gitdir:`.
    let main = t.join("main");
    fake_repo(&main, SHA_A);
    write(&main.join(".git/refs/heads/feature"), &format!("{SHA_E}\n"));
    for (name, gitdir) in [
        (
            "wt-abs",
            main.join(".git/worktrees/wt-abs").display().to_string(),
        ),
        ("wt-rel", "../main/.git/worktrees/wt-rel".to_string()),
    ] {
        let wt_git = main.join(".git/worktrees").join(name);
        write(&wt_git.join("HEAD"), "ref: refs/heads/feature\n");
        write(&wt_git.join("commondir"), "../..\n");
        let wt = t.join(name);
        write(&wt.join(".git"), &format!("gitdir: {gitdir}\n"));
        assert_eq!(git_head(&wt).as_deref(), Some(SHA_E), "{name}");
        assert_eq!(paths::project_id(&wt), paths::project_id(&main), "{name}");
    }

    let plain = t.join("plain");
    fs::create_dir(&plain).unwrap();
    assert_eq!(git_head(&plain), None);
}
