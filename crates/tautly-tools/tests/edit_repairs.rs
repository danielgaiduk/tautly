//! Safety of tolerant repairs (checkpoint 2: C1, C2, I3, I9): a repair never writes copied
//! `read` artifacts or drops lines the model left out, never leaves a whole-file re-read
//! baseline behind, and only matches content the model has seen as it is now.

use std::path::Path;
use std::sync::Arc;

use proptest::prelude::*;
use serde_json::{Value, json};
use tautly_provider::EditFormat;
use tautly_sandbox::{LocalSandbox, Sandbox};
use tautly_tools::edit::str_replace::{MatchError, apply_staged};
use tautly_tools::edit::tolerant::{RepairStage, locate};
use tautly_tools::{EditTool, ReadTool, Tool, ToolCx, ToolResult};
use tempfile::TempDir;

struct Env {
    dir: TempDir,
    sb: LocalSandbox,
    reads: Arc<tautly_tools::ReadTracker>,
}

fn env() -> Env {
    let dir = TempDir::new().unwrap();
    let sb = LocalSandbox::new(dir.path().to_path_buf()).unwrap();
    Env {
        dir,
        sb,
        reads: Arc::default(),
    }
}

impl Env {
    fn cx(&self, turn: u32, format: EditFormat) -> ToolCx {
        ToolCx::new(format, self.dir.path().join(".spill"))
            .with_reads(Arc::clone(&self.reads))
            .with_turn(turn)
    }

    async fn put(&self, path: &str, text: &str) {
        self.sb
            .write_atomic(Path::new(path), text.as_bytes(), None)
            .await
            .unwrap();
    }

    async fn slurp(&self, path: &str) -> String {
        String::from_utf8(self.sb.read(Path::new(path)).await.unwrap()).unwrap()
    }

    async fn read(&self, turn: u32, args: Value) -> ToolResult {
        self.read_as(turn, EditFormat::StrReplace, args).await
    }

    async fn read_as(&self, turn: u32, format: EditFormat, args: Value) -> ToolResult {
        let r = ReadTool.run(args, &self.sb, &self.cx(turn, format)).await;
        assert!(!r.is_error, "{}", r.output);
        r
    }

    async fn edit(&self, turn: u32, path: &str, old: &str, new: &str) -> ToolResult {
        EditTool
            .run(
                json!({"path": path, "old_text": old, "new_text": new}),
                &self.sb,
                &self.cx(turn, EditFormat::StrReplace),
            )
            .await
    }

    async fn patch(&self, turn: u32, patch: &str) -> ToolResult {
        EditTool
            .run(
                json!({"patch": patch}),
                &self.sb,
                &self.cx(turn, EditFormat::Patch),
            )
            .await
    }
}

// ------------------------------------------------------------------ C1: read artifacts

#[tokio::test]
async fn partly_numbered_new_text_is_rejected_and_nothing_is_written() {
    // The reviewer's `artifact_mixed` probe.
    let e = env();
    let src = "fn main() {\n    let a = 1;\n    let b = 2;\n}\n";
    e.put("a.rs", src).await;
    e.read(1, json!({"path": "a.rs"})).await;
    let old = "2\t    let a = 1;\n3\t    let b = 2;";
    let new = "2\t    let a = 1;\n    let c = 3;\n3\t    let b = 2;";
    let r = e.edit(1, "a.rs", old, new).await;
    assert!(r.is_error, "{}", r.output);
    assert!(r.output.contains("line numbers"), "{}", r.output);
    assert_eq!(e.slurp("a.rs").await, src);
}

#[tokio::test]
async fn partly_anchored_new_text_is_rejected() {
    let e = env();
    let src = "fn main() {\n    let a = 1;\n    let b = 2;\n}\n";
    e.put("a.rs", src).await;
    e.read(1, json!({"path": "a.rs"})).await;
    let old = "2:ab12|    let a = 1;\n3:cd34|    let b = 2;";
    let new = "2:ab12|    let a = 1;\n    let c = 3;\n3:cd34|    let b = 2;";
    let r = e.edit(1, "a.rs", old, new).await;
    assert!(r.is_error, "{}", r.output);
    assert_eq!(e.slurp("a.rs").await, src);
}

