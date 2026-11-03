//! Diff previews (spec 7.11.4): `Tool::preview` runs the edit pipeline without writing, its
//! diff is the diff `run` applies, and diffs are capped for display.

use std::path::{Path, PathBuf};

use proptest::prelude::*;
use serde_json::{Value, json};
use tautly_provider::EditFormat;
use tautly_sandbox::{LocalSandbox, Sandbox};
use tautly_tools::preview::{PREVIEW_MAX_BYTES, PREVIEW_MAX_LINES};
use tautly_tools::{
    DiffPreview, EditTool, Preview, ReadTool, Tool, ToolCx, ToolResult, WriteTool, unified_diff,
};
use tempfile::TempDir;

struct Env {
    dir: TempDir,
    sb: LocalSandbox,
    cx: ToolCx,
}

fn env(format: EditFormat) -> Env {
    let dir = TempDir::new().unwrap();
    let sb = LocalSandbox::new(dir.path().to_path_buf()).unwrap();
    let cx = ToolCx::new(format, dir.path().join(".spill")).with_turn(1);
    Env { dir, sb, cx }
}

impl Env {
    async fn put(&self, rel: &str, bytes: &[u8]) {
        self.sb
            .write_atomic(Path::new(rel), bytes, None)
            .await
            .unwrap();
    }

    async fn slurp(&self, rel: &str) -> Vec<u8> {
        self.sb.read(Path::new(rel)).await.unwrap()
    }

    /// Read `rel` in full, as the model would before editing it; returns the output.
    async fn read(&self, rel: &str) -> String {
        let r = ReadTool
            .run(json!({"path": rel, "full": true}), &self.sb, &self.cx)
            .await;
        assert!(!r.is_error, "{}", r.output);
        r.output
    }

    async fn preview(&self, tool: &dyn Tool, args: &Value) -> Result<Preview, ToolResult> {
        tool.preview(args, &self.sb, &self.cx)
            .await
            .expect("this tool has a preview")
    }

    /// `run` with the bases of `preview` approved, as the engine does after an approval.
    async fn run_approved(&self, tool: &dyn Tool, args: Value, preview: &Preview) -> ToolResult {
        let cx = ToolCx::new(self.cx.edit_format(), self.dir.path().join(".spill"))
            .with_reads(self.cx.reads().clone())
            .with_turn(1)
            .with_approved_bases(preview.bases.clone());
        tool.run(args, &self.sb, &cx).await
    }

    fn full(&self, rel: &str) -> PathBuf {
        self.sb.resolve(Path::new(rel)).unwrap()
    }
}

fn lines_of(n: usize, tag: &str) -> String {
    (1..=n).map(|i| format!("{tag} line {i}\n")).collect()
}

fn marker_ok(d: &DiffPreview) {
    assert!(d.truncated, "{d:?}");
    let kept = d.unified.lines().count() as u32 - 1;
    let marker = format!("… {} more lines (diff truncated)", d.lines - kept);
    assert!(d.unified.ends_with(&marker), "{:?}", d.unified);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    /// Whatever `str_replace` edit is made, the previewed diff is the applied diff.
    #[test]
    fn preview_equals_applied_diff(
        words in prop::collection::vec("[a-z]{1,8}", 1..30),
        start in 0usize..30,
        len in 1usize..5,
        new in prop::collection::vec("[a-z ]{0,10}", 0..5),
        crlf in any::<bool>(),
    ) {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let e = env(EditFormat::StrReplace);
            // Numbered lines keep every old_text unique.
            let lines: Vec<String> =
                words.iter().enumerate().map(|(i, w)| format!("{i}:{w}")).collect();
            let eol = if crlf { "\r\n" } else { "\n" };
            let content: String = lines.iter().map(|l| format!("{l}{eol}")).collect();
            e.put("f.txt", content.as_bytes()).await;
            e.read("f.txt").await;
            let start = start % lines.len();
            let end = (start + len).min(lines.len());
            let old_text = lines[start..end].join("\n");
            let new_text = new.join("\n");
            prop_assume!(old_text != new_text);
            let args = json!({"path": "f.txt", "old_text": old_text, "new_text": new_text});
            let p = e.preview(&EditTool, &args).await.unwrap();
            assert_eq!(p.bases, vec![(e.full("f.txt"), Some(blake3::hash(content.as_bytes())))]);
            let r = e.run_approved(&EditTool, args, &p).await;
            assert!(!r.is_error, "{}", r.output);
            assert_eq!(r.applied_diff.as_ref(), Some(&p.diff));
            assert!(p.diff.unified.starts_with("--- a/f.txt\n+++ b/f.txt\n@@ "), "{}", p.diff.unified);
            Ok(())
        })?;
    }
}

