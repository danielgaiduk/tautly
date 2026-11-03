//! What results tell the engine (checkpoint 2: I5, I2): effect shapes, structured result
//! metadata, and the read tracker's forget API.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{Value, json};
use tautly_provider::EditFormat;
use tautly_sandbox::{LocalSandbox, Sandbox};
use tautly_tools::{
    AppliedEffect, BashTool, EditTool, LineRange, ReadTool, ReadTracker, Shown, Tool, ToolCx,
    ToolResult, WriteTool,
};
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

    fn full(&self, path: &str) -> PathBuf {
        self.sb.resolve(Path::new(path)).unwrap()
    }

    async fn run(&self, tool: &dyn Tool, turn: u32, args: Value) -> ToolResult {
        tool.run(args, &self.sb, &self.cx(turn, EditFormat::StrReplace))
            .await
    }

    async fn run_as(
        &self,
        tool: &dyn Tool,
        turn: u32,
        format: EditFormat,
        args: Value,
    ) -> ToolResult {
        tool.run(args, &self.sb, &self.cx(turn, format)).await
    }

    /// The `shown` of a single-target read.
    async fn shown(&self, turn: u32, args: Value) -> Shown {
        let r = self.run(&ReadTool, turn, args).await;
        assert!(!r.is_error, "{}", r.output);
        match r.effects.as_slice() {
            [AppliedEffect::Read { shown, .. }] => *shown,
            other => panic!("expected one read effect, got {other:?}"),
        }
    }
}

const ALL: LineRange = LineRange::ALL;

fn lines(n: usize) -> String {
    (1..=n).map(|i| format!("line {i}\n")).collect()
}

// ------------------------------------------------------------------ read effects

#[tokio::test]
async fn read_reports_range_hash_and_how_it_was_shown() {
    let e = env();
    e.put("a.txt", &lines(10)).await;
    let r = e
        .run(
            &ReadTool,
            1,
            json!({"path": "a.txt", "start_line": 2, "end_line": 4}),
        )
        .await;
    assert_eq!(
        r.effects,
        vec![AppliedEffect::Read {
            path: e.full("a.txt"),
            range: LineRange {
                start: Some(2),
                end: Some(4)
            },
            file_hash: blake3::hash(lines(10).as_bytes()),
            shown: Shown::Full,
        }]
    );
    assert_eq!(e.shown(1, json!({"path": "a.txt"})).await, Shown::Full);
    assert_eq!(
        e.shown(2, json!({"path": "a.txt"})).await,
        Shown::Unchanged { since: 1 }
    );
    e.put("a.txt", &lines(10).replace("line 5\n", "line five\n"))
        .await;
    assert_eq!(
        e.shown(3, json!({"path": "a.txt"})).await,
        Shown::Diff { since: 1 }
    );
    assert_eq!(
        e.shown(4, json!({"path": "a.txt", "full": true})).await,
        Shown::Full
    );
}

#[tokio::test]
async fn cut_reads_are_partial() {
    let e = env();
    e.put("big.txt", &lines(2500)).await;
    assert_eq!(e.shown(1, json!({"path": "big.txt"})).await, Shown::Partial);
    let wide: String = (0..400).map(|i| format!("{i:0>200}\n")).collect();
    e.put("wide.txt", &wide).await;
    assert_eq!(
        e.shown(1, json!({"path": "wide.txt"})).await,
        Shown::Partial
    );
}

#[tokio::test]
async fn hashline_change_is_shown_full() {
    let e = env();
    e.put("a.txt", &lines(5)).await;
    let r = e
        .run_as(&ReadTool, 1, EditFormat::Hashline, json!({"path": "a.txt"}))
        .await;
    assert!(!r.is_error);
    e.put("a.txt", &lines(6)).await;
    let r = e
        .run_as(&ReadTool, 2, EditFormat::Hashline, json!({"path": "a.txt"}))
        .await;
    assert!(r.output.contains("full content"), "{}", r.output);
    assert!(matches!(
        r.effects.as_slice(),
        [AppliedEffect::Read {
            shown: Shown::Full,
            ..
        }]
    ));
}