#[test]
fn mixed_artifacts_in_new_text_are_a_match_error() {
    let text = "a\nb\nc\n";
    assert_eq!(
        apply_staged(text, "1\ta\n2\tb", "1\ta\nx\n2\tb"),
        Err(MatchError::MixedArtifacts)
    );
    // Fully numbered new_text is still stripped like old_text.
    let (out, stage) = apply_staged(text, "1\ta\n2\tb", "1\ta\n2\tx").unwrap();
    assert_eq!(stage, RepairStage::Normalized);
    assert_eq!(out, "a\nx\nc\n");
    // An exact match leaves new_text alone: it may legitimately start with numbers.
    let (out, stage) = apply_staged("id\tname\n", "id\tname", "id\tname\n1\tbob").unwrap();
    assert_eq!(stage, RepairStage::Exact);
    assert_eq!(out, "id\tname\n1\tbob\n");
}

/// Whether `line` looks like a copied `read` artifact.
fn artifact_line(line: &str) -> bool {
    let digits = line.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return false;
    }
    let rest = &line[digits..];
    rest.starts_with('\t')
        || (rest.len() >= 6
            && rest.starts_with(':')
            && rest.as_bytes()[5] == b'|'
            && rest[1..5].bytes().all(|b| b.is_ascii_hexdigit()))
}

proptest! {
    /// A model that copies numbered lines into old_text and into only some lines of
    /// new_text never gets those numbers written into the file.
    #[test]
    fn applied_edits_never_introduce_artifact_lines(
        words in prop::collection::vec("[a-z]{1,8}( [a-z]{1,8})?", 3..12),
        start in 0usize..3,
        len in 1usize..4,
        mask in prop::collection::vec(any::<bool>(), 1..8),
        hashline in any::<bool>(),
    ) {
        let text = words.iter().map(|w| format!("{w}\n")).collect::<String>();
        let lines: Vec<&str> = text.lines().collect();
        let start = start.min(lines.len() - 1);
        let end = (start + len).min(lines.len());
        let tag = |n: usize, l: &str| {
            if hashline { format!("{n}:ab12|{l}") } else { format!("{n}\t{l}") }
        };
        let old = (start..end).map(|i| tag(i + 1, lines[i])).collect::<Vec<_>>().join("\n");
        let new = mask
            .iter()
            .enumerate()
            .map(|(k, numbered)| {
                let l = format!("new{k}");
                if *numbered { tag(start + 1 + k, &l) } else { l }
            })
            .collect::<Vec<_>>()
            .join("\n");
        if let Ok((out, _)) = apply_staged(&text, &old, &new) {
            for line in out.lines() {
                prop_assert!(!artifact_line(line), "{line:?} written from new={new:?}");
            }
        }
    }
}

// ------------------------------------------------------------------ C2: anchor stage

fn big_block() -> Vec<String> {
    let mut lines = vec!["fn big() {".to_string()];
    lines.extend((0..20).map(|i| format!("    step_{i}();")));
    lines.push("}".to_string());
    lines
}

#[tokio::test]
async fn anchor_match_never_drops_lines_the_model_left_out() {
    // The reviewer's `anchor_drop` probe: old_text omits step_7 and step_13.
    let e = env();
    let lines = big_block();
    let src = lines.join("\n") + "\n";
    e.put("a.rs", &src).await;
    e.read(1, json!({"path": "a.rs"})).await;
    let old: Vec<&str> = lines
        .iter()
        .map(String::as_str)
        .filter(|l| !l.contains("step_7(") && !l.contains("step_13("))
        .collect();
    let old = old.join("\n");
    let new = old.replace("step_0();", "step_zero();");
    let r = e.edit(2, "a.rs", &old, &new).await;
    assert!(r.is_error, "{}", r.output);
    assert!(r.output.contains("not found"), "{}", r.output);
    assert_eq!(e.slurp("a.rs").await, src);
}

#[test]
fn anchor_stage_needs_equal_interior_line_counts() {
    let file = big_block().join("\n") + "\n";
    // One file line missing from the model's interior: no anchor match.
    let short: Vec<String> = big_block()
        .into_iter()
        .filter(|l| !l.contains("step_7("))
        .collect();
    assert!(matches!(
        locate(&file, &short.join("\n")),
        Err(MatchError::NotFound { .. })
    ));
    // One extra line in the model's interior: no anchor match either.
    let mut long = big_block();
    long.insert(5, "    invented();".into());
    assert!(matches!(
        locate(&file, &long.join("\n")),
        Err(MatchError::NotFound { .. })
    ));
    // Same count, one line changed: still an anchor match.
    let mut changed = big_block();
    changed[7] = "    step_six_and_a_half();".into();
    let loc = locate(&file, &changed.join("\n")).unwrap();
    assert_eq!(loc.stage, RepairStage::Anchor);
}

// ------------------------------------------------------------------ I3: no baseline after a repair

