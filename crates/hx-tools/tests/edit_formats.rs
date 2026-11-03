use std::collections::HashMap;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use hx_provider::{Capabilities, EditFormat};
use hx_sandbox::{
    Child, Effect, FileMeta, HttpResponse, LocalSandbox, Sandbox, SandboxError, SpawnSpec, Url,
};
use hx_tools::edit::hashline::{self, Anchor, HashlineError, HashlineOp};
use hx_tools::edit::patch::{self, PatchError, PatchOp};
use hx_tools::{AppliedEffect, EditTool, ReadTool, Tool, ToolCx, ToolResult};
use serde_json::{Value, json};
use tempfile::TempDir;

// ---------------------------------------------------------------- helpers

struct Env<S: Sandbox = LocalSandbox> {
    dir: TempDir,
    sb: S,
    cx: ToolCx,
}

fn env_with(format: EditFormat) -> Env {
    let dir = TempDir::new().unwrap();
    let sb = LocalSandbox::new(dir.path().to_path_buf()).unwrap();
    let cx = ToolCx::new(format, dir.path().join(".spill"));
    Env { dir, sb, cx }
}

fn patch_env() -> Env {
    env_with(EditFormat::Patch)
}

fn hashline_env() -> Env {
    env_with(EditFormat::Hashline)
}

async fn put<S: Sandbox>(e: &Env<S>, path: &str, bytes: &[u8]) {
    e.sb.write_atomic(Path::new(path), bytes, None)
        .await
        .unwrap();
}

async fn slurp<S: Sandbox>(e: &Env<S>, path: &str) -> Vec<u8> {
    e.sb.read(Path::new(path)).await.unwrap()
}

fn exists<S: Sandbox>(e: &Env<S>, path: &str) -> bool {
    e.dir.path().join(path).exists()
}

/// Read output of `path` (as the model would see it).
async fn read<S: Sandbox>(e: &Env<S>, path: &str) -> String {
    let r = ReadTool.run(json!({"path": path}), &e.sb, &e.cx).await;
    assert!(!r.is_error, "{}", r.output);
    r.output
}

/// The `<n>:<hash>` anchor `read` printed for line `n` of `path`.
async fn anchor<S: Sandbox>(e: &Env<S>, path: &str, n: usize) -> String {
    let out = read(e, path).await;
    let prefix = format!("{n}:");
    out.lines()
        .find(|l| l.starts_with(&prefix))
        .and_then(|l| l.split('|').next())
        .unwrap_or_else(|| panic!("no anchor for line {n} in:\n{out}"))
        .to_string()
}

fn text(lines: &[&str]) -> String {
    let mut s = lines.join("\n");
    s.push('\n');
    s
}

async fn run_patch<S: Sandbox>(e: &Env<S>, patch: &str) -> ToolResult {
    EditTool.run(json!({ "patch": patch }), &e.sb, &e.cx).await
}

async fn run_hashline<S: Sandbox>(e: &Env<S>, path: &str, ops: Value) -> ToolResult {
    EditTool
        .run(json!({ "path": path, "ops": ops }), &e.sb, &e.cx)
        .await
}

fn caps(edit_format: EditFormat) -> Capabilities {
    Capabilities {
        edit_format,
        ..Capabilities::conservative()
    }
}

fn files_of(map: &[(&str, &str)]) -> impl Fn(&Path) -> Option<String> {
    let map: HashMap<PathBuf, String> = map
        .iter()
        .map(|(k, v)| (PathBuf::from(k), v.to_string()))
        .collect();
    move |p| map.get(p).cloned()
}

fn hl(line: usize, line_text: &str) -> Anchor {
    Anchor {
        line,
        hash: blake3::hash(line_text.as_bytes()).to_hex()[..4].to_string(),
    }
}

// ---------------------------------------------------------------- patch: pure applier

#[test]
fn patch_update_with_context_applies() {
    let files = files_of(&[("src/lib.rs", "fn a() {}\n\nfn main() {\n    old();\n}\n")]);
    let p = text(&[
        "*** Begin Patch",
        "*** Update File: src/lib.rs",
        "@@ fn main() {",
        " fn main() {",
        "-    old();",
        "+    new();",
        "+    more();",
        " }",
        "*** End Patch",
    ]);
    let out = patch::apply(&files, &p).unwrap();
    assert_eq!(
        out,
        vec![(
            PathBuf::from("src/lib.rs"),
            PatchOp::Update("fn a() {}\n\nfn main() {\n    new();\n    more();\n}\n".into())
        )]
    );
}

#[test]
fn patch_add_and_delete_file() {
    let files = files_of(&[("old.txt", "gone\n")]);
    let p = text(&[
        "*** Begin Patch",
        "*** Add File: dir/new.txt",
        "+hello",
        "+world",
        "*** Delete File: old.txt",
        "*** End Patch",
    ]);
    let out = patch::apply(&files, &p).unwrap();
    assert_eq!(
        out,
        vec![
            (
                PathBuf::from("dir/new.txt"),
                PatchOp::Add("hello\nworld\n".into())
            ),
            (PathBuf::from("old.txt"), PatchOp::Delete),
        ]
    );
}

#[test]
fn patch_context_mismatch_errors_with_hunk_index() {
    let files = files_of(&[("a.txt", "one\ntwo\nthree\nfour\n")]);
    let p = text(&[
        "*** Begin Patch",
        "*** Update File: a.txt",
        "@@",
        " one",
        "-two",
        "+TWO",
        "@@",
        " three",
        "-missing line",
        "+x",
        "*** End Patch",
    ]);
    let err = patch::apply(&files, &p).unwrap_err();
    match &err {
        PatchError::Hunk { path, hunk, .. } => {
            assert_eq!(path, Path::new("a.txt"));
            assert_eq!(*hunk, 2);
        }
        other => panic!("expected hunk error, got {other:?}"),
    }
    let msg = err.to_string();
    assert!(msg.contains("hunk 2"), "{msg}");
    assert!(msg.contains("a.txt"), "{msg}");
    // Nearest real lines help the model retry.
    assert!(msg.contains("three"), "{msg}");
}

