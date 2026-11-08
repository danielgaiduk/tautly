//! Writes and cancellation (Task 25 fix I1): a cancel seen before the commit point
//! changes nothing; after it, the write is reported in full and the slow bookkeeping
//! (read baseline, re-index) is skipped.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;
use tautly_provider::EditFormat;
use tautly_sandbox::{LocalSandbox, Sandbox, SandboxError};
use tautly_testkit::{Delegate, Delegating};
use tautly_tools::{
    AppliedEffect, EditTool, LineRange, ReadTool, ReadTracker, Tool, ToolCx, WriteTool,
};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

fn setup() -> (TempDir, LocalSandbox) {
    let dir = TempDir::new().unwrap();
    let sb = LocalSandbox::new(dir.path().to_path_buf()).unwrap();
    (dir, sb)
}

fn cancelled_cx(dir: &TempDir) -> ToolCx {
    let cancel = CancellationToken::new();
    cancel.cancel();
    ToolCx::new(EditFormat::StrReplace, dir.path().join(".spill"))
        .with_cancel(cancel)
        .with_turn(1)
}

#[tokio::test]
async fn a_write_cancelled_before_its_commit_changes_nothing() {
    let (dir, sb) = setup();
    sb.write_atomic(Path::new("a.txt"), b"alpha\n", None)
        .await
        .unwrap();
    let r = WriteTool
        .run(
            json!({"path": "a.txt", "content": "beta\n"}),
            &sb,
            &cancelled_cx(&dir),
        )
        .await;
    assert!(r.is_error, "{}", r.output);
    assert!(r.output.contains("cancelled"), "{}", r.output);
    assert!(r.effects.is_empty());
    assert_eq!(sb.read(Path::new("a.txt")).await.unwrap(), b"alpha\n");
}

#[tokio::test]
async fn an_edit_cancelled_before_its_commit_changes_nothing() {
    let (dir, sb) = setup();
    sb.write_atomic(Path::new("a.txt"), b"alpha\n", None)
        .await
        .unwrap();
    // Read first (an edit needs it), with a live token and the same tracker.
    let reads = Arc::new(ReadTracker::default());
    let live = ToolCx::new(EditFormat::StrReplace, dir.path().join(".spill"))
        .with_reads(Arc::clone(&reads))
        .with_turn(1);
    let read = ReadTool.run(json!({"path": "a.txt"}), &sb, &live).await;
    assert!(!read.is_error, "{}", read.output);
    let r = EditTool
        .run(
            json!({"path": "a.txt", "old_text": "alpha", "new_text": "beta"}),
            &sb,
            &cancelled_cx(&dir).with_reads(reads),
        )
        .await;
    assert!(r.is_error, "{}", r.output);
    assert!(r.output.contains("cancelled"), "{}", r.output);
    assert!(r.effects.is_empty());
    assert_eq!(sb.read(Path::new("a.txt")).await.unwrap(), b"alpha\n");
}

/// A sandbox that cancels `cancel` as soon as a write has been committed.
struct CancelOnWrite {
    sb: LocalSandbox,
    cancel: CancellationToken,
}

#[async_trait]
impl Delegate for CancelOnWrite {
    fn inner(&self) -> &LocalSandbox {
        &self.sb
    }
    async fn write_atomic(
        &self,
        p: &Path,
        bytes: &[u8],
        mode: Option<u32>,
    ) -> Result<(), SandboxError> {
        let r = self.sb.write_atomic(p, bytes, mode).await;
        self.cancel.cancel();
        r
    }
}

#[tokio::test]
async fn a_write_cancelled_after_its_commit_reports_the_change_and_skips_the_baseline() {
    let (dir, sb) = setup();
    let cancel = CancellationToken::new();
    let sb = Delegating(CancelOnWrite {
        sb,
        cancel: cancel.clone(),
    });
    sb.sb
        .write_atomic(Path::new("a.txt"), b"alpha\n", None)
        .await
        .unwrap();
    let reads = Arc::new(ReadTracker::default());
    let cx = ToolCx::new(EditFormat::StrReplace, dir.path().join(".spill"))
        .with_cancel(cancel)
        .with_reads(Arc::clone(&reads))
        .with_turn(1);
    let r = WriteTool
        .run(json!({"path": "a.txt", "content": "beta\n"}), &sb, &cx)
        .await;
    assert!(!r.is_error, "{}", r.output);
    assert!(matches!(r.effects[0], AppliedEffect::Edited { .. }));
    assert_eq!(sb.read(Path::new("a.txt")).await.unwrap(), b"beta\n");
    // No baseline (its spill was skipped), but the stale-read guard knows the new file.
    let full = dir.path().canonicalize().unwrap().join("a.txt");
    assert!(
        reads
            .previous(&full, LineRange::ALL, EditFormat::StrReplace)
            .is_none()
    );
    assert_eq!(
        reads.last(&full).map(|(_, h)| h),
        Some(blake3::hash(b"beta\n"))
    );
}
