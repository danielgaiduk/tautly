//! Unified diffs of file changes (spec 7.11.4): what an edit or write would apply, shown in
//! approvals, and what it did apply, shown in results.
//!
//! Format: 3 context lines, `--- a/<path>` / `+++ b/<path>` with root-relative paths,
//! `/dev/null` for a created or deleted file, `Binary file <path> differs` when either side
//! is not UTF-8 text; several files are concatenated in order.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use similar::{ChangeTag, TextDiff};
use tautly_sandbox::{Sandbox, SandboxError};

use crate::{ToolCx, ToolResult};

/// Most lines of an approval or result diff.
pub const PREVIEW_MAX_LINES: u32 = 200;
/// Most bytes of an approval or result diff.
pub const PREVIEW_MAX_BYTES: usize = 16 * 1024;
/// Context lines around each change.
const CONTEXT: usize = 3;
/// Changes with more content than this are diffed on the blocking pool.
const BLOCKING_DIFF_BYTES: usize = 1024 * 1024;
/// The error kind of a call whose file changed between its approval and its run.
pub(crate) const CHANGED_AFTER_APPROVAL: &str = "changed_after_approval";

/// One file's change for [`unified_diff`]: `(root-relative path, before, after)`, where
/// None is a file that does not exist on that side.
pub type FileChange<'a> = (PathBuf, Option<&'a [u8]>, Option<&'a [u8]>);

/// A unified diff, capped for display.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffPreview {
    /// The diff text; when `truncated`, it ends with `… N more lines (diff truncated)`.
    pub unified: String,
    /// Lines in the full (uncapped) diff.
    pub lines: u32,
    pub truncated: bool,
}

/// What a tool would change, from [`crate::Tool::preview`]: the diff, and the hash of each
/// file it starts from (None: the file does not exist), keyed by the sandbox-resolved path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preview {
    pub diff: DiffPreview,
    pub bases: Vec<(PathBuf, Option<blake3::Hash>)>,
}

/// One file's diff, uncapped, with its line counts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
    /// Empty when the two sides are equal.
    pub text: String,
    /// Lines added and removed (0 for a binary file).
    pub added: u32,
    pub removed: u32,
}

/// The diff of one file at root-relative `path` from `before` to `after` (None: the file
/// does not exist on that side).
pub fn file_diff(path: &Path, before: Option<&[u8]>, after: Option<&[u8]>) -> FileDiff {
    let empty = FileDiff {
        text: String::new(),
        added: 0,
        removed: 0,
    };
    if before == after {
        return empty;
    }
    let shown = path.to_string_lossy();
    let (Ok(old), Ok(new)) = (text_of(before), text_of(after)) else {
        return FileDiff {
            text: format!("Binary file {shown} differs\n"),
            ..empty
        };
    };
    let old_header = before.map_or_else(|| "/dev/null".to_string(), |_| format!("a/{shown}"));
    let new_header = after.map_or_else(|| "/dev/null".to_string(), |_| format!("b/{shown}"));
    let diff = TextDiff::from_lines(old, new);
    let (mut added, mut removed) = (0u32, 0u32);
    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Insert => added = added.saturating_add(1),
            ChangeTag::Delete => removed = removed.saturating_add(1),
            ChangeTag::Equal => {}
        }
    }
    let text = diff
        .unified_diff()
        .context_radius(CONTEXT)
        .header(&old_header, &new_header)
        .to_string();
    FileDiff {
        text,
        added,
        removed,
    }
}

/// A missing side is empty text; content that is not UTF-8 text cannot be diffed by line.
fn text_of(side: Option<&[u8]>) -> Result<&str, &'static str> {
    side.map_or(Ok(""), crate::spec::decode_text)
}