#[tokio::test]
async fn batched_read_reports_one_effect_per_target() {
    let e = env();
    e.put("a.txt", &lines(5)).await;
    e.put("b.txt", &lines(5)).await;
    e.run(&ReadTool, 1, json!({"path": "b.txt"})).await;
    let r = e
        .run(
            &ReadTool,
            2,
            json!({"targets": [
                {"path": "a.txt", "start_line": 2, "end_line": 3},
                {"path": "b.txt"},
                {"path": "missing.txt"}
            ]}),
        )
        .await;
    assert!(!r.is_error, "{}", r.output);
    let hash = blake3::hash(lines(5).as_bytes());
    assert_eq!(
        r.effects,
        vec![
            AppliedEffect::Read {
                path: e.full("a.txt"),
                range: LineRange {
                    start: Some(2),
                    end: Some(3)
                },
                file_hash: hash,
                shown: Shown::Full,
            },
            AppliedEffect::Read {
                path: e.full("b.txt"),
                range: ALL,
                file_hash: hash,
                shown: Shown::Unchanged { since: 1 },
            },
        ]
    );
}

// ------------------------------------------------------------------ write effects

#[tokio::test]
async fn edits_report_the_pre_image_mode() {
    let e = env();
    e.sb.write_atomic(Path::new("run.sh"), b"echo a\n", Some(0o755))
        .await
        .unwrap();
    e.run(&ReadTool, 1, json!({"path": "run.sh"})).await;
    let r = e
        .run(
            &EditTool,
            1,
            json!({"path": "run.sh", "old_text": "echo a", "new_text": "echo b"}),
        )
        .await;
    assert!(!r.is_error, "{}", r.output);
    assert_eq!(
        r.effects,
        vec![AppliedEffect::Edited {
            path: e.full("run.sh"),
            pre_image: Some(b"echo a\n".to_vec()),
            pre_mode: Some(0o755),
            post_hash: blake3::hash(b"echo b\n"),
        }]
    );
}

#[tokio::test]
async fn write_reports_the_pre_mode_of_an_overwritten_file_only() {
    let e = env();
    e.sb.write_atomic(Path::new("a.txt"), b"old", Some(0o600))
        .await
        .unwrap();
    let r = e
        .run(&WriteTool, 1, json!({"path": "a.txt", "content": "new"}))
        .await;
    assert!(matches!(
        r.effects.as_slice(),
        [AppliedEffect::Edited {
            pre_mode: Some(0o600),
            ..
        }]
    ));
    let r = e
        .run(&WriteTool, 1, json!({"path": "b.txt", "content": "new"}))
        .await;
    assert!(matches!(
        r.effects.as_slice(),
        [AppliedEffect::Edited {
            pre_image: None,
            pre_mode: None,
            ..
        }]
    ));
}

#[tokio::test]
async fn patch_delete_reports_deleted() {
    let e = env();
    e.sb.write_atomic(Path::new("old.sh"), b"bye\n", Some(0o750))
        .await
        .unwrap();
    e.run_as(&ReadTool, 1, EditFormat::Patch, json!({"path": "old.sh"}))
        .await;
    let r = e
        .run_as(
            &EditTool,
            1,
            EditFormat::Patch,
            json!({"patch": "*** Begin Patch\n*** Delete File: old.sh\n*** End Patch"}),
        )
        .await;
    assert!(!r.is_error, "{}", r.output);
    assert_eq!(
        r.effects,
        vec![AppliedEffect::Deleted {
            path: e.full("old.sh"),
            pre_image: b"bye\n".to_vec(),
            pre_mode: 0o750,
        }]
    );
}

// ------------------------------------------------------------------ bash

#[tokio::test]
async fn bash_reports_an_exec_effect_and_its_exit_code() {
    let e = env();
    let r = e.run(&BashTool, 1, json!({"command": "exit 3"})).await;
    assert!(r.is_error);
    assert_eq!(r.effects, vec![AppliedEffect::Exec { may_write: true }]);
    assert_eq!(r.meta.exit_code, Some(3));
    let r = e.run(&BashTool, 1, json!({"command": "true"})).await;
    assert_eq!(r.meta.exit_code, Some(0));
    assert_eq!(r.effects, vec![AppliedEffect::Exec { may_write: true }]);
}

