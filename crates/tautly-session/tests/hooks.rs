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
    list_here_in, list_in, paths,
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
        parent_step: Some(4),
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
    assert_eq!(h.parent_step, Some(4));
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
    assert_eq!(h.parent_step, None);
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
    // branches, and whose `gitdir` names the `.git` file back; once with an absolute and
    // once with relative links (git's `worktree.useRelativePaths`).
    let main = t.join("main");
    fake_repo(&main, SHA_A);
    write(&main.join(".git/refs/heads/feature"), &format!("{SHA_E}\n"));
    for (name, gitdir, back) in [
        (
            "wt-abs",
            main.join(".git/worktrees/wt-abs").display().to_string(),
            t.join("wt-abs/.git").display().to_string(),
        ),
        (
            "wt-rel",
            "../main/.git/worktrees/wt-rel".to_string(),
            "../../../../wt-rel/.git".to_string(),
        ),
    ] {
        let wt_git = main.join(".git/worktrees").join(name);
        write(&wt_git.join("HEAD"), "ref: refs/heads/feature\n");
        write(&wt_git.join("commondir"), "../..\n");
        write(&wt_git.join("gitdir"), &format!("{back}\n"));
        let wt = t.join(name);
        write(&wt.join(".git"), &format!("gitdir: {gitdir}\n"));
        assert_eq!(git_head(&wt).as_deref(), Some(SHA_E), "{name}");
        assert_eq!(paths::project_id(&wt), paths::project_id(&main), "{name}");
    }

    let plain = t.join("plain");
    fs::create_dir(&plain).unwrap();
    assert_eq!(git_head(&plain), None);
}

#[test]
fn packed_refs_scan_skips_non_utf8_lines_and_refuses_oversize_files() {
    let top = tempfile::tempdir().unwrap();
    let repo = top.path().join("repo");
    write(&repo.join(".git/HEAD"), "ref: refs/heads/packed\n");
    let mut packed = b"# pack-refs with: peeled sorted \n".to_vec();
    packed.extend_from_slice(SHA_A.as_bytes());
    packed.extend_from_slice(b" refs/tags/caf\xe9\n");
    packed.extend_from_slice(b"\xff\xfe not utf-8 at all\n");
    packed.extend_from_slice(format!("{SHA_C} refs/heads/packed\n").as_bytes());
    fs::write(repo.join(".git/packed-refs"), &packed).unwrap();
    assert_eq!(git_head(&repo).as_deref(), Some(SHA_C));

    // Too big to scan within the start-up budget: no head (it is optional), rather than a
    // slow start. The branch line is at the very end.
    let mut big = vec![b'#'; 9 * 1024 * 1024];
    big.push(b'\n');
    big.extend_from_slice(format!("{SHA_C} refs/heads/packed\n").as_bytes());
    fs::write(repo.join(".git/packed-refs"), &big).unwrap();
    assert_eq!(git_head(&repo), None);
    // A loose ref is still read.
    write(&repo.join(".git/refs/heads/packed"), &format!("{SHA_B}\n"));
    assert_eq!(git_head(&repo).as_deref(), Some(SHA_B));
}