#[tokio::test]
async fn preview_writes_nothing() {
    let e = env(EditFormat::StrReplace);
    e.put("a.txt", b"one\ntwo\n").await;
    e.read("a.txt").await;
    let before = (
        e.slurp("a.txt").await,
        e.sb.metadata(&e.full("a.txt")).await.unwrap().mtime,
    );

    let args = json!({"path": "a.txt", "old_text": "two", "new_text": "TWO"});
    let p = e.preview(&EditTool, &args).await.unwrap();
    assert!(
        p.diff.unified.contains("-two\n+TWO\n"),
        "{}",
        p.diff.unified
    );
    let w = json!({"path": "new.txt", "content": "fresh\n"});
    e.preview(&WriteTool, &w).await.unwrap();
    let w2 = json!({"path": "a.txt", "content": "replaced\n"});
    e.preview(&WriteTool, &w2).await.unwrap();

    let after = (
        e.slurp("a.txt").await,
        e.sb.metadata(&e.full("a.txt")).await.unwrap().mtime,
    );
    assert_eq!(before, after);
    assert!(!e.dir.path().join("new.txt").exists());
    // Nothing the stale guard tracks moved either: the edit still applies without a re-read.
    let r = EditTool.run(args, &e.sb, &e.cx).await;
    assert!(!r.is_error, "{}", r.output);
    assert_eq!(e.slurp("a.txt").await, b"one\nTWO\n");
}

#[tokio::test]
async fn preview_for_each_edit_format() {
    // str_replace: the exact unified format.
    let e = env(EditFormat::StrReplace);
    e.put("src/lib.rs", b"a\nb\nc\n").await;
    e.read("src/lib.rs").await;
    let args = json!({"path": "src/lib.rs", "old_text": "b", "new_text": "B"});
    let p = e.preview(&EditTool, &args).await.unwrap();
    assert_eq!(
        p.diff,
        DiffPreview {
            unified: "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n".into(),
            lines: 7,
            truncated: false,
        }
    );
    let r = e.run_approved(&EditTool, args, &p).await;
    assert_eq!(r.applied_diff, Some(p.diff));

    // patch: update, add and delete in one call, concatenated in patch order.
    let e = env(EditFormat::Patch);
    e.put("u.txt", b"keep\nold\n").await;
    e.put("gone.txt", b"bye\n").await;
    e.read("u.txt").await;
    e.read("gone.txt").await;
    let patch = "*** Begin Patch\n*** Update File: u.txt\n@@\n keep\n-old\n+new\n\
                 *** Add File: added.txt\n+hello\n*** Delete File: gone.txt\n*** End Patch";
    let args = json!({ "patch": patch });
    let p = e.preview(&EditTool, &args).await.unwrap();
    let u = p.diff.unified.as_str();
    assert!(
        u.contains("--- a/u.txt\n+++ b/u.txt\n@@ -1,2 +1,2 @@\n keep\n-old\n+new\n"),
        "{u}"
    );
    assert!(
        u.contains("--- /dev/null\n+++ b/added.txt\n@@ -0,0 +1 @@\n+hello\n"),
        "{u}"
    );
    assert!(
        u.contains("--- a/gone.txt\n+++ /dev/null\n@@ -1 +0,0 @@\n-bye\n"),
        "{u}"
    );
    assert!(u.find("u.txt") < u.find("added.txt") && u.find("added.txt") < u.find("gone.txt"));
    assert_eq!(
        p.bases,
        vec![
            (e.full("u.txt"), Some(blake3::hash(b"keep\nold\n"))),
            (e.full("added.txt"), None),
            (e.full("gone.txt"), Some(blake3::hash(b"bye\n"))),
        ]
    );
    assert!(e.dir.path().join("gone.txt").exists() && !e.dir.path().join("added.txt").exists());
    let r = e.run_approved(&EditTool, args, &p).await;
    assert!(!r.is_error, "{}", r.output);
    assert_eq!(r.applied_diff, Some(p.diff));

    // hashline.
    let e = env(EditFormat::Hashline);
    e.put("h.txt", b"x\ny\n").await;
    let out = e.read("h.txt").await;
    let anchor = out
        .lines()
        .find(|l| l.starts_with("2:"))
        .and_then(|l| l.split('|').next())
        .unwrap()
        .to_string();
    let args = json!({"path": "h.txt", "ops": [{"op": "replace", "start": anchor, "end": anchor, "lines": ["Y"]}]});
    let p = e.preview(&EditTool, &args).await.unwrap();
    assert!(
        p.diff.unified.contains(" x\n-y\n+Y\n"),
        "{}",
        p.diff.unified
    );
    let r = e.run_approved(&EditTool, args, &p).await;
    assert!(!r.is_error, "{}", r.output);
    assert_eq!(r.applied_diff, Some(p.diff));
}