#[test]
fn patch_ambiguous_context_is_an_error() {
    let files = files_of(&[("a.txt", "x = 1\ny\nx = 1\nz\n")]);
    let p = text(&[
        "*** Begin Patch",
        "*** Update File: a.txt",
        "@@",
        "-x = 1",
        "+x = 2",
        "*** End Patch",
    ]);
    let err = patch::apply(&files, &p).unwrap_err();
    assert!(
        matches!(
            err,
            PatchError::Hunk {
                hunk: 1,
                error: hx_tools::edit::str_replace::MatchError::Ambiguous { .. },
                ..
            }
        ),
        "{err:?}"
    );
    assert!(err.to_string().contains("2 places"), "{err}");
}

#[test]
fn patch_header_text_disambiguates_hunk() {
    let files = files_of(&[("a.py", "def f():\n    return 1\n\ndef g():\n    return 1\n")]);
    let p = text(&[
        "*** Begin Patch",
        "*** Update File: a.py",
        "@@ def g():",
        "-    return 1",
        "+    return 2",
        "*** End Patch",
    ]);
    let out = patch::apply(&files, &p).unwrap();
    assert_eq!(
        out[0].1,
        PatchOp::Update("def f():\n    return 1\n\ndef g():\n    return 2\n".into())
    );
}

#[test]
fn patch_context_matches_line_aligned_not_mid_line() {
    // `b = 1` also occurs inside `ab = 1`, but only whole lines may match.
    let files = files_of(&[("a.txt", "ab = 1\nb = 1\n")]);
    let p = text(&[
        "*** Begin Patch",
        "*** Update File: a.txt",
        "@@",
        "-b = 1",
        "+b = 2",
        "*** End Patch",
    ]);
    let out = patch::apply(&files, &p).unwrap();
    assert_eq!(out[0].1, PatchOp::Update("ab = 1\nb = 2\n".into()));
}

#[test]
fn patch_reuses_tolerant_indentation_stage() {
    let files = files_of(&[("a.rs", "impl X {\n    fn f() {\n        old();\n    }\n}\n")]);
    // The model wrote the hunk without the impl's indentation.
    let p = text(&[
        "*** Begin Patch",
        "*** Update File: a.rs",
        "@@",
        " fn f() {",
        "-    old();",
        "+    new();",
        " }",
        "*** End Patch",
    ]);
    let staged = patch::apply_staged(&files, &p).unwrap();
    assert_eq!(
        staged[0].op,
        PatchOp::Update("impl X {\n    fn f() {\n        new();\n    }\n}\n".into())
    );
    assert_eq!(staged[0].repairs.len(), 1);
    assert_eq!(staged[0].repairs[0].0, 1);
}

#[test]
fn patch_pure_addition_hunk_appends_or_follows_header() {
    let files = files_of(&[("a.txt", "one\ntwo\n")]);
    let append = text(&[
        "*** Begin Patch",
        "*** Update File: a.txt",
        "@@",
        "+three",
        "*** End Patch",
    ]);
    assert_eq!(
        patch::apply(&files, &append).unwrap()[0].1,
        PatchOp::Update("one\ntwo\nthree\n".into())
    );
    let after = text(&[
        "*** Begin Patch",
        "*** Update File: a.txt",
        "@@ one",
        "+one and a half",
        "*** End Patch",
    ]);
    assert_eq!(
        patch::apply(&files, &after).unwrap()[0].1,
        PatchOp::Update("one\none and a half\ntwo\n".into())
    );
}

#[test]
fn patch_first_hunk_may_omit_at_at_header() {
    let files = files_of(&[("a.txt", "one\ntwo\n")]);
    let p = text(&[
        "*** Begin Patch",
        "*** Update File: a.txt",
        " one",
        "-two",
        "+2",
        "*** End Patch",
    ]);
    assert_eq!(
        patch::apply(&files, &p).unwrap()[0].1,
        PatchOp::Update("one\n2\n".into())
    );
}

#[test]
fn patch_multiple_hunks_apply_against_the_original() {
    let files = files_of(&[("a.txt", "a\nb\nc\nd\ne\nf\n")]);
    let p = text(&[
        "*** Begin Patch",
        "*** Update File: a.txt",
        "@@",
        "-e",
        "+E",
        "@@",
        "-b",
        "+B",
        "+B2",
        "*** End Patch",
    ]);
    assert_eq!(
        patch::apply(&files, &p).unwrap()[0].1,
        PatchOp::Update("a\nB\nB2\nc\nd\nE\nf\n".into())
    );
}

#[test]
fn patch_overlapping_hunks_are_an_error() {
    let files = files_of(&[("a.txt", "a\nb\nc\n")]);
    let p = text(&[
        "*** Begin Patch",
        "*** Update File: a.txt",
        "@@",
        "-a",
        "-b",
        "+x",
        "@@",
        "-b",
        "-c",
        "+y",
        "*** End Patch",
    ]);
    let err = patch::apply(&files, &p).unwrap_err();
    assert!(
        matches!(err, PatchError::Overlap { a: 1, b: 2, .. }),
        "{err:?}"
    );
}