#[tokio::test]
async fn reread_after_an_indentation_repair_returns_the_content() {
    // The reviewer's `reindent_reread` probe.
    let e = env();
    let src = "def f():\n    if x:\n        a()\n        b()\n    return 1\n";
    e.put("a.py", src).await;
    e.read(1, json!({"path": "a.py"})).await;
    let r = e
        .edit(
            1,
            "a.py",
            "if x:\n    a()\n    b()",
            "if x:\n    a()\n    c()\n    b()",
        )
        .await;
    assert!(!r.is_error, "{}", r.output);
    assert!(r.output.contains("indentation-relative"), "{}", r.output);
    let again = e.read(2, json!({"path": "a.py"})).await;
    assert!(!again.output.contains("unchanged"), "{}", again.output);
    assert!(again.output.contains("        c()"), "{}", again.output);
}

#[tokio::test]
async fn reread_after_a_normalized_repair_returns_the_content() {
    let e = env();
    e.put("a.txt", "say \"hi\"\nnext\n").await;
    e.read(1, json!({"path": "a.txt"})).await;
    let r = e
        .edit(
            1,
            "a.txt",
            "say \u{201C}hi\u{201D}",
            "say \u{201C}yo\u{201D}",
        )
        .await;
    assert!(r.output.contains("normalized"), "{}", r.output);
    let again = e.read(2, json!({"path": "a.txt"})).await;
    assert!(!again.output.contains("unchanged"), "{}", again.output);
}

#[tokio::test]
async fn reread_after_an_exact_edit_still_says_unchanged() {
    let e = env();
    e.put("a.txt", "one\ntwo\n").await;
    e.read(1, json!({"path": "a.txt"})).await;
    let r = e.edit(1, "a.txt", "two", "three").await;
    assert!(!r.is_error, "{}", r.output);
    let again = e.read(2, json!({"path": "a.txt"})).await;
    assert!(again.output.contains("unchanged"), "{}", again.output);
}

#[tokio::test]
async fn reread_after_a_repaired_patch_returns_the_content() {
    let e = env();
    e.put("a.rs", "impl X {\n    fn f() {\n        old();\n    }\n}\n")
        .await;
    e.read_as(1, EditFormat::Patch, json!({"path": "a.rs"}))
        .await;
    let p = "*** Begin Patch\n*** Update File: a.rs\n@@\n fn f() {\n-    old();\n+    new();\n }\n*** End Patch";
    let r = e.patch(1, p).await;
    assert!(!r.is_error, "{}", r.output);
    assert!(r.output.contains("applied via"), "{}", r.output);
    let again = e
        .read_as(2, EditFormat::Patch, json!({"path": "a.rs"}))
        .await;
    assert!(!again.output.contains("unchanged"), "{}", again.output);
}

// ------------------------------------------------------------------ I9: repairs only on seen content

/// 60 lines; lines 40-42 form an indented block the model remembers.
fn sixty() -> String {
    (1..=60)
        .map(|i| match i {
            40 => "    if ready {\n".to_string(),
            41 => "        go();\n".to_string(),
            42 => "    }\n".to_string(),
            i => format!("line {i}\n"),
        })
        .collect()
}

/// A formatter re-indents lines 40-42 behind the model's back.
fn reformatted() -> String {
    sixty().replace(
        "    if ready {\n        go();\n    }\n",
        "  if ready {\n    go();\n  }\n",
    )
}

const REMEMBERED: &str = "    if ready {\n        go();\n    }";
const REPLACEMENT: &str = "    if ready {\n        stop();\n    }";

#[tokio::test]
async fn a_repair_over_unseen_changes_is_stale() {
    let e = env();
    e.put("a.rs", &sixty()).await;
    e.read(1, json!({"path": "a.rs"})).await;
    e.put("a.rs", &reformatted()).await;
    // A partial read elsewhere refreshes the stale-read guard.
    e.read(2, json!({"path": "a.rs", "start_line": 1, "end_line": 10}))
        .await;
    let r = e.edit(3, "a.rs", REMEMBERED, REPLACEMENT).await;
    assert!(r.is_error, "{}", r.output);
    assert!(r.output.contains("lines 40-42"), "{}", r.output);
    assert_eq!(e.slurp("a.rs").await, reformatted());
}

#[tokio::test]
async fn a_repair_inside_a_fresh_range_read_is_applied() {
    let e = env();
    e.put("a.rs", &sixty()).await;
    e.read(1, json!({"path": "a.rs"})).await;
    e.put("a.rs", &reformatted()).await;
    e.read(2, json!({"path": "a.rs", "start_line": 38, "end_line": 45}))
        .await;
    let r = e.edit(3, "a.rs", REMEMBERED, REPLACEMENT).await;
    assert!(!r.is_error, "{}", r.output);
    assert!(
        e.slurp("a.rs").await.contains("    stop();"),
        "{}",
        e.slurp("a.rs").await
    );
}