#[tokio::test]
async fn preview_fails_like_run() {
    let e = env(EditFormat::StrReplace);
    e.put("a.txt", b"one\n").await;
    let args = json!({"path": "a.txt", "old_text": "one", "new_text": "1"});
    let err = e.preview(&EditTool, &args).await.unwrap_err();
    assert_eq!(err.meta.error_kind, Some("not_read"));
    e.read("a.txt").await;
    let args = json!({"path": "a.txt", "old_text": "nowhere", "new_text": "1"});
    let err = e.preview(&EditTool, &args).await.unwrap_err();
    let run = EditTool.run(args, &e.sb, &e.cx).await;
    assert!(err.is_error);
    assert_eq!((err.output, err.meta), (run.output, run.meta));
    // Tools that change no files have no preview.
    assert!(
        ReadTool
            .preview(&json!({"path": "a.txt"}), &e.sb, &e.cx)
            .await
            .is_none()
    );
}

#[tokio::test]
async fn run_refuses_a_file_changed_after_approval() {
    let e = env(EditFormat::StrReplace);
    e.put("a.txt", b"one\n").await;
    e.read("a.txt").await;
    let args = json!({"path": "a.txt", "old_text": "one", "new_text": "1"});
    let p = e.preview(&EditTool, &args).await.unwrap();
    e.put("a.txt", b"one\nexternal\n").await;
    let r = e.run_approved(&EditTool, args, &p).await;
    assert!(r.is_error);
    assert_eq!(r.meta.error_kind, Some("changed_after_approval"));
    assert!(
        r.output.starts_with("stale: file changed after approval"),
        "{}",
        r.output
    );
    assert_eq!(e.slurp("a.txt").await, b"one\nexternal\n");

    // A file that appeared after a preview that would create it.
    let w = json!({"path": "n.txt", "content": "mine\n"});
    let p = e.preview(&WriteTool, &w).await.unwrap();
    e.put("n.txt", b"theirs\n").await;
    let r = e.run_approved(&WriteTool, w, &p).await;
    assert_eq!(r.meta.error_kind, Some("changed_after_approval"));
    assert_eq!(e.slurp("n.txt").await, b"theirs\n");
}

#[tokio::test]
async fn write_new_file_diff_uses_dev_null() {
    let e = env(EditFormat::StrReplace);
    let args = json!({"path": "docs/new.md", "content": "# Title\nbody\n"});
    let p = e.preview(&WriteTool, &args).await.unwrap();
    assert_eq!(
        p.diff.unified,
        "--- /dev/null\n+++ b/docs/new.md\n@@ -0,0 +1,2 @@\n+# Title\n+body\n"
    );
    assert_eq!(p.diff.lines, 5);
    assert_eq!(
        p.bases,
        vec![(
            e.dir.path().canonicalize().unwrap().join("docs/new.md"),
            None
        )]
    );
    let r = e.run_approved(&WriteTool, args, &p).await;
    assert!(!r.is_error, "{}", r.output);
    assert_eq!(r.applied_diff, Some(p.diff));

    // An unapproved write (auto-edit, yolo) reports its diff too.
    let r = WriteTool
        .run(
            json!({"path": "docs/new.md", "content": "# Title\n"}),
            &e.sb,
            &e.cx,
        )
        .await;
    let d = r.applied_diff.unwrap();
    assert!(
        d.unified
            .starts_with("--- a/docs/new.md\n+++ b/docs/new.md\n"),
        "{}",
        d.unified
    );
    assert!(d.unified.contains("-body\n"), "{}", d.unified);
}