#[test]
fn patch_rejects_move_missing_files_and_duplicates() {
    let files = files_of(&[("a.txt", "x\n"), ("b.txt", "y\n")]);
    let mv = text(&[
        "*** Begin Patch",
        "*** Update File: a.txt",
        "*** Move to: c.txt",
        "@@",
        "-x",
        "+z",
        "*** End Patch",
    ]);
    assert!(matches!(
        patch::apply(&files, &mv).unwrap_err(),
        PatchError::Unsupported(_)
    ));

    let missing = text(&[
        "*** Begin Patch",
        "*** Update File: nope.txt",
        "@@",
        "-x",
        "+z",
        "*** End Patch",
    ]);
    assert_eq!(
        patch::apply(&files, &missing).unwrap_err(),
        PatchError::NotFound("nope.txt".into())
    );

    let del_missing = text(&[
        "*** Begin Patch",
        "*** Delete File: nope.txt",
        "*** End Patch",
    ]);
    assert_eq!(
        patch::apply(&files, &del_missing).unwrap_err(),
        PatchError::NotFound("nope.txt".into())
    );

    let add_existing = text(&[
        "*** Begin Patch",
        "*** Add File: a.txt",
        "+x",
        "*** End Patch",
    ]);
    assert_eq!(
        patch::apply(&files, &add_existing).unwrap_err(),
        PatchError::AlreadyExists("a.txt".into())
    );

    let dup = text(&[
        "*** Begin Patch",
        "*** Update File: a.txt",
        "@@",
        "-x",
        "+1",
        "*** Update File: a.txt",
        "@@",
        "-x",
        "+2",
        "*** End Patch",
    ]);
    assert_eq!(
        patch::apply(&files, &dup).unwrap_err(),
        PatchError::DuplicateFile("a.txt".into())
    );
}

#[test]
fn patch_rejects_malformed_envelopes() {
    let files = files_of(&[("a.txt", "x\n")]);
    for bad in [
        "",
        "just some prose\n",
        "*** Begin Patch\n*** Update File: a.txt\n@@\n-x\n+y\n", // no End Patch
        "*** Begin Patch\n*** Update File: a.txt\n@@\n?x\n*** End Patch\n", // bad line prefix
        "*** Begin Patch\n*** Frobnicate File: a.txt\n*** End Patch\n",
    ] {
        let err = patch::apply(&files, bad).unwrap_err();
        assert!(matches!(err, PatchError::Format(_)), "{bad:?}: {err:?}");
    }
}

#[test]
fn patch_accepts_plain_unified_diff() {
    let files = files_of(&[("a.txt", "one\ntwo\nthree\n"), ("gone.txt", "x\ny\n")]);
    let p = text(&[
        "diff --git a/a.txt b/a.txt",
        "index 111..222 100644",
        "--- a/a.txt",
        "+++ b/a.txt",
        "@@ -1,3 +1,3 @@",
        " one",
        "-two",
        "+TWO",
        " three",
        "--- /dev/null",
        "+++ b/new.txt",
        "@@ -0,0 +1,2 @@",
        "+n1",
        "+n2",
        "--- a/gone.txt",
        "+++ /dev/null",
        "@@ -1,2 +0,0 @@",
        "-x",
        "-y",
    ]);
    let out = patch::apply(&files, &p).unwrap();
    assert_eq!(
        out,
        vec![
            (
                PathBuf::from("a.txt"),
                PatchOp::Update("one\nTWO\nthree\n".into())
            ),
            (PathBuf::from("new.txt"), PatchOp::Add("n1\nn2\n".into())),
            (PathBuf::from("gone.txt"), PatchOp::Delete),
        ]
    );
}

#[test]
fn unified_diff_ignores_wrong_counts_and_no_newline_markers() {
    let files = files_of(&[("a.txt", "one\ntwo\nthree\n")]);
    let p = text(&[
        "--- a/a.txt",
        "+++ b/a.txt",
        "@@ -1,9 +1,9 @@",
        " one",
        "-two",
        "\\ No newline at end of file",
        "+TWO",
        " three",
    ]);
    assert_eq!(
        patch::apply(&files, &p).unwrap()[0].1,
        PatchOp::Update("one\nTWO\nthree\n".into())
    );
}

#[test]
fn unified_diff_rename_is_unsupported() {
    let files = files_of(&[("a.txt", "x\n")]);
    let p = text(&["--- a/a.txt", "+++ b/b.txt", "@@ -1 +1 @@", "-x", "+y"]);
    assert!(matches!(
        patch::apply(&files, &p).unwrap_err(),
        PatchError::Unsupported(_)
    ));
}

#[test]
fn touched_lists_every_path_without_files() {
    let p = text(&[
        "*** Begin Patch",
        "*** Update File: a.txt",
        "@@",
        "-x",
        "+y",
        "*** Add File: b/c.txt",
        "+z",
        "*** Delete File: d.txt",
        "*** End Patch",
    ]);
    let t = patch::touched(&p).unwrap();
    let got: Vec<(String, patch::Kind)> = t
        .iter()
        .map(|t| (t.path.display().to_string(), t.kind))
        .collect();
    assert_eq!(
        got,
        vec![
            ("a.txt".to_string(), patch::Kind::Update),
            ("b/c.txt".to_string(), patch::Kind::Add),
            ("d.txt".to_string(), patch::Kind::Delete),
        ]
    );
}

// ---------------------------------------------------------------- patch: tool

