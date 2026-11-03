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
            assert_eq!(r.applied_diff.as_deref(), Some(&p.diff));
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
    assert_eq!(r.applied_diff.as_deref(), Some(&p.diff));

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
    assert_eq!(r.applied_diff.as_deref(), Some(&p.diff));

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
    assert_eq!(r.applied_diff.as_deref(), Some(&p.diff));
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
    assert_eq!(r.applied_diff.as_deref(), Some(&p.diff));

    // An unapproved write (auto-edit, yolo) reports its diff too.
    let r = WriteTool
        .run(
            json!({"path": "docs/new.md", "content": "# Title\n"}),
            &e.sb,
            &e.cx,
        )
        .await;
    let d = *r.applied_diff.unwrap();
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
async fn a_large_file_previews_the_diff_it_applies() {
    let e = env(EditFormat::StrReplace);
    let big = lines_of(100_000, "row");
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
    assert_eq!(r.applied_diff.as_deref(), Some(&p.diff));
}

/// An approval covers the files the preview read. A symlink that now points elsewhere
/// names another file, so the approved diff would not be the applied one.
#[tokio::test]
async fn an_approved_call_through_a_retargeted_symlink_is_refused() {
    let e = env(EditFormat::StrReplace);
    e.put("t1.txt", b"one\n").await;
    e.put("t2.txt", b"one\n").await;
    let link = e.dir.path().join("link.txt");
    std::os::unix::fs::symlink("t1.txt", &link).unwrap();
    // Fixture setup: the sandbox follows links, and the test must replace the link itself.
    #[allow(clippy::disallowed_methods)]
    let retarget = |to: &str| {
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(to, &link).unwrap();
    };

    let w = json!({"path": "link.txt", "content": "mine\n"});
    let p = e.preview(&WriteTool, &w).await.unwrap();
    retarget("t2.txt");
    let r = e.run_approved(&WriteTool, w, &p).await;
    assert_eq!(
        r.meta.error_kind,
        Some("changed_after_approval"),
        "{}",
        r.output
    );
    assert_eq!(e.slurp("t1.txt").await, b"one\n");
    assert_eq!(e.slurp("t2.txt").await, b"one\n");

    // An edit, with both targets read so the stale guard alone would let it through.
    retarget("t1.txt");
    e.read("t1.txt").await;
    e.read("t2.txt").await;
    let args = json!({"path": "link.txt", "old_text": "one", "new_text": "ONE"});
    let p = e.preview(&EditTool, &args).await.unwrap();
    retarget("t2.txt");
    let r = e.run_approved(&EditTool, args, &p).await;
    assert_eq!(
        r.meta.error_kind,
        Some("changed_after_approval"),
        "{}",
        r.output
    );
    assert_eq!(e.slurp("t2.txt").await, b"one\n");
}

#[tokio::test]
async fn an_approved_patch_with_one_changed_file_writes_nothing() {
    let e = env(EditFormat::Patch);
    e.put("a.txt", b"a\n").await;
    e.put("b.txt", b"b\n").await;
    e.read("a.txt").await;
    e.read("b.txt").await;
    let patch = "*** Begin Patch\n*** Update File: a.txt\n@@\n-a\n+A\n\
                 *** Update File: b.txt\n@@\n-b\n+B\n*** Add File: c.txt\n+c\n*** End Patch";
    let args = json!({ "patch": patch });
    let p = e.preview(&EditTool, &args).await.unwrap();
    e.put("b.txt", b"b\nexternal\n").await;
    let r = e.run_approved(&EditTool, args, &p).await;
    assert_eq!(
        r.meta.error_kind,
        Some("changed_after_approval"),
        "{}",
        r.output
    );
    assert!(r.output.contains("b.txt"), "{}", r.output);
    assert_eq!(e.slurp("a.txt").await, b"a\n");
    assert_eq!(e.slurp("b.txt").await, b"b\nexternal\n");
    assert!(!e.dir.path().join("c.txt").exists());
}