#[tokio::test]
async fn a_repair_after_a_whole_file_read_at_the_current_hash_is_applied() {
    let e = env();
    e.put("a.rs", &reformatted()).await;
    e.read(1, json!({"path": "a.rs"})).await;
    let r = e.edit(1, "a.rs", REMEMBERED, REPLACEMENT).await;
    assert!(!r.is_error, "{}", r.output);
}

#[tokio::test]
async fn an_exact_edit_after_a_partial_read_is_still_allowed() {
    let e = env();
    e.put("a.rs", &sixty()).await;
    e.read(1, json!({"path": "a.rs"})).await;
    e.put("a.rs", &reformatted()).await;
    e.read(2, json!({"path": "a.rs", "start_line": 1, "end_line": 10}))
        .await;
    let r = e.edit(3, "a.rs", "line 55", "line fifty-five").await;
    assert!(!r.is_error, "{}", r.output);
}

#[tokio::test]
async fn a_patch_hunk_repair_over_unseen_changes_is_stale() {
    let e = env();
    e.put("a.rs", &sixty()).await;
    e.read_as(1, EditFormat::Patch, json!({"path": "a.rs"}))
        .await;
    e.put("a.rs", &reformatted()).await;
    e.read_as(
        2,
        EditFormat::Patch,
        json!({"path": "a.rs", "start_line": 1, "end_line": 10}),
    )
    .await;
    let p = "*** Begin Patch\n*** Update File: a.rs\n@@\n     if ready {\n-        go();\n+        stop();\n     }\n*** End Patch";
    let r = e.patch(3, p).await;
    assert!(r.is_error, "{}", r.output);
    assert!(r.output.contains("lines 40-42"), "{}", r.output);
    assert_eq!(e.slurp("a.rs").await, reformatted());
}

/// C-M8: the result's meta names the stage that located a repaired edit, so evals can
/// count repairs by stage from the event stream; an exact edit names none.
#[tokio::test]
async fn meta_names_the_repair_stage() {
    let e = env();
    e.put(
        "a.py",
        "def f():\n    if x:\n        a()\n        b()\n    return 1\n",
    )
    .await;
    e.read(1, json!({"path": "a.py"})).await;
    let r = e
        .edit(
            1,
            "a.py",
            "if x:\n    a()\n    b()",
            "if x:\n    a()\n    c()\n    b()",
        )
        .await;
    assert!(!r.is_error, "{}", r.output);
    assert_eq!(r.meta.repair, Some(RepairStage::Indentation));

    let exact = e.edit(1, "a.py", "return 1", "return 2").await;
    assert!(!exact.is_error, "{}", exact.output);
    assert_eq!(exact.meta.repair, None);

    e.put("a.rs", "impl X {\n    fn f() {\n        old();\n    }\n}\n")
        .await;
    e.read_as(1, EditFormat::Patch, json!({"path": "a.rs"}))
        .await;
    let p = "*** Begin Patch\n*** Update File: a.rs\n@@\n fn f() {\n-    old();\n+    new();\n }\n*** End Patch";
    let patched = e.patch(1, p).await;
    assert!(!patched.is_error, "{}", patched.output);
    assert!(patched.meta.repair.is_some(), "{:?}", patched.meta);

    // Persisted and read back; absent in older logs; a newer stage name reads as none.
    let v = serde_json::to_value(&r.meta).unwrap();
    assert_eq!(v["repair"], "indentation");
    let back: tautly_tools::ToolMeta = serde_json::from_value(v).unwrap();
    assert_eq!(back.repair, Some(RepairStage::Indentation));
    let old: tautly_tools::ToolMeta =
        serde_json::from_value(json!({"exit_code": null, "error_kind": null})).unwrap();
    assert_eq!(old.repair, None);
    assert!(serde_json::to_value(&old).unwrap().get("repair").is_none());
    let newer: tautly_tools::ToolMeta =
        serde_json::from_value(json!({"exit_code": null, "repair": "telepathic"})).unwrap();
    assert_eq!(newer.repair, None);
    let odd: tautly_tools::ToolMeta =
        serde_json::from_value(json!({"exit_code": 1, "repair": 7})).unwrap();
    assert_eq!((odd.repair, odd.exit_code), (None, Some(1)));
}