#[tokio::test]
async fn patch_tool_updates_adds_and_deletes_with_effects() {
    let e = patch_env();
    put(&e, "a.txt", b"one\ntwo\n").await;
    put(&e, "old.txt", b"bye\n").await;
    read(&e, "a.txt").await;
    read(&e, "old.txt").await;
    let p = text(&[
        "*** Begin Patch",
        "*** Update File: a.txt",
        "@@",
        " one",
        "-two",
        "+2",
        "*** Add File: sub/new.txt",
        "+fresh",
        "*** Delete File: old.txt",
        "*** End Patch",
    ]);
    let r = run_patch(&e, &p).await;
    assert!(!r.is_error, "{}", r.output);
    assert_eq!(slurp(&e, "a.txt").await, b"one\n2\n");
    assert_eq!(slurp(&e, "sub/new.txt").await, b"fresh\n");
    assert!(!exists(&e, "old.txt"));

    let root = e.sb.root().to_path_buf();
    assert_eq!(
        r.effects,
        vec![
            AppliedEffect::Edited {
                path: root.join("a.txt"),
                pre_image: Some(b"one\ntwo\n".to_vec()),
                post_hash: blake3::hash(b"one\n2\n"),
            },
            AppliedEffect::Edited {
                path: root.join("sub/new.txt"),
                pre_image: None,
                post_hash: blake3::hash(b"fresh\n"),
            },
            AppliedEffect::Edited {
                path: root.join("old.txt"),
                pre_image: Some(b"bye\n".to_vec()),
                post_hash: blake3::hash(b""),
            },
        ]
    );
    // The edited file can be edited again without a re-read.
    let again = text(&[
        "*** Begin Patch",
        "*** Update File: a.txt",
        "@@",
        "-2",
        "+two again",
        "*** End Patch",
    ]);
    let r = run_patch(&e, &again).await;
    assert!(!r.is_error, "{}", r.output);
    assert_eq!(slurp(&e, "a.txt").await, b"one\ntwo again\n");
}

#[tokio::test]
async fn patch_multi_file_is_all_or_nothing() {
    let e = patch_env();
    put(&e, "a.txt", b"one\ntwo\n").await;
    put(&e, "b.txt", b"x\ny\n").await;
    read(&e, "a.txt").await;
    read(&e, "b.txt").await;
    let p = text(&[
        "*** Begin Patch",
        "*** Update File: a.txt",
        "@@",
        "-two",
        "+2",
        "*** Update File: b.txt",
        "@@",
        "-does not exist",
        "+z",
        "*** Add File: c.txt",
        "+c",
        "*** End Patch",
    ]);
    let r = run_patch(&e, &p).await;
    assert!(r.is_error);
    assert!(r.output.contains("hunk 1"), "{}", r.output);
    assert!(r.output.contains("b.txt"), "{}", r.output);
    assert_eq!(slurp(&e, "a.txt").await, b"one\ntwo\n");
    assert_eq!(slurp(&e, "b.txt").await, b"x\ny\n");
    assert!(!exists(&e, "c.txt"));
    assert!(r.effects.is_empty());
}

/// Delegates to a real sandbox but fails writes to paths ending in `fail_suffix`.
struct FailingSandbox {
    inner: LocalSandbox,
    fail_suffix: &'static str,
}

#[async_trait]
impl Sandbox for FailingSandbox {
    fn root(&self) -> &Path {
        self.inner.root()
    }
    fn resolve(&self, p: &Path) -> Result<PathBuf, SandboxError> {
        self.inner.resolve(p)
    }
    async fn read(&self, p: &Path) -> Result<Vec<u8>, SandboxError> {
        self.inner.read(p).await
    }
    async fn metadata(&self, p: &Path) -> Result<FileMeta, SandboxError> {
        self.inner.metadata(p).await
    }
    async fn write_atomic(
        &self,
        p: &Path,
        bytes: &[u8],
        mode: Option<u32>,
    ) -> Result<(), SandboxError> {
        if p.to_string_lossy().ends_with(self.fail_suffix) {
            return Err(SandboxError::Io(std::io::Error::other("disk full")));
        }
        self.inner.write_atomic(p, bytes, mode).await
    }
    async fn remove_file(&self, p: &Path) -> Result<(), SandboxError> {
        self.inner.remove_file(p).await
    }
    async fn spill(&self, bytes: &[u8]) -> Result<PathBuf, SandboxError> {
        self.inner.spill(bytes).await
    }
    async fn spawn(&self, spec: SpawnSpec) -> Result<Child, SandboxError> {
        self.inner.spawn(spec).await
    }
    fn walker(&self, base: &Path) -> Result<ignore::WalkBuilder, SandboxError> {
        self.inner.walker(base)
    }
    async fn http_get(&self, url: &Url) -> Result<HttpResponse, SandboxError> {
        self.inner.http_get(url).await
    }
}

#[tokio::test]
async fn patch_write_failure_rolls_back_earlier_files() {
    let dir = TempDir::new().unwrap();
    let local = LocalSandbox::new(dir.path().to_path_buf()).unwrap();
    let e = Env {
        cx: ToolCx::new(EditFormat::Patch, dir.path().join(".spill")),
        sb: FailingSandbox {
            inner: local,
            fail_suffix: "boom.txt",
        },
        dir,
    };
    put(&e, "a.txt", b"one\ntwo\n").await;
    put(&e, "gone.txt", b"keep me\n").await;
    // An executable file: its mode must survive delete + rollback.
    e.sb.write_atomic(Path::new("run.sh"), b"#!/bin/sh\n", Some(0o755))
        .await
        .unwrap();
    for f in ["a.txt", "gone.txt", "run.sh"] {
        read(&e, f).await;
    }
    let p = text(&[
        "*** Begin Patch",
        "*** Update File: a.txt",
        "@@",
        "-two",
        "+2",
        "*** Delete File: gone.txt",
        "*** Add File: made.txt",
        "+made",
        "*** Delete File: run.sh",
        "*** Add File: boom.txt",
        "+never lands",
        "*** End Patch",
    ]);
    let r = run_patch(&e, &p).await;
    assert!(r.is_error);
    assert!(r.output.contains("boom.txt"), "{}", r.output);
    assert!(r.output.contains("rolled back"), "{}", r.output);
    assert_eq!(slurp(&e, "a.txt").await, b"one\ntwo\n");
    assert_eq!(slurp(&e, "gone.txt").await, b"keep me\n");
    assert_eq!(slurp(&e, "run.sh").await, b"#!/bin/sh\n");
    assert!(!exists(&e, "made.txt"));
    assert!(!exists(&e, "boom.txt"));
    let mode = e.sb.metadata(Path::new("run.sh")).await.unwrap().mode;
    assert_eq!(mode & 0o777, 0o755);
    assert!(r.effects.is_empty());
}