#[tokio::test]
async fn an_approved_write_over_a_changed_file_is_refused() {
    let e = env(EditFormat::StrReplace);
    e.put("a.txt", b"old\n").await;
    let w = json!({"path": "a.txt", "content": "mine\n"});
    let p = e.preview(&WriteTool, &w).await.unwrap();
    assert!(
        p.diff.unified.contains("-old\n+mine\n"),
        "{}",
        p.diff.unified
    );
    e.put("a.txt", b"theirs\n").await;
    let r = e.run_approved(&WriteTool, w, &p).await;
    assert_eq!(r.meta.error_kind, Some("changed_after_approval"));
    assert!(
        r.output.starts_with("stale: file changed after approval"),
        "{}",
        r.output
    );
    assert_eq!(e.slurp("a.txt").await, b"theirs\n");
}

#[test]
fn creating_or_deleting_an_empty_file_shows_the_headers() {
    let d = unified_diff(
        &[
            (PathBuf::from("new.txt"), None, Some(b"".as_slice())),
            (PathBuf::from("gone.txt"), Some(b"".as_slice()), None),
        ],
        PREVIEW_MAX_LINES,
        PREVIEW_MAX_BYTES,
    );
    assert_eq!(
        d.unified,
        "--- /dev/null\n+++ b/new.txt\n--- a/gone.txt\n+++ /dev/null\n"
    );
    assert_eq!(d.lines, 4);
}

/// A diff that takes too long (Myers is quadratic on a full rewrite) stops at the time
/// budget and shows a summary with the line counts instead.
#[test]
fn a_slow_diff_falls_back_to_a_summary() {
    let old = lines_of(60_000, "old");
    let new = lines_of(60_000, "new");
    let started = std::time::Instant::now();
    let d = unified_diff(
        &[
            (
                PathBuf::from("big.txt"),
                Some(old.as_bytes()),
                Some(new.as_bytes()),
            ),
            (
                PathBuf::from("small.txt"),
                Some(b"a\n".as_slice()),
                Some(b"b\n".as_slice()),
            ),
        ],
        PREVIEW_MAX_LINES,
        PREVIEW_MAX_BYTES,
    );
    let elapsed = started.elapsed();
    assert!(elapsed < std::time::Duration::from_secs(5), "{elapsed:?}");
    assert!(!d.truncated, "{d:?}");
    assert!(
        d.unified.starts_with(
            "File big.txt: diff too large or slow to show, summary only: +60000 -60000 lines\n"
        ),
        "{}",
        &d.unified[..d.unified.len().min(300)]
    );
}

/// The diff of a large change runs on the blocking pool: a current-thread runtime keeps
/// running its other tasks meanwhile.
#[test]
fn a_large_diff_does_not_block_the_async_workers() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::{Duration, Instant};

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let e = env(EditFormat::StrReplace);
        // About 120 KB of content, fully rewritten: slow to diff.
        let old = lines_of(5_000, "old");
        let new = lines_of(5_000, "new");
        e.put("a.txt", old.as_bytes()).await;
        let ticks = Arc::new(AtomicU32::new(0));
        let counter = Arc::clone(&ticks);
        let ticker = tokio::spawn(async move {
            let mut every = tokio::time::interval(Duration::from_millis(5));
            loop {
                every.tick().await;
                counter.fetch_add(1, Ordering::Relaxed);
            }
        });
        tokio::task::yield_now().await;
        let started = Instant::now();
        let before = ticks.load(Ordering::Relaxed);
        let p = e
            .preview(&WriteTool, &json!({"path": "a.txt", "content": new}))
            .await
            .unwrap();
        let elapsed = started.elapsed();
        let during = ticks.load(Ordering::Relaxed) - before;
        ticker.abort();
        assert!(p.diff.lines > 0);
        // An inline diff would let no tick through while it runs.
        let expected = (elapsed.as_millis() / 5) as u32;
        assert!(
            elapsed < Duration::from_millis(50) || during * 4 >= expected,
            "{during} ticks in {elapsed:?}"
        );
    });
}
