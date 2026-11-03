//! Diff-on-re-read (spec 7.2): a repeated read of the same range returns `unchanged`, a
//! small diff, or the full content.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use hx_provider::EditFormat;
use hx_sandbox::{LocalSandbox, Sandbox};
use hx_tools::{EditTool, LineRange, ReadTool, ReadTracker, Tool, ToolCx, ToolResult, WriteTool};
use serde_json::{Value, json};
use tempfile::TempDir;

struct Env {
    dir: TempDir,
    sb: LocalSandbox,
    reads: Arc<ReadTracker>,
}

fn env() -> Env {
    let dir = TempDir::new().unwrap();
    let sb = LocalSandbox::new(dir.path().to_path_buf()).unwrap();
    Env {
        dir,
        sb,
        reads: Arc::new(ReadTracker::default()),
    }
}

impl Env {
    fn cx(&self, turn: u32, format: EditFormat) -> ToolCx {
        ToolCx::new(format, self.dir.path().join(".spill"))
            .with_reads(Arc::clone(&self.reads))
            .with_turn(turn)
    }

    async fn put(&self, path: &str, content: &str) {
        self.sb
            .write_atomic(Path::new(path), content.as_bytes(), None)
            .await
            .unwrap();
    }

    async fn run_at(&self, tool: &dyn Tool, turn: u32, args: Value) -> ToolResult {
        tool.run(args, &self.sb, &self.cx(turn, EditFormat::StrReplace))
            .await
    }

    async fn read_at(&self, turn: u32, args: Value) -> String {
        let r = self.run_at(&ReadTool, turn, args).await;
        assert!(!r.is_error, "{}", r.output);
        r.output
    }
}

/// `line1\n...\nline10\n`, with the 1-based lines in `changed` altered.
fn ten_lines(changed: &[usize]) -> String {
    (1..=10)
        .map(|n| {
            if changed.contains(&n) {
                format!("CHANGED{n}\n")
            } else {
                format!("line{n}\n")
            }
        })
        .collect()
}