#[test]
fn summaries_carry_root_label_origin_parent_and_continue_here_keeps_this_tree() {
    let data = tempfile::tempdir().unwrap();
    let top = tempfile::tempdir().unwrap();
    let main = top.path().join("main");
    let wt = top.path().join("wt");
    fs::create_dir(&main).unwrap();
    git(&main, &["init", "-q"]);
    write(&main.join("a.txt"), "a\n");
    git(&main, &["add", "-A"]);
    git(&main, &["commit", "-q", "-m", "one"]);
    git(
        &main,
        &["worktree", "add", "-q", "-b", "side", wt.to_str().unwrap()],
    );

    let a = SessionWriter::<Msg>::create_in(data.path(), &main, HeaderInit::default()).unwrap();
    let b = SessionWriter::<Msg>::create_in(
        data.path(),
        &wt,
        HeaderInit {
            parent: Some((a.session_id().to_string(), 0)),
            label: Some("side-try".into()),
            origin: Origin::WhatIf,
            ..HeaderInit::default()
        },
    )
    .unwrap();

    // The project's listing spans both worktrees, and says where each session ran.
    let all = list_in(data.path(), &wt).unwrap();
    assert_eq!(all.len(), 2);
    let sb = all.iter().find(|s| s.id == b.session_id()).unwrap();
    assert_eq!(sb.root, wt.canonicalize().unwrap());
    assert_eq!(sb.label.as_deref(), Some("side-try"));
    assert_eq!(sb.origin, Origin::WhatIf);
    assert_eq!(sb.parent, Some((a.session_id().to_string(), 0)));
    let sa = all.iter().find(|s| s.id == a.session_id()).unwrap();
    assert_eq!(sa.root, main.canonicalize().unwrap());
    assert_eq!((sa.label.clone(), sa.origin), (None, Origin::User));

    // "Continue here" only offers this tree's sessions.
    let here: Vec<_> = list_here_in(data.path(), &wt)
        .unwrap()
        .into_iter()
        .map(|s| s.id)
        .collect();
    assert_eq!(here, [b.session_id().to_string()]);
    let here: Vec<_> = list_here_in(data.path(), &main)
        .unwrap()
        .into_iter()
        .map(|s| s.id)
        .collect();
    assert_eq!(here, [a.session_id().to_string()]);
}

#[test]
fn t_never_goes_backwards_even_after_reopen() {
    let (data, project) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut w = SessionWriter::<Msg>::create_in(data.path(), project.path(), HeaderInit::default())
        .unwrap();
    w.append(&msg("now")).unwrap();
    w.checkpoint().unwrap();
    let path = w.path().to_path_buf();
    drop(w);
    // A record written while the clock was far ahead (then stepped back).
    let future = now_ms() + 3_600_000;
    OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(
            format!("{{\"type\":\"record\",\"offset\":1,\"t\":{future},\"data\":{{\"text\":\"ahead\"}}}}\n")
                .as_bytes(),
        )
        .unwrap();
    let mut w = SessionWriter::<Msg>::reopen(&path).unwrap();
    w.append(&msg("after")).unwrap();
    w.append(&msg("again")).unwrap();
    w.checkpoint().unwrap();
    let ts: Vec<u64> = SessionReader::<Msg>::open(&path)
        .unwrap()
        .entries()
        .map(|e| e.unwrap().t_ms)
        .collect();
    assert_eq!(ts.len(), 4);
    assert!(ts.windows(2).all(|w| w[0] <= w[1]), "{ts:?}");
    assert_eq!(ts[2], future);
}

