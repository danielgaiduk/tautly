//! Unified diffs of file changes (spec 7.11.4): what an edit or write would apply, shown in
//! approvals, and what it did apply, shown in results.
//!
//! Format: 3 context lines, `--- a/<path>` / `+++ b/<path>` with root-relative paths,
//! `/dev/null` for a created or deleted file, `Binary file <path> differs` when either side
//! is not UTF-8 text; several files are concatenated in order.
//!
//! Cost is bounded: line diffing is quadratic in the worst case (a full rewrite), so one
//! call gets [`DIFF_BUDGET`], after which a file shows a summary line with its counts
//! instead of hunks; text past the caps is counted, never formatted; and a change of more
//! than [`BLOCKING_DIFF_BYTES`] is diffed on the blocking pool.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use similar::{Algorithm, ChangeTag, TextDiff};
use tautly_sandbox::{Sandbox, SandboxError};

use crate::{ToolCx, ToolResult};

/// Most lines of an approval or result diff.
pub const PREVIEW_MAX_LINES: u32 = 200;
/// Most bytes of an approval or result diff.
pub const PREVIEW_MAX_BYTES: usize = 16 * 1024;
/// How long one diff call may spend matching lines before files fall back to a summary.
pub const DIFF_BUDGET: Duration = Duration::from_millis(300);
/// Changes with more content than this are diffed on the blocking pool.
pub const BLOCKING_DIFF_BYTES: usize = 64 * 1024;
/// Context lines around each change.
const CONTEXT: usize = 3;
/// The error kind of a call whose file changed between its approval and its run.
pub(crate) const CHANGED_AFTER_APPROVAL: &str = "changed_after_approval";

/// One file's change for [`unified_diff`]: `(root-relative path, before, after)`, where
/// None is a file that does not exist on that side.
pub type FileChange<'a> = (PathBuf, Option<&'a [u8]>, Option<&'a [u8]>);

/// [`FileChange`] with owned content.
pub type OwnedChange = (PathBuf, Option<Vec<u8>>, Option<Vec<u8>>);

/// A file a call starts from: its resolved path and content hash (None: does not exist).
pub type Base = (PathBuf, Option<blake3::Hash>);

/// A unified diff, capped for display.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffPreview {
    /// The diff text; when `truncated`, it ends with `… N more lines (diff truncated)`.
    pub unified: String,
    /// Lines in the full (uncapped) diff.
    pub lines: u32,
    pub truncated: bool,
}

/// What a tool would change, from [`crate::Tool::preview`]: the diff, and the hash of each
/// file it starts from, keyed by the sandbox-resolved path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preview {
    pub diff: DiffPreview,
    pub bases: Vec<Base>,
}

/// Lines one file's diff adds and removes (0 for a binary file).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LineCounts {
    pub added: u32,
    pub removed: u32,
}