#[tokio::test]
async fn patch_tool_requires_read_for_update_and_delete() {
    let e = patch_env();
    put(&e, "a.txt", b"x\n").await;
    put(&e, "b.txt", b"y\n").await;
    let upd = text(&[
        "*** Begin Patch",
        "*** Update File: a.txt",
        "@@",
        "-x",
        "+z",
        "*** End Patch",
    ]);
    let r = run_patch(&e, &upd).await;
    assert!(r.is_error);
    assert!(r.output.contains("read the file first"), "{}", r.output);
    assert!(r.output.contains("a.txt"), "{}", r.output);

    let del = text(&["*** Begin Patch", "*** Delete File: b.txt", "*** End Patch"]);
    let r = run_patch(&e, &del).await;
    assert!(r.is_error);
    assert!(r.output.contains("read the file first"), "{}", r.output);
    assert!(exists(&e, "b.txt"));
}

#[tokio::test]
async fn patch_tool_rejects_stale_file() {
    let e = patch_env();
    put(&e, "a.txt", b"x\n").await;
    read(&e, "a.txt").await;
    put(&e, "a.txt", b"x changed\n").await;
    let p = text(&[
        "*** Begin Patch",
        "*** Update File: a.txt",
        "@@",
        "-x changed",
        "+z",
        "*** End Patch",
    ]);
    let r = run_patch(&e, &p).await;
    assert!(r.is_error);
    assert!(r.output.contains("changed since last read"), "{}", r.output);
    assert_eq!(slurp(&e, "a.txt").await, b"x changed\n");
}

#[tokio::test]
async fn patch_tool_add_over_existing_file_is_rejected() {
    let e = patch_env();
    put(&e, "a.txt", b"keep\n").await;
    let p = text(&[
        "*** Begin Patch",
        "*** Add File: a.txt",
        "+clobber",
        "*** End Patch",
    ]);
    let r = run_patch(&e, &p).await;
    assert!(r.is_error);
    assert!(r.output.contains("already exists"), "{}", r.output);
    assert_eq!(slurp(&e, "a.txt").await, b"keep\n");
}

#[tokio::test]
async fn patch_tool_confines_every_path() {
    let e = patch_env();
    put(&e, "a.txt", b"x\n").await;
    read(&e, "a.txt").await;
    let outside = TempDir::new().unwrap();
    let target = outside.path().join("evil.txt");
    for bad in ["../evil.txt".to_string(), target.display().to_string()] {
        let p = text(&[
            "*** Begin Patch",
            "*** Update File: a.txt",
            "@@",
            "-x",
            "+y",
            &format!("*** Add File: {bad}"),
            "+pwned",
            "*** End Patch",
        ]);
        let r = run_patch(&e, &p).await;
        assert!(r.is_error, "{bad}");
        assert!(r.output.contains("escapes sandbox root"), "{}", r.output);
        assert_eq!(slurp(&e, "a.txt").await, b"x\n", "nothing is written");
    }
    assert!(!target.exists());
    assert!(!e.dir.path().parent().unwrap().join("evil.txt").exists());
}

#[tokio::test]
async fn patch_tool_preserves_crlf_bom_and_reports_repairs() {
    let e = patch_env();
    put(&e, "a.txt", b"\xEF\xBB\xBF    one\r\n    two\r\nthree\r\n").await;
    read(&e, "a.txt").await;
    // The file indents more than the model did: the tolerant stage repairs it and says so.
    let p = text(&[
        "*** Begin Patch",
        "*** Update File: a.txt",
        "@@",
        " one",
        "-two",
        "+TWO",
        "*** End Patch",
    ]);
    let r = run_patch(&e, &p).await;
    assert!(!r.is_error, "{}", r.output);
    assert!(r.output.contains("applied via"), "{}", r.output);
    assert_eq!(
        slurp(&e, "a.txt").await,
        b"\xEF\xBB\xBF    one\r\n    TWO\r\nthree\r\n"
    );
}

#[tokio::test]
async fn patch_tool_bad_args_do_not_panic() {
    let e = patch_env();
    let r = EditTool.run(json!({"nope": 1}), &e.sb, &e.cx).await;
    assert!(r.is_error);
    let r = run_patch(&e, "not a patch").await;
    assert!(r.is_error);
}

#[test]
fn patch_effect_lists_every_touched_path() {
    let p = text(&[
        "*** Begin Patch",
        "*** Update File: a.txt",
        "@@",
        "-x",
        "+y",
        "*** Add File: b/c.txt",
        "+z",
        "*** Delete File: .github/workflows/ci.yml",
        "*** End Patch",
    ]);
    let eff = EditTool.effect(&json!({ "patch": p })).unwrap();
    assert_eq!(
        eff,
        Effect::Write {
            paths: vec![
                "a.txt".into(),
                "b/c.txt".into(),
                ".github/workflows/ci.yml".into()
            ]
        }
    );
    assert!(EditTool.effect(&json!({ "patch": "garbage" })).is_err());
}

// ---------------------------------------------------------------- hashline: pure applier