#[tokio::test]
async fn a_killed_command_has_no_exit_code() {
    let e = env();
    let r = e
        .run(&BashTool, 1, json!({"command": "sleep 5", "timeout_s": 1}))
        .await;
    assert!(r.is_error);
    assert_eq!(r.meta.exit_code, None);
    assert_eq!(r.effects, vec![AppliedEffect::Exec { may_write: true }]);
}

// ------------------------------------------------------------------ edit error kinds

async fn edit_kind(e: &Env, old: &str, new: &str) -> Option<&'static str> {
    let r = e
        .run(
            &EditTool,
            9,
            json!({"path": "k.txt", "old_text": old, "new_text": new}),
        )
        .await;
    assert!(r.is_error, "{}", r.output);
    r.meta.error_kind
}

#[tokio::test]
async fn edit_failures_carry_a_stable_kind() {
    let e = env();
    e.put("k.txt", "a\nb\na\n").await;
    assert_eq!(edit_kind(&e, "b", "c").await, Some("not_read"));
    e.run(&ReadTool, 1, json!({"path": "k.txt"})).await;
    assert_eq!(edit_kind(&e, "zzz", "c").await, Some("not_found"));
    assert_eq!(edit_kind(&e, "a", "c").await, Some("ambiguous"));
    assert_eq!(
        edit_kind(&e, "2\tb\n3\ta", "2\tb\nx\n3\ta").await,
        Some("mixed_artifacts")
    );
    e.put("k.txt", "a\nb\na\nd\n").await;
    assert_eq!(edit_kind(&e, "d", "c").await, Some("stale"));
}

// ------------------------------------------------------------------ I2: forgetting baselines

#[tokio::test]
async fn forget_shown_at_drops_baselines_up_to_that_turn() {
    let e = env();
    for (turn, f) in [(1, "a.txt"), (2, "b.txt"), (3, "c.txt")] {
        e.put(f, &lines(3)).await;
        assert_eq!(e.shown(turn, json!({"path": f})).await, Shown::Full);
    }
    e.reads.forget_shown_at(2);
    assert_eq!(e.shown(4, json!({"path": "a.txt"})).await, Shown::Full);
    assert_eq!(e.shown(4, json!({"path": "b.txt"})).await, Shown::Full);
    assert_eq!(
        e.shown(4, json!({"path": "c.txt"})).await,
        Shown::Unchanged { since: 3 }
    );
}

#[tokio::test]
async fn forget_path_drops_every_range_of_one_path() {
    let e = env();
    e.put("a.txt", &lines(9)).await;
    e.put("b.txt", &lines(9)).await;
    e.shown(1, json!({"path": "a.txt"})).await;
    e.shown(1, json!({"path": "a.txt", "start_line": 2, "end_line": 3}))
        .await;
    e.shown(1, json!({"path": "b.txt"})).await;
    e.reads.forget_path(&e.full("a.txt"));
    assert_eq!(e.shown(2, json!({"path": "a.txt"})).await, Shown::Full);
    assert_eq!(
        e.shown(2, json!({"path": "a.txt", "start_line": 2, "end_line": 3}))
            .await,
        Shown::Full
    );
    assert_eq!(
        e.shown(2, json!({"path": "b.txt"})).await,
        Shown::Unchanged { since: 1 }
    );
}

#[tokio::test]
async fn clear_drops_baselines_but_keeps_the_guard() {
    let e = env();
    e.put("a.txt", &lines(3)).await;
    e.shown(1, json!({"path": "a.txt"})).await;
    e.reads.clear();
    // Still read as far as the stale-read guard is concerned: an exact edit needs no
    // re-read.
    let r = e
        .run(
            &EditTool,
            2,
            json!({"path": "a.txt", "old_text": "line 2", "new_text": "line two"}),
        )
        .await;
    assert!(!r.is_error, "{}", r.output);
    assert_eq!(e.shown(3, json!({"path": "a.txt"})).await, Shown::Full);
}