/// The diff of several files, concatenated, capped at `max_lines` and `max_bytes`.
pub fn unified_diff(files: &[FileChange<'_>], max_lines: u32, max_bytes: usize) -> DiffPreview {
    diff_files(files, max_lines, max_bytes).0
}

/// [`unified_diff`], plus each file's line counts (in `files` order).
pub fn diff_files(
    files: &[FileChange<'_>],
    max_lines: u32,
    max_bytes: usize,
) -> (DiffPreview, Vec<LineCounts>) {
    let deadline = Instant::now() + DIFF_BUDGET;
    let mut out = Capped::new(max_lines, max_bytes);
    let counts = files
        .iter()
        .map(|(path, before, after)| render(&mut out, path, *before, *after, deadline))
        .collect();
    (out.finish(), counts)
}

/// [`diff_files`] on the blocking pool when the content is over [`BLOCKING_DIFF_BYTES`].
pub async fn diff_owned(
    files: Vec<OwnedChange>,
    max_lines: u32,
    max_bytes: usize,
) -> (DiffPreview, Vec<LineCounts>) {
    let size = files
        .iter()
        .map(|(_, a, b)| a.as_ref().map_or(0, Vec::len) + b.as_ref().map_or(0, Vec::len))
        .sum::<usize>();
    let run = move || {
        let refs: Vec<FileChange<'_>> = files
            .iter()
            .map(|(p, a, b)| (p.clone(), a.as_deref(), b.as_deref()))
            .collect();
        diff_files(&refs, max_lines, max_bytes)
    };
    if size <= BLOCKING_DIFF_BYTES {
        return run();
    }
    match tokio::task::spawn_blocking(run).await {
        Ok(d) => d,
        Err(e) => std::panic::resume_unwind(e.into_panic()),
    }
}

/// [`unified_diff`] with the approval/result caps, off the async workers when large.
pub(crate) async fn display_diff(files: &[FileChange<'_>]) -> DiffPreview {
    let size = files
        .iter()
        .map(|(_, a, b)| a.map_or(0, <[u8]>::len) + b.map_or(0, <[u8]>::len))
        .sum::<usize>();
    if size <= BLOCKING_DIFF_BYTES {
        return unified_diff(files, PREVIEW_MAX_LINES, PREVIEW_MAX_BYTES);
    }
    let owned = files
        .iter()
        .map(|(p, a, b)| (p.clone(), a.map(<[u8]>::to_vec), b.map(<[u8]>::to_vec)))
        .collect();
    diff_owned(owned, PREVIEW_MAX_LINES, PREVIEW_MAX_BYTES)
        .await
        .0
}

/// Append one file's diff to `out`; its line counts.
fn render(
    out: &mut Capped,
    path: &Path,
    before: Option<&[u8]>,
    after: Option<&[u8]>,
    deadline: Instant,
) -> LineCounts {
    if before == after {
        return LineCounts::default();
    }
    let shown = path.to_string_lossy();
    let (Ok(old), Ok(new)) = (text_of(before), text_of(after)) else {
        out.push(&[&format!("Binary file {shown} differs\n")]);
        return LineCounts::default();
    };
    let diff = TextDiff::configure()
        .algorithm(Algorithm::Myers)
        .deadline(deadline)
        .diff_lines(old, new);
    // Past the deadline the matcher gave up part way: its diff is valid but no longer a
    // useful one to read, and its counts are an upper bound.
    let timed_out = Instant::now() >= deadline;
    let mut counts = LineCounts::default();
    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Insert => counts.added = counts.added.saturating_add(1),
            ChangeTag::Delete => counts.removed = counts.removed.saturating_add(1),
            ChangeTag::Equal => {}
        }
    }
    if timed_out {
        out.push(&[&format!(
            "File {shown}: diff too large or slow to show, summary only: +{} -{} lines\n",
            counts.added, counts.removed
        )]);
        return counts;
    }
    let old_header = match before {
        Some(_) => format!("--- a/{shown}\n"),
        None => "--- /dev/null\n".to_string(),
    };
    let new_header = match after {
        Some(_) => format!("+++ b/{shown}\n"),
        None => "+++ /dev/null\n".to_string(),
    };
    out.push(&[&old_header]);
    out.push(&[&new_header]);
    let mut unified = diff.unified_diff();
    unified.context_radius(CONTEXT);
    for hunk in unified.iter_hunks() {
        if out.full() {
            let lines: usize = hunk
                .iter_changes()
                .map(|c| 1 + usize::from(c.missing_newline()))
                .sum();
            out.skip(1 + lines);
            continue;
        }
        out.push(&[&format!("{}\n", hunk.header())]);
        for change in hunk.iter_changes() {
            if out.full() {
                out.skip(1 + usize::from(change.missing_newline()));
                continue;
            }
            let tag = match change.tag() {
                ChangeTag::Equal => " ",
                ChangeTag::Delete => "-",
                ChangeTag::Insert => "+",
            };
            if change.missing_newline() {
                out.push(&[tag, change.value(), "\n"]);
                out.push(&["\\ No newline at end of file\n"]);
            } else {
                out.push(&[tag, change.value()]);
            }
        }
    }
    counts
}

/// A missing side is empty text; content that is not UTF-8 text cannot be diffed by line.
fn text_of(side: Option<&[u8]>) -> Result<&str, &'static str> {
    side.map_or(Ok(""), crate::spec::decode_text)
}

/// Diff text being built under a line and byte cap. Lines past the cap are only counted.
struct Capped {
    text: String,
    /// Lines of the full diff so far.
    lines: usize,
    /// Lines in `text`.
    kept: usize,
    /// A line did not fit: nothing more goes into `text`.
    stopped: bool,
    max_lines: usize,
    max_bytes: usize,
}