#[test]
fn hashline_replace_range() {
    let src = "a\nb\nc\nd\n";
    let ops = [HashlineOp::Replace {
        start: hl(2, "b"),
        end: hl(3, "c"),
        lines: vec!["X".into(), "Y".into(), "Z".into()],
    }];
    assert_eq!(hashline::apply(src, &ops).unwrap(), "a\nX\nY\nZ\nd\n");
}

#[test]
fn hashline_stale_anchor_hash_errors() {
    let src = "a\nb\nc\n";
    let ops = [HashlineOp::Replace {
        start: hl(2, "not b"),
        end: hl(2, "not b"),
        lines: vec!["X".into()],
    }];
    assert_eq!(
        hashline::apply(src, &ops).unwrap_err(),
        HashlineError::Stale { line: 2 }
    );
    // The end anchor is checked too, and nothing is repaired by searching elsewhere:
    // line 3's text exists at line 3, but anchored as line 1 it is still stale.
    let ops = [HashlineOp::Delete {
        start: hl(1, "a"),
        end: hl(1, "c"),
    }];
    assert_eq!(
        hashline::apply(src, &ops).unwrap_err(),
        HashlineError::Stale { line: 1 }
    );
}

#[test]
fn hashline_insert_after_last_line() {
    let ops = [HashlineOp::InsertAfter {
        at: hl(3, "c"),
        lines: vec!["d".into(), "e".into()],
    }];
    assert_eq!(
        hashline::apply("a\nb\nc\n", &ops).unwrap(),
        "a\nb\nc\nd\ne\n"
    );
    // No trailing newline in the source: still appended on its own line.
    assert_eq!(hashline::apply("a\nb\nc", &ops).unwrap(), "a\nb\nc\nd\ne\n");
}

#[test]
fn hashline_delete_and_insert_in_the_middle() {
    let src = "a\nb\nc\nd\n";
    let ops = [HashlineOp::Delete {
        start: hl(2, "b"),
        end: hl(3, "c"),
    }];
    assert_eq!(hashline::apply(src, &ops).unwrap(), "a\nd\n");
    let ops = [HashlineOp::InsertAfter {
        at: hl(1, "a"),
        lines: vec!["a2".into()],
    }];
    assert_eq!(hashline::apply(src, &ops).unwrap(), "a\na2\nb\nc\nd\n");
}

#[test]
fn hashline_ops_are_validated_against_the_original_and_applied_bottom_up() {
    let src = "a\nb\nc\nd\ne\n";
    // Listed top-down and out of order; line numbers all refer to the original.
    let ops = [
        HashlineOp::Replace {
            start: hl(1, "a"),
            end: hl(1, "a"),
            lines: vec!["A1".into(), "A2".into()],
        },
        HashlineOp::Delete {
            start: hl(4, "d"),
            end: hl(4, "d"),
        },
        HashlineOp::InsertAfter {
            at: hl(2, "b"),
            lines: vec!["b2".into()],
        },
    ];
    assert_eq!(hashline::apply(src, &ops).unwrap(), "A1\nA2\nb\nb2\nc\ne\n");
}

#[test]
fn hashline_adjacent_ops_keep_their_order() {
    let src = "a\nb\nc\n";
    // Insert after line 1 and replace line 2: insertion lands before the replacement.
    let ops = [
        HashlineOp::Replace {
            start: hl(2, "b"),
            end: hl(2, "b"),
            lines: vec!["B".into()],
        },
        HashlineOp::InsertAfter {
            at: hl(1, "a"),
            lines: vec!["ins".into()],
        },
        HashlineOp::InsertAfter {
            at: hl(2, "b"),
            lines: vec!["after".into()],
        },
    ];
    assert_eq!(hashline::apply(src, &ops).unwrap(), "a\nins\nB\nafter\nc\n");
}

#[test]
fn hashline_overlapping_ops_are_an_error() {
    let src = "a\nb\nc\nd\n";
    let rep = |s: usize, st: &str, e: usize, et: &str| HashlineOp::Replace {
        start: hl(s, st),
        end: hl(e, et),
        lines: vec!["X".into()],
    };
    let err = hashline::apply(src, &[rep(1, "a", 2, "b"), rep(2, "b", 3, "c")]).unwrap_err();
    assert!(matches!(err, HashlineError::Overlap { .. }), "{err:?}");
    // An insertion inside a replaced range.
    let err = hashline::apply(
        src,
        &[
            rep(2, "b", 3, "c"),
            HashlineOp::InsertAfter {
                at: hl(2, "b"),
                lines: vec!["Y".into()],
            },
        ],
    )
    .unwrap_err();
    assert!(matches!(err, HashlineError::Overlap { .. }), "{err:?}");
    // Two insertions at the same place are ambiguous.
    let ins = || HashlineOp::InsertAfter {
        at: hl(1, "a"),
        lines: vec!["Y".into()],
    };
    let err = hashline::apply(src, &[ins(), ins()]).unwrap_err();
    assert!(matches!(err, HashlineError::Overlap { .. }), "{err:?}");
}

#[test]
fn hashline_range_errors() {
    let src = "a\nb\n";
    let err = hashline::apply(
        src,
        &[HashlineOp::Delete {
            start: hl(2, "b"),
            end: hl(1, "a"),
        }],
    )
    .unwrap_err();
    assert_eq!(err, HashlineError::BadRange { start: 2, end: 1 });
    let err = hashline::apply(
        src,
        &[HashlineOp::InsertAfter {
            at: hl(5, "x"),
            lines: vec![],
        }],
    )
    .unwrap_err();
    assert_eq!(err, HashlineError::OutOfRange { line: 5, total: 2 });
    let err = hashline::apply(
        src,
        &[HashlineOp::InsertAfter {
            at: Anchor {
                line: 0,
                hash: "0000".into(),
            },
            lines: vec![],
        }],
    )
    .unwrap_err();
    assert_eq!(err, HashlineError::OutOfRange { line: 0, total: 2 });
}