fn numbered(content: &str) -> String {
    content
        .lines()
        .enumerate()
        .map(|(i, l)| format!("{}\t{l}", i + 1))
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn identical_reread_returns_unchanged_since_turn() {
    let e = env();
    e.put("a.txt", &ten_lines(&[])).await;
    let first = e.read_at(3, json!({"path": "a.txt"})).await;
    assert_eq!(first, numbered(&ten_lines(&[])));
    let second = e.read_at(5, json!({"path": "a.txt"})).await;
    assert_eq!(second, "unchanged since turn 3");
    // Still anchored to the turn the content was last shown.
    let third = e.read_at(8, json!({"path": "a.txt"})).await;
    assert_eq!(third, "unchanged since turn 3");
}

#[tokio::test]
async fn small_change_returns_line_numbered_unified_diff() {
    let e = env();
    e.put("a.txt", &ten_lines(&[])).await;
    e.read_at(3, json!({"path": "a.txt"})).await;
    e.put("a.txt", &ten_lines(&[5])).await;
    let out = e.read_at(4, json!({"path": "a.txt"})).await;
    assert_eq!(
        out,
        "changed since turn 3:\n@@ -3,5 +3,5 @@\n line3\n line4\n-line5\n+CHANGED5\n line6\n line7"
    );
    // The diff is now the baseline: a further identical read is unchanged since turn 4.
    assert_eq!(
        e.read_at(6, json!({"path": "a.txt"})).await,
        "unchanged since turn 4"
    );
}

#[tokio::test]
async fn diff_hunks_use_new_file_line_numbers_for_ranged_reads() {
    let e = env();
    e.put("a.txt", &ten_lines(&[])).await;
    let args = json!({"path": "a.txt", "start_line": 4, "end_line": 9});
    e.read_at(1, args.clone()).await;
    e.put("a.txt", &ten_lines(&[8])).await;
    let out = e.read_at(2, args).await;
    assert_eq!(
        out,
        "changed since turn 1:\n@@ -6,4 +6,4 @@\n line6\n line7\n-line8\n+CHANGED8\n line9"
    );
}

#[tokio::test]
async fn change_of_half_or_more_returns_full() {
    let e = env();
    e.put("a.txt", &ten_lines(&[])).await;
    e.read_at(3, json!({"path": "a.txt"})).await;
    let after = ten_lines(&[1, 2, 3, 4, 5]);
    e.put("a.txt", &after).await;
    let out = e.read_at(4, json!({"path": "a.txt"})).await;
    assert_eq!(
        out,
        format!("changed since turn 3 (full content):\n{}", numbered(&after))
    );
    // 4 of 10 is still a diff, against the full read just made.
    let four = after
        .replace("line6", "X6")
        .replace("line7", "X7")
        .replace("line8", "X8")
        .replace("line9", "X9");
    e.put("a.txt", &four).await;
    let out = e.read_at(5, json!({"path": "a.txt"})).await;
    assert!(out.starts_with("changed since turn 4:\n@@"), "{out}");
}

#[tokio::test]
async fn full_true_forces_full_content() {
    let e = env();
    e.put("a.txt", &ten_lines(&[])).await;
    e.read_at(3, json!({"path": "a.txt"})).await;
    let out = e.read_at(4, json!({"path": "a.txt", "full": true})).await;
    assert_eq!(out, numbered(&ten_lines(&[])));
    // Also when the file changed a little.
    e.put("a.txt", &ten_lines(&[5])).await;
    let out = e.read_at(5, json!({"path": "a.txt", "full": true})).await;
    assert_eq!(out, numbered(&ten_lines(&[5])));
    // A forced full read is the new baseline.
    assert_eq!(
        e.read_at(6, json!({"path": "a.txt"})).await,
        "unchanged since turn 5"
    );
}

#[tokio::test]
async fn different_range_is_not_a_reread() {
    let e = env();
    e.put("a.txt", &ten_lines(&[])).await;
    e.read_at(1, json!({"path": "a.txt", "start_line": 1, "end_line": 5}))
        .await;
    let out = e
        .read_at(2, json!({"path": "a.txt", "start_line": 1, "end_line": 6}))
        .await;
    assert_eq!(
        out,
        numbered(&ten_lines(&[]))
            .lines()
            .take(6)
            .collect::<Vec<_>>()
            .join("\n")
    );
    let whole = e.read_at(3, json!({"path": "a.txt"})).await;
    assert_eq!(whole, numbered(&ten_lines(&[])));
}

#[tokio::test]
async fn change_outside_the_range_is_unchanged() {
    let e = env();
    e.put("a.txt", &ten_lines(&[])).await;
    let args = json!({"path": "a.txt", "start_line": 1, "end_line": 3});
    e.read_at(1, args.clone()).await;
    e.put("a.txt", &ten_lines(&[9])).await;
    assert_eq!(e.read_at(2, args).await, "unchanged since turn 1");
}

#[tokio::test]
async fn tracker_keeps_contents_on_disk_not_in_memory() {
    let e = env();
    let content = ten_lines(&[]);
    e.put("a.txt", &content).await;
    e.read_at(2, json!({"path": "a.txt"})).await;
    let full = e.sb.resolve(Path::new("a.txt")).unwrap();
    let prev = e.reads.previous(&full, LineRange::ALL).expect("recorded");
    assert_eq!(prev.turn, 2);
    // The sandbox spills content-addressed files; the tracker holds only the reference.
    let on_disk = e.sb.read(&prev.content_path).await.unwrap();
    assert_eq!(String::from_utf8(on_disk).unwrap(), content.trim_end());
    assert!(prev.content_path.starts_with(".hx/tmp/spill"));
    let debug = format!("{:?}", e.reads);
    assert!(!debug.contains("line5"), "{debug}");
    assert!(
        e.reads
            .previous(
                &full,
                LineRange {
                    start: Some(1),
                    end: Some(3)
                }
            )
            .is_none()
    );
}

#[tokio::test]
async fn edit_updates_tracker_so_reread_after_own_edit_is_diffed_against_post_edit() {
    let e = env();
    e.put("a.txt", &ten_lines(&[])).await;
    e.read_at(1, json!({"path": "a.txt"})).await;
    let r = e
        .run_at(
            &EditTool,
            2,
            json!({"path": "a.txt", "old_text": "line5", "new_text": "EDITED5"}),
        )
        .await;
    assert!(!r.is_error, "{}", r.output);
    // Re-read right after the edit: nothing changed since the post-edit state.
    assert_eq!(
        e.read_at(3, json!({"path": "a.txt"})).await,
        "unchanged since turn 2"
    );
    // Someone else changes another line: the diff shows only that line.
    let mut content = ten_lines(&[]).replace("line5", "EDITED5");
    content = content.replace("line9", "OTHER9");
    e.put("a.txt", &content).await;
    let out = e.read_at(4, json!({"path": "a.txt"})).await;
    assert_eq!(
        out,
        "changed since turn 2:\n@@ -7,4 +7,4 @@\n line7\n line8\n-line9\n+OTHER9\n line10"
    );
}

#[tokio::test]
async fn write_updates_tracker_for_reread() {
    let e = env();
    let r = e
        .run_at(
            &WriteTool,
            4,
            json!({"path": "w.txt", "content": ten_lines(&[])}),
        )
        .await;
    assert!(!r.is_error, "{}", r.output);
    assert_eq!(
        e.read_at(5, json!({"path": "w.txt"})).await,
        "unchanged since turn 4"
    );
}

#[tokio::test]
async fn unchanged_reread_keeps_the_stale_read_guard_fresh() {
    let e = env();
    e.put("a.txt", &ten_lines(&[])).await;
    e.read_at(1, json!({"path": "a.txt"})).await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    // Same content, new mtime.
    e.put("a.txt", &ten_lines(&[])).await;
    assert_eq!(
        e.read_at(2, json!({"path": "a.txt"})).await,
        "unchanged since turn 1"
    );
    let r = e
        .run_at(
            &EditTool,
            3,
            json!({"path": "a.txt", "old_text": "line5", "new_text": "EDITED5"}),
        )
        .await;
    assert!(!r.is_error, "{}", r.output);
}

#[tokio::test]
async fn batched_targets_each_participate() {
    let e = env();
    e.put("a.txt", "one\ntwo\n").await;
    e.put("b.txt", "x\ny\n").await;
    let args =
        json!({"targets": [{"path": "a.txt"}, {"path": "b.txt", "start_line": 1, "end_line": 1}]});
    e.read_at(1, args.clone()).await;
    e.put("b.txt", "z\ny\n").await;
    let out = e.read_at(2, args).await;
    assert!(out.contains("== a.txt ==\nunchanged since turn 1"), "{out}");
    assert!(
        out.contains("== b.txt:1-1 ==\nchanged since turn 1 (full content):\n1\tz"),
        "{out}"
    );
    // A single read of the same file and range shares the baseline.
    assert_eq!(
        e.read_at(3, json!({"path": "a.txt"})).await,
        "unchanged since turn 1"
    );
}

#[tokio::test]
async fn missing_spilled_copy_falls_back_to_full_content() {
    let e = env();
    e.put("a.txt", &ten_lines(&[])).await;
    e.read_at(1, json!({"path": "a.txt"})).await;
    let full = e.sb.resolve(Path::new("a.txt")).unwrap();
    let prev = e.reads.previous(&full, LineRange::ALL).unwrap();
    e.sb.remove_file(&prev.content_path).await.unwrap();
    e.put("a.txt", &ten_lines(&[5])).await;
    // The read still succeeds, with everything the diff would have implied.
    let out = e.read_at(2, json!({"path": "a.txt"})).await;
    assert_eq!(
        out,
        format!(
            "changed since turn 1 (full content):\n{}",
            numbered(&ten_lines(&[5]))
        )
    );
}

#[tokio::test]
async fn hashline_format_returns_full_anchored_content_when_changed() {
    let e = env();
    e.put("a.txt", &ten_lines(&[])).await;
    let read = |turn| {
        let cx = e.cx(turn, EditFormat::Hashline);
        let sb = &e.sb;
        async move {
            let r = ReadTool.run(json!({"path": "a.txt"}), sb, &cx).await;
            assert!(!r.is_error, "{}", r.output);
            r.output
        }
    };
    read(1).await;
    assert_eq!(read(2).await, "unchanged since turn 1");
    e.put("a.txt", &ten_lines(&[5])).await;
    let out = read(3).await;
    assert!(
        out.starts_with("changed since turn 1 (full content):\n1:"),
        "{out}"
    );
    assert!(out.contains("|CHANGED5"), "{out}");
    assert!(!out.contains("@@"), "{out}");
}

#[tokio::test]
async fn hashline_edit_does_not_make_the_next_read_unchanged() {
    let e = env();
    e.put("a.txt", "one\ntwo\nthree\n").await;
    let cx = |turn| e.cx(turn, EditFormat::Hashline);
    let r = ReadTool.run(json!({"path": "a.txt"}), &e.sb, &cx(1)).await;
    let anchor = r
        .output
        .lines()
        .nth(1)
        .and_then(|l| l.split('|').next())
        .unwrap()
        .to_string();
    let r = EditTool
        .run(
            json!({"path": "a.txt", "ops": [{"op": "replace", "start": anchor, "end": anchor, "lines": ["TWO", "2b"]}]}),
            &e.sb,
            &cx(2),
        )
        .await;
    assert!(!r.is_error, "{}", r.output);
    let r = ReadTool.run(json!({"path": "a.txt"}), &e.sb, &cx(3)).await;
    assert!(
        r.output
            .starts_with("changed since turn 1 (full content):\n1:"),
        "{}",
        r.output
    );
    assert!(r.output.contains("|2b"), "{}", r.output);
}

// ---- truncated output never becomes a baseline ----

#[tokio::test]
async fn output_cut_by_the_byte_cap_is_returned_again_not_unchanged() {
    let e = env();
    let huge: String = (1..=3)
        .map(|n| format!("{n}{}\n", "x".repeat(20_000)))
        .collect();
    e.put("huge.txt", &huge).await;
    let first = e.read_at(1, json!({"path": "huge.txt"})).await;
    assert!(first.contains("truncated"), "{}", first.len());
    let second = e.read_at(2, json!({"path": "huge.txt"})).await;
    assert!(!second.contains("unchanged"), "{}", &second[..80]);
    assert!(second.starts_with("1\t1xxx"), "{}", &second[..80]);
    // Narrowing to a range that fits is a different read and works as usual.
    let args = json!({"path": "huge.txt", "start_line": 1, "end_line": 1});
    e.read_at(3, args.clone()).await;
    assert_eq!(e.read_at(4, args).await, "unchanged since turn 3");
}

#[tokio::test]
async fn output_cut_by_the_line_window_is_returned_again_not_unchanged() {
    let e = env();
    let long: String = (1..=2500).map(|n| format!("row{n}\n")).collect();
    e.put("long.txt", &long).await;
    let first = e.read_at(1, json!({"path": "long.txt"})).await;
    assert!(first.ends_with("[500 more lines; use start_line=2001 to continue]"));
    let second = e.read_at(2, json!({"path": "long.txt"})).await;
    assert!(second.starts_with("1\trow1\n"), "{}", &second[..40]);
    assert!(second.ends_with("[500 more lines; use start_line=2001 to continue]"));
}

#[tokio::test]
async fn truncated_batch_target_is_returned_again_but_whole_ones_are_unchanged() {
    let e = env();
    let big: String = (0..520)
        .map(|i| format!("big {i:04} {}\n", "x".repeat(30)))
        .collect();
    e.put("big.txt", &big).await;
    e.put("big2.txt", &big).await;
    e.put("small.txt", "tiny\n").await;
    let args =
        json!({"targets": [{"path": "big.txt"}, {"path": "big2.txt"}, {"path": "small.txt"}]});
    let first = e.read_at(1, args.clone()).await;
    assert_eq!(first.matches("truncated").count(), 2, "{}", first.len());
    let second = e.read_at(2, args).await;
    assert!(
        second.contains("== small.txt ==\nunchanged since turn 1"),
        "{second}"
    );
    assert!(second.contains("== big.txt ==\n1\tbig 0000"), "{second}");
    assert!(second.contains("== big2.txt ==\n1\tbig 0000"), "{second}");
}

#[tokio::test]
async fn write_of_a_file_beyond_the_window_sets_no_baseline() {
    let e = env();
    let long: String = (1..=2500).map(|n| format!("row{n}\n")).collect();
    let r = e
        .run_at(&WriteTool, 4, json!({"path": "w.txt", "content": long}))
        .await;
    assert!(!r.is_error, "{}", r.output);
    let out = e.read_at(5, json!({"path": "w.txt"})).await;
    assert!(out.starts_with("1\trow1\n"), "{}", &out[..40]);
}