/// The diff of several files, concatenated, capped at `max_lines` and `max_bytes`.
pub fn unified_diff(files: &[FileChange<'_>], max_lines: u32, max_bytes: usize) -> DiffPreview {
    let full: String = files
        .iter()
        .map(|(path, before, after)| file_diff(path, *before, *after).text)
        .collect();
    cap(full, max_lines, max_bytes)
}

/// Cut `full` (whole lines, each ending in a newline) to at most `max_lines` lines and
/// `max_bytes` bytes, the marker included.
pub fn cap(full: String, max_lines: u32, max_bytes: usize) -> DiffPreview {
    let total = full.split_inclusive('\n').count();
    let lines = u32::try_from(total).unwrap_or(u32::MAX);
    if lines <= max_lines && full.len() <= max_bytes {
        return DiffPreview {
            unified: full,
            lines,
            truncated: false,
        };
    }
    // Room for the longest marker this diff can need.
    let reserve = marker(total).len();
    let mut kept = 0usize;
    let mut len = 0usize;
    for line in full.split_inclusive('\n') {
        if kept as u64 >= u64::from(max_lines) || len + line.len() + reserve > max_bytes {
            break;
        }
        kept += 1;
        len += line.len();
    }
    let mut unified = full;
    unified.truncate(len);
    unified.push_str(&marker(total - kept));
    DiffPreview {
        unified,
        lines,
        truncated: true,
    }
}

fn marker(more: usize) -> String {
    format!("… {more} more lines (diff truncated)")
}

/// The display path of `full`: relative to the sandbox root, else as the call named it.
pub(crate) fn shown_path(sb: &dyn Sandbox, full: &Path, requested: &str) -> PathBuf {
    full.strip_prefix(sb.root())
        .map_or_else(|_| PathBuf::from(requested), Path::to_path_buf)
}

/// [`unified_diff`] with the approval/result caps; large changes are diffed on the
/// blocking pool.
pub(crate) async fn display_diff(files: &[FileChange<'_>]) -> DiffPreview {
    let size: usize = files
        .iter()
        .map(|(_, a, b)| a.map_or(0, <[u8]>::len) + b.map_or(0, <[u8]>::len))
        .sum();
    if size <= BLOCKING_DIFF_BYTES {
        return unified_diff(files, PREVIEW_MAX_LINES, PREVIEW_MAX_BYTES);
    }
    type Owned = (PathBuf, Option<Vec<u8>>, Option<Vec<u8>>);
    let owned: Vec<Owned> = files
        .iter()
        .map(|(p, a, b)| (p.clone(), a.map(<[u8]>::to_vec), b.map(<[u8]>::to_vec)))
        .collect();
    let diffed = tokio::task::spawn_blocking(move || {
        let refs: Vec<FileChange<'_>> = owned
            .iter()
            .map(|(p, a, b)| (p.clone(), a.as_deref(), b.as_deref()))
            .collect();
        unified_diff(&refs, PREVIEW_MAX_LINES, PREVIEW_MAX_BYTES)
    })
    .await;
    match diffed {
        Ok(d) => d,
        Err(e) => std::panic::resume_unwind(e.into_panic()),
    }
}

/// When the call was approved on a preview, refuse to run if any file it starts from no
/// longer has the hash the preview saw: the approved diff must be the applied diff.
pub(crate) async fn check_approved(sb: &dyn Sandbox, cx: &ToolCx) -> Result<(), ToolResult> {
    let Some(bases) = cx.approved_bases() else {
        return Ok(());
    };
    for (path, base) in bases {
        let shown = shown_path(sb, path, &path.to_string_lossy());
        let now = match sb.read(path).await {
            Ok(bytes) => Some(blake3::hash(&bytes)),
            Err(SandboxError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                return Err(ToolResult::error_kind(
                    format!("{}: {e}", shown.display()),
                    "io",
                ));
            }
        };
        if now != *base {
            return Err(ToolResult::error_kind(
                format!(
                    "stale: file changed after approval: {} changed between the approval and \
                     the write, so nothing was changed; read it again before editing",
                    shown.display()
                ),
                CHANGED_AFTER_APPROVAL,
            ));
        }
    }
    Ok(())
}