#[test]
fn hashline_blank_only_results_keep_their_newline() {
    // Deleting the only non-blank line leaves one blank line, not an empty file.
    let ops = [HashlineOp::Delete {
        start: hl(1, "a"),
        end: hl(1, "a"),
    }];
    assert_eq!(hashline::apply("a\n\n", &ops).unwrap(), "\n");
    // Replacing the only (blank) line with a blank line is a no-op.
    let ops = [HashlineOp::Replace {
        start: hl(1, ""),
        end: hl(1, ""),
        lines: vec![String::new()],
    }];
    assert_eq!(hashline::apply("\n", &ops).unwrap(), "\n");
    // Deleting every line still empties the file.
    let ops = [HashlineOp::Delete {
        start: hl(1, "a"),
        end: hl(1, "a"),
    }];
    assert_eq!(hashline::apply("a\n", &ops).unwrap(), "");
}

#[test]
fn hashline_lines_with_embedded_newlines_are_split() {
    let ops = [HashlineOp::Replace {
        start: hl(1, "a"),
        end: hl(1, "a"),
        lines: vec!["x\ny".into(), "z\r".into()],
    }];
    assert_eq!(hashline::apply("a\nb\n", &ops).unwrap(), "x\ny\nz\nb\n");
}

#[test]
fn anchor_parse_accepts_read_output_and_rejects_junk() {
    assert_eq!(
        Anchor::parse("12:ab3f").unwrap(),
        Anchor {
            line: 12,
            hash: "ab3f".into()
        }
    );
    assert_eq!(Anchor::parse(" 3:AB3F ").unwrap().hash, "ab3f");
    for bad in [
        "",
        "12",
        "12:",
        "12:ab3",
        "12:ab3fg",
        "x:ab3f",
        "12:ab3f|foo",
        ":ab3f",
    ] {
        assert!(
            matches!(Anchor::parse(bad), Err(HashlineError::BadAnchor(_))),
            "{bad}"
        );
    }
}

#[test]
fn hashline_first_line_of_bom_file_matches_what_read_printed() {
    // `read` hashes line 1 including the BOM; the applier sees BOM-less text.
    let line1_as_read = "\u{FEFF}first";
    let ops = [HashlineOp::Replace {
        start: hl(1, line1_as_read),
        end: hl(1, line1_as_read),
        lines: vec!["FIRST".into()],
    }];
    assert_eq!(
        hashline::apply("first\nsecond\n", &ops).unwrap(),
        "FIRST\nsecond\n"
    );
}

// ---------------------------------------------------------------- hashline: tool

#[tokio::test]
async fn hashline_tool_edits_using_read_anchors() {
    let e = hashline_env();
    put(&e, "a.txt", b"one\ntwo\nthree\n").await;
    let a2 = anchor(&e, "a.txt", 2).await;
    let a3 = anchor(&e, "a.txt", 3).await;
    let r = run_hashline(
        &e,
        "a.txt",
        json!([
            {"op": "replace", "start": a2, "end": a2, "lines": ["TWO", "2b"]},
            {"op": "insert_after", "at": a3, "lines": ["four"]}
        ]),
    )
    .await;
    assert!(!r.is_error, "{}", r.output);
    assert_eq!(slurp(&e, "a.txt").await, b"one\nTWO\n2b\nthree\nfour\n");
    assert_eq!(
        r.effects,
        vec![AppliedEffect::Edited {
            path: e.sb.root().join("a.txt"),
            pre_image: Some(b"one\ntwo\nthree\n".to_vec()),
            post_hash: blake3::hash(b"one\nTWO\n2b\nthree\nfour\n"),
        }]
    );
    // Consecutive edit needs only fresh anchors, not a fresh stale-guard read.
    let a1 = anchor(&e, "a.txt", 1).await;
    let r = run_hashline(
        &e,
        "a.txt",
        json!([{"op": "delete", "start": a1, "end": a1}]),
    )
    .await;
    assert!(!r.is_error, "{}", r.output);
    assert_eq!(slurp(&e, "a.txt").await, b"TWO\n2b\nthree\nfour\n");
}

#[tokio::test]
async fn hashline_tool_reports_stale_anchor_and_leaves_file_alone() {
    let e = hashline_env();
    put(&e, "a.txt", b"one\ntwo\n").await;
    let a2 = anchor(&e, "a.txt", 2).await;
    // Someone changes line 2 but the file is then re-read, so the stale-read guard is
    // satisfied and only the anchor hash can catch it.
    put(&e, "a.txt", b"one\nTWO changed\n").await;
    read(&e, "a.txt").await;
    let r = run_hashline(
        &e,
        "a.txt",
        json!([{"op": "replace", "start": a2, "end": a2, "lines": ["x"]}]),
    )
    .await;
    assert!(r.is_error);
    assert!(r.output.contains("line 2"), "{}", r.output);
    assert!(r.output.contains("changed"), "{}", r.output);
    assert_eq!(slurp(&e, "a.txt").await, b"one\nTWO changed\n");
}