impl Capped {
    fn new(max_lines: u32, max_bytes: usize) -> Self {
        Capped {
            text: String::new(),
            lines: 0,
            kept: 0,
            stopped: false,
            max_lines: usize::try_from(max_lines).unwrap_or(usize::MAX),
            max_bytes,
        }
    }

    fn full(&self) -> bool {
        self.stopped
    }

    /// One line, given in parts that end with its newline.
    fn push(&mut self, parts: &[&str]) {
        self.lines += 1;
        if self.stopped {
            return;
        }
        let len: usize = parts.iter().map(|p| p.len()).sum();
        if self.kept < self.max_lines && self.text.len() + len <= self.max_bytes {
            parts.iter().for_each(|p| self.text.push_str(p));
            self.kept += 1;
        } else {
            self.stopped = true;
        }
    }

    /// `n` lines that are counted but not shown.
    fn skip(&mut self, n: usize) {
        self.lines += n;
        self.stopped |= n > 0;
    }

    /// The text, ending with the marker when lines were left out; whole lines are dropped
    /// from the end until the marker fits the byte cap.
    fn finish(mut self) -> DiffPreview {
        let lines = u32::try_from(self.lines).unwrap_or(u32::MAX);
        if self.stopped {
            loop {
                let marker = format!("… {} more lines (diff truncated)", self.lines - self.kept);
                if self.kept == 0 || self.text.len() + marker.len() <= self.max_bytes {
                    self.text.push_str(&marker);
                    break;
                }
                let body = &self.text[..self.text.len() - 1];
                let cut = body.rfind('\n').map_or(0, |i| i + 1);
                self.text.truncate(cut);
                self.kept -= 1;
            }
        }
        DiffPreview {
            unified: self.text,
            lines,
            truncated: self.stopped,
        }
    }
}

/// The display path of `full`: relative to the sandbox root, else as the call named it.
pub(crate) fn shown_path(sb: &dyn Sandbox, full: &Path, requested: &str) -> PathBuf {
    full.strip_prefix(sb.root())
        .map_or_else(|_| PathBuf::from(requested), Path::to_path_buf)
}

fn changed_after_approval(sb: &dyn Sandbox, path: &Path) -> ToolResult {
    let shown = shown_path(sb, path, &path.to_string_lossy());
    ToolResult::error_kind(
        format!(
            "stale: file changed after approval: {} changed between the approval and the \
             write, so nothing was changed; read it again before editing",
            shown.display()
        ),
        CHANGED_AFTER_APPROVAL,
    )
}

/// When the call was approved on a preview, refuse unless `bases` (the files the run's
/// own pipeline just read, resolved and hashed) are exactly the approved ones: then what
/// is written is what was approved, whatever moved in between (contents or symlinks).
pub(crate) fn check_approved(
    sb: &dyn Sandbox,
    cx: &ToolCx,
    bases: &[Base],
) -> Result<(), ToolResult> {
    let Some(approved) = cx.approved_bases() else {
        return Ok(());
    };
    if approved == bases {
        return Ok(());
    }
    let differs = approved
        .iter()
        .zip(bases)
        .find(|(a, b)| a != b)
        .map(|(a, _)| a)
        .or_else(|| approved.get(bases.len()))
        .or_else(|| bases.get(approved.len()));
    let path = differs.map_or(Path::new(""), |(p, _)| p.as_path());
    Err(changed_after_approval(sb, path))
}

/// The run of an approved call failed before writing anything (say, the stale guard): if
/// an approved file changed since the preview, report that instead, since it is the cause.
pub(crate) async fn explain_failure(
    sb: &dyn Sandbox,
    cx: &ToolCx,
    failure: ToolResult,
) -> ToolResult {
    let Some(approved) = cx.approved_bases() else {
        return failure;
    };
    for (path, base) in approved {
        let now = match sb.read(path).await {
            Ok(bytes) => Some(blake3::hash(&bytes)),
            Err(SandboxError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(_) => continue,
        };
        if now != *base {
            return changed_after_approval(sb, path);
        }
    }
    failure
}