#[test]
fn diff_capped_at_200_lines_with_marker() {
    let old = lines_of(250, "old");
    let new = lines_of(250, "new");
    let d = unified_diff(
        &[(
            PathBuf::from("big.txt"),
            Some(old.as_bytes()),
            Some(new.as_bytes()),
        )],
        PREVIEW_MAX_LINES,
        PREVIEW_MAX_BYTES,
    );
    assert!(d.lines >= 250, "{}", d.lines);
    assert_eq!(d.lines, 3 + 500);
    assert_eq!(d.unified.lines().count(), 201);
    marker_ok(&d);
    assert!(d.unified.ends_with("… 303 more lines (diff truncated)"));
    assert!(
        d.unified
            .starts_with("--- a/big.txt\n+++ b/big.txt\n@@ -1,250 +1,250 @@\n")
    );

    // At the cap exactly, nothing is cut.
    let d = unified_diff(
        &[(
            PathBuf::from("big.txt"),
            Some(old.as_bytes()),
            Some(new.as_bytes()),
        )],
        503,
        usize::MAX,
    );
    assert!(!d.truncated);
    assert_eq!(d.unified.lines().count(), 503);
}

#[test]
fn diff_capped_at_16kb() {
    let long = |c: char| {
        (0..40)
            .map(|_| format!("{}\n", c.to_string().repeat(1000)))
            .collect::<String>()
    };
    let (old, new) = (long('a'), long('b'));
    let d = unified_diff(
        &[(
            PathBuf::from("wide.txt"),
            Some(old.as_bytes()),
            Some(new.as_bytes()),
        )],
        PREVIEW_MAX_LINES,
        PREVIEW_MAX_BYTES,
    );
    assert_eq!(d.lines, 83);
    assert!(d.unified.len() <= PREVIEW_MAX_BYTES, "{}", d.unified.len());
    assert!(
        d.unified.len() > PREVIEW_MAX_BYTES - 1100,
        "{}",
        d.unified.len()
    );
    marker_ok(&d);
}

#[tokio::test]
async fn binary_content_shows_binary_differs() {
    let d = unified_diff(
        &[
            (
                PathBuf::from("img.png"),
                Some(b"\x89PNG\x00\xff".as_slice()),
                Some(b"\x89PNG\x00\xfe".as_slice()),
            ),
            (PathBuf::from("t.txt"), None, Some(b"text\n".as_slice())),
        ],
        PREVIEW_MAX_LINES,
        PREVIEW_MAX_BYTES,
    );
    assert_eq!(
        d.unified,
        "Binary file img.png differs\n--- /dev/null\n+++ b/t.txt\n@@ -0,0 +1 @@\n+text\n"
    );
    assert_eq!(d.lines, 5);

    // `write` over a non-UTF-8 file.
    let e = env(EditFormat::StrReplace);
    e.put("blob.bin", b"\xff\xfe\xfd").await;
    let p = e
        .preview(
            &WriteTool,
            &json!({"path": "blob.bin", "content": "now text\n"}),
        )
        .await
        .unwrap();
    assert_eq!(p.diff.unified, "Binary file blob.bin differs\n");
}

#[test]
fn identical_content_has_no_diff_and_a_missing_newline_is_marked() {
    let d = unified_diff(
        &[(
            PathBuf::from("same.txt"),
            Some(b"x\n".as_slice()),
            Some(b"x\n".as_slice()),
        )],
        PREVIEW_MAX_LINES,
        PREVIEW_MAX_BYTES,
    );
    assert_eq!((d.unified.as_str(), d.lines, d.truncated), ("", 0, false));
    let d = unified_diff(
        &[(
            PathBuf::from("n.txt"),
            Some(b"x".as_slice()),
            Some(b"y".as_slice()),
        )],
        PREVIEW_MAX_LINES,
        PREVIEW_MAX_BYTES,
    );
    assert!(
        d.unified
            .contains("-x\n\\ No newline at end of file\n+y\n\\ No newline at end of file\n"),
        "{}",
        d.unified
    );
}

#[tokio::test]
async fn a_large_file_is_diffed_off_the_async_workers_with_the_same_result() {
    let e = env(EditFormat::StrReplace);
    // Over 1 MB, so the diff runs on the blocking pool.
    let big = lines_of(100_000, "row");
    assert!(big.len() > 1024 * 1024);
    e.put("big.txt", big.as_bytes()).await;
    e.read("big.txt").await;
    let args = json!({"path": "big.txt", "old_text": "row line 50000\n", "new_text": "changed\n"});
    let p = e.preview(&EditTool, &args).await.unwrap();
    assert!(
        p.diff
            .unified
            .starts_with("--- a/big.txt\n+++ b/big.txt\n@@ -49997,7 +49997,7 @@\n"),
        "{}",
        p.diff.unified
    );
    assert!(p.diff.unified.contains("-row line 50000\n+changed\n"));
    let r = e.run_approved(&EditTool, args, &p).await;
    assert!(!r.is_error, "{}", r.output);
    assert_eq!(r.applied_diff, Some(p.diff));
}