#[tokio::test]
async fn hashline_tool_applies_stale_read_guard() {
    let e = hashline_env();
    put(&e, "a.txt", b"one\n").await;
    // Never read: no anchors can exist, but a guessed one must still be refused.
    let guessed = format!("1:{}", &blake3::hash(b"one").to_hex()[..4]);
    let r = run_hashline(
        &e,
        "a.txt",
        json!([{"op": "delete", "start": guessed, "end": guessed}]),
    )
    .await;
    assert!(r.is_error);
    assert!(r.output.contains("read the file first"), "{}", r.output);

    let a1 = anchor(&e, "a.txt", 1).await;
    // Changed content (not just mtime, which can be too coarse to move on a fast rewrite).
    put(&e, "a.txt", b"ONE\n").await;
    let r = run_hashline(
        &e,
        "a.txt",
        json!([{"op": "delete", "start": a1, "end": a1}]),
    )
    .await;
    assert!(r.is_error);
    assert!(r.output.contains("changed since last read"), "{}", r.output);
    assert_eq!(slurp(&e, "a.txt").await, b"ONE\n");
}

#[tokio::test]
async fn hashline_tool_preserves_crlf_bom_and_no_trailing_newline() {
    let e = hashline_env();
    put(&e, "a.txt", b"\xEF\xBB\xBFone\r\ntwo").await;
    let a1 = anchor(&e, "a.txt", 1).await;
    let a2 = anchor(&e, "a.txt", 2).await;
    let r = run_hashline(
        &e,
        "a.txt",
        json!([
            {"op": "replace", "start": a1, "end": a1, "lines": ["ONE"]},
            {"op": "insert_after", "at": a2, "lines": ["three"]}
        ]),
    )
    .await;
    assert!(!r.is_error, "{}", r.output);
    assert_eq!(slurp(&e, "a.txt").await, b"\xEF\xBB\xBFONE\r\ntwo\r\nthree");
}

#[tokio::test]
async fn hashline_tool_rejects_bad_args_and_escapes() {
    let e = hashline_env();
    put(&e, "a.txt", b"one\n").await;
    let a1 = anchor(&e, "a.txt", 1).await;
    for args in [
        json!({"path": "a.txt", "ops": [{"op": "frobnicate"}]}),
        json!({"path": "a.txt", "ops": [{"op": "delete", "start": "garbage", "end": a1}]}),
        json!({"path": "a.txt", "ops": []}),
        json!({"path": "a.txt"}),
        json!({"path": "../x.txt", "ops": [{"op": "delete", "start": a1, "end": a1}]}),
    ] {
        let r = EditTool.run(args.clone(), &e.sb, &e.cx).await;
        assert!(r.is_error, "{args}");
    }
    assert_eq!(slurp(&e, "a.txt").await, b"one\n");
}

#[tokio::test]
async fn hashline_tool_names_the_missing_field_of_an_op() {
    let e = hashline_env();
    put(&e, "a.txt", b"one\ntwo\n").await;
    let a1 = anchor(&e, "a.txt", 1).await;
    let cases = [
        (json!({"op": "replace", "start": a1, "lines": ["x"]}), "end"),
        (json!({"op": "replace", "start": a1, "end": a1}), "lines"),
        (json!({"op": "insert_after", "lines": ["x"]}), "at"),
        (json!({"op": "insert_after", "at": a1}), "lines"),
        (json!({"op": "delete", "end": a1}), "start"),
    ];
    for (op, field) in cases {
        let r = run_hashline(&e, "a.txt", json!([op])).await;
        assert!(r.is_error, "{op}");
        assert!(
            r.output.contains(&format!("missing `{field}`")),
            "{op}: {}",
            r.output
        );
        assert!(r.output.contains("op 1"), "{op}: {}", r.output);
    }
    let r = run_hashline(&e, "a.txt", json!([{"op": "frobnicate"}])).await;
    assert!(r.is_error);
    assert!(r.output.contains("replace"), "{}", r.output);
    assert!(r.output.contains("insert_after"), "{}", r.output);
    assert_eq!(slurp(&e, "a.txt").await, b"one\ntwo\n");
}

#[test]
fn hashline_effect_is_the_single_path() {
    let eff = EditTool
        .effect(&json!({"path": "src/a.rs", "ops": [{"op": "delete", "start": "1:abcd", "end": "1:abcd"}]}))
        .unwrap();
    assert_eq!(
        eff,
        Effect::Write {
            paths: vec!["src/a.rs".into()]
        }
    );
    assert!(EditTool.effect(&json!({"path": "a", "ops": "no"})).is_err());
}

// ---------------------------------------------------------------- spec

#[test]
fn edit_tool_schema_switches_with_format() {
    let patch = EditTool.spec(&caps(EditFormat::Patch));
    assert_eq!(patch.name, "edit");
    assert!(patch.schema["properties"].get("patch").is_some());
    assert!(patch.schema["properties"].get("ops").is_none());
    assert!(patch.description.contains("*** Begin Patch"));

    let hashline = EditTool.spec(&caps(EditFormat::Hashline));
    assert_eq!(hashline.name, "edit");
    assert!(hashline.schema["properties"].get("ops").is_some());
    assert!(hashline.schema["properties"].get("patch").is_none());
    assert!(hashline.description.contains("insert_after"));

    // Hashline is the default for the models least likely to cope with `oneOf`/`const`.
    for fmt in [
        EditFormat::Patch,
        EditFormat::Hashline,
        EditFormat::StrReplace,
    ] {
        let schema = EditTool.spec(&caps(fmt)).schema.to_string();
        assert!(!schema.contains("oneOf"), "{fmt:?}: {schema}");
        assert!(!schema.contains("const"), "{fmt:?}: {schema}");
    }
    let op = &hashline.schema["properties"]["ops"]["items"];
    assert_eq!(
        op["properties"]["op"]["enum"],
        json!(["replace", "insert_after", "delete"])
    );
    assert_eq!(op["required"], json!(["op"]));

    let str_replace = EditTool.spec(&caps(EditFormat::StrReplace));
    assert!(str_replace.schema["properties"].get("old_text").is_some());
}