#[test]
fn fork_copy_keeps_records_verbatim_with_their_original_t() {
    let data = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let mut parent =
        SessionWriter::<Msg>::create_in(data.path(), project.path(), HeaderInit::default())
            .unwrap();
    parent.append(&msg("one")).unwrap();
    parent.append(&msg("two")).unwrap();
    parent.checkpoint().unwrap();
    let parent_path = parent.path().to_path_buf();
    drop(parent);
    // A record from a newer version, with a field this one does not know.
    let later = now_ms() + 60_000;
    OpenOptions::new()
        .append(true)
        .open(&parent_path)
        .unwrap()
        .write_all(
            format!(
                "{{\"type\":\"record\",\"offset\":2,\"t\":{later},\
                 \"data\":{{\"text\":\"three\",\"added_later\":[1,2]}}}}\n"
            )
            .as_bytes(),
        )
        .unwrap();
    let parent_entries: Vec<_> = SessionReader::<Msg>::open(&parent_path)
        .unwrap()
        .entries()
        .map(Result::unwrap)
        .collect();

    let mut child =
        SessionWriter::<Msg>::create_in(data.path(), project.path(), HeaderInit::default())
            .unwrap();
    assert_eq!(child.copy_from(&parent_path, 3).unwrap(), 3);
    child.append(&msg("child")).unwrap();
    child.checkpoint().unwrap();
    let child_entries: Vec<_> = SessionReader::<Msg>::open(child.path())
        .unwrap()
        .entries()
        .map(Result::unwrap)
        .collect();
    assert_eq!(child_entries.len(), 4);
    for (c, p) in child_entries.iter().zip(&parent_entries) {
        assert_eq!((c.offset, c.t_ms, &c.rec), (p.offset, p.t_ms, &p.rec));
    }
    // After the copy the clock continues from the copied records.
    assert!(child_entries[3].t_ms >= later);
    let raw = fs::read_to_string(child.path()).unwrap();
    assert!(
        raw.lines()
            .nth(3)
            .unwrap()
            .contains(r#""added_later":[1,2]"#),
        "{raw}"
    );

    // Fewer than asked when the parent is shorter; a prefix when asked for less.
    let mut short =
        SessionWriter::<Msg>::create_in(data.path(), project.path(), HeaderInit::default())
            .unwrap();
    assert_eq!(short.copy_from(&parent_path, 2).unwrap(), 2);
    let mut long =
        SessionWriter::<Msg>::create_in(data.path(), project.path(), HeaderInit::default())
            .unwrap();
    assert_eq!(long.copy_from(&parent_path, 10).unwrap(), 3);
}

/// Review B-I2 (probe P2): a checkout that arrived as an archive with its `.git` must not
/// take another project's id (which keys its session logs and private memory episodes)
/// by naming that project's git directory. A `.git` file or `commondir` counts only when
/// git's own links name this work tree back.
#[test]
fn git_pointers_without_a_back_link_do_not_take_another_projects_id() {
    let top = tempfile::tempdir().unwrap();
    let t = top.path().canonicalize().unwrap();
    let victim = t.join("victim");
    fake_repo(&victim, SHA_A);
    // A real linked worktree of the victim, at `t/real`.
    let real_git = victim.join(".git/worktrees/real");
    write(&real_git.join("HEAD"), "ref: refs/heads/main\n");
    write(&real_git.join("commondir"), "../..\n");
    write(
        &real_git.join("gitdir"),
        &format!("{}\n", t.join("real/.git").display()),
    );
    write(
        &t.join("real/.git"),
        &format!("gitdir: {}\n", real_git.display()),
    );
    let victim_id = paths::project_id(&victim);
    assert_eq!(paths::project_id(&t.join("real")), victim_id);
    assert_eq!(git_head(&t.join("real")).as_deref(), Some(SHA_A));

    // P2: a `.git` directory whose `commondir` names the victim's.
    let p2 = t.join("p2");
    write(&p2.join(".git/HEAD"), "ref: refs/heads/main\n");
    write(&p2.join(".git/commondir"), "../../victim/.git\n");
    // A `.git` file naming the victim's git directory itself.
    let direct = t.join("direct");
    write(&direct.join(".git"), "gitdir: ../victim/.git\n");
    // A `.git` file naming the victim's real worktree (whose back-link names `t/real`).
    let borrowed = t.join("borrowed");
    write(
        &borrowed.join(".git"),
        &format!("gitdir: {}\n", real_git.display()),
    );
    // A git directory of its own that names this `.git` back, but whose `commondir` is
    // the victim's (it is not one of the victim's worktrees).
    let forged = t.join("forged");
    let fake = forged.join("fake/worktrees/w");
    write(&forged.join(".git"), "gitdir: fake/worktrees/w\n");
    write(&fake.join("HEAD"), "ref: refs/heads/main\n");
    write(
        &fake.join("gitdir"),
        &format!("{}\n", forged.join(".git").display()),
    );
    write(&fake.join("commondir"), "../../../../victim/.git\n");
    // A `.git` symbolic link to the victim's.
    let linked = t.join("linked");
    fs::create_dir(&linked).unwrap();
    std::os::unix::fs::symlink(victim.join(".git"), linked.join(".git")).unwrap();

    for root in [&p2, &direct, &borrowed, &forged, &linked] {
        let id = paths::project_id(root);
        assert_ne!(id, victim_id, "{}", root.display());
        assert_eq!(id, paths::project_hash(root), "{}", root.display());
        // Nor is the victim's head read as this tree's.
        assert_eq!(git_head(root), None, "{}", root.display());
    }
    // The two hostile checkouts of P2's kind cannot share one id either.
    assert_ne!(paths::project_id(&p2), paths::project_id(&direct));
}

/// A submodule's `.git` file names `<super>/.git/modules/<name>`, which has no `gitdir`
/// back-link: its `core.worktree` names the submodule's work tree instead.
#[test]
fn submodule_git_dir_is_trusted_through_core_worktree() {
    let top = tempfile::tempdir().unwrap();
    let t = top.path().canonicalize().unwrap();
    let sup = t.join("super");
    fake_repo(&sup, SHA_A);
    let module = sup.join(".git/modules/sub");
    write(&module.join("HEAD"), &format!("{SHA_B}\n"));
    write(
        &module.join("config"),
        "[core]\n\trepositoryformatversion = 0\n\tbare = false\n\tworktree = ../../../sub\n\
         [remote \"origin\"]\n\turl = /elsewhere\n",
    );
    let sub = sup.join("sub");
    write(&sub.join(".git"), "gitdir: ../.git/modules/sub\n");
    assert_eq!(git_head(&sub).as_deref(), Some(SHA_B));
    assert_ne!(paths::project_id(&sub), paths::project_id(&sup));

    // Another checkout naming the same module dir is not its work tree.
    let other = t.join("other");
    write(
        &other.join(".git"),
        &format!("gitdir: {}\n", module.display()),
    );
    assert_eq!(git_head(&other), None);
    assert_eq!(paths::project_id(&other), paths::project_hash(&other));
    // Nor does a `worktree` key of another section count.
    write(
        &module.join("config"),
        "[core]\n\tbare = false\n[remote \"x\"]\n\tworktree = ../../../sub\n",
    );
    assert_eq!(git_head(&sub), None);
}

/// Worktrees (absolute and relative links) and submodules made by git itself keep their
/// ids and heads.
#[test]
fn real_git_worktrees_and_submodules_are_trusted() {
    let top = tempfile::tempdir().unwrap();
    let t = top.path().canonicalize().unwrap();
    let lib = t.join("lib");
    fs::create_dir(&lib).unwrap();
    git(&lib, &["init", "-q"]);
    write(&lib.join("a.txt"), "a\n");
    git(&lib, &["add", "-A"]);
    git(&lib, &["commit", "-q", "-m", "one"]);
    let head = git(&lib, &["rev-parse", "HEAD"]);
    for (name, relative) in [("wt", "false"), ("wt-rel", "true")] {
        let wt = t.join(name);
        let setting = format!("worktree.useRelativePaths={relative}");
        git(
            &lib,
            &[
                "-c",
                &setting,
                "worktree",
                "add",
                "-q",
                "--detach",
                wt.to_str().unwrap(),
            ],
        );
        assert_eq!(paths::project_id(&wt), paths::project_id(&lib), "{name}");
        assert_eq!(git_head(&wt).as_deref(), Some(head.as_str()), "{name}");
    }
    let sup = t.join("super");
    fs::create_dir(&sup).unwrap();
    git(&sup, &["init", "-q"]);
    git(
        &sup,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "-q",
            "../lib",
            "sub",
        ],
    );
    let sub = sup.join("sub");
    assert_eq!(git_head(&sub).as_deref(), Some(head.as_str()));
    assert_ne!(paths::project_id(&sub), paths::project_hash(&sub));
}
