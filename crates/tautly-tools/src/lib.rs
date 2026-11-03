//! The `Tool` trait, shared tool context, and the built-in read-side tools.

mod bash;
mod cap;
pub mod edit;
pub mod filters;
mod find;
mod glob;
mod grep;
mod index_tools;
mod memory_tools;
mod persist;
mod pin_state;
pub mod preview;
mod procs;
mod read;
mod reread;
mod spec;
mod web_fetch;
mod write;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::SystemTime;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tautly_index::Index;
use tautly_memory::MemoryStore;
use tautly_provider::{Capabilities, EditFormat, SideModel, ToolSpec};
use tautly_sandbox::{Effect, FileMeta, Sandbox};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub use bash::{BashTool, cap_output};
pub use cap::cap_text;
pub use edit::EditTool;
pub use find::FindTool;
pub use glob::GlobTool;
pub use grep::GrepTool;
pub use index_tools::{OutlineTool, RefsTool, SymbolTool};
pub use memory_tools::{RecallTool, RememberTool};
pub use persist::PersistedEffect;
pub use pin_state::{
    ContextOp, MAX_PIN_BYTES, PinTool, PlanStep, StatePatch, StateTool, StepStatus,
    parse_context_op,
};
pub use preview::{DiffPreview, FileChange, Preview, unified_diff};
pub use procs::{ProcessGroups, group_alive};
pub use read::ReadTool;
pub use web_fetch::WebFetchTool;
pub use write::WriteTool;

/// Invalid tool arguments.
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("invalid arguments: {0}")]
    BadArgs(String),
}

/// A side effect a tool actually applied, reported to the engine. Paths are absolute and
/// canonical (as the sandbox resolved them).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AppliedEffect {
    /// One read target: `range` as requested (a symbol target: the symbol's lines),
    /// `file_hash` of the whole file, and how it was shown.
    Read {
        path: PathBuf,
        range: LineRange,
        #[serde(with = "persist::hash_hex")]
        file_hash: blake3::Hash,
        shown: Shown,
    },
    /// A file was written. `pre_image`/`pre_mode` (permission bits) describe it before the
    /// write; both are None when the write created it.
    Edited {
        path: PathBuf,
        #[serde(with = "persist::b64_opt")]
        pre_image: Option<Vec<u8>>,
        pre_mode: Option<u32>,
        #[serde(with = "persist::hash_hex")]
        post_hash: blake3::Hash,
    },
    /// A file was deleted; `pre_mode` holds its permission bits.
    Deleted {
        path: PathBuf,
        #[serde(with = "persist::b64")]
        pre_image: Vec<u8>,
        pre_mode: u32,
    },
    /// A command ran that may have changed any file.
    Exec { may_write: bool },
}

/// How a read target was shown, which tells the engine what the output depends on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Shown {
    /// The requested lines in full; this output is now the target's re-read baseline.
    Full,
    /// Cut short (the 2,000-line window or the output cap); no baseline was kept.
    Partial,
    /// `unchanged since turn N`: only meaningful while the turn-`since` output is in context.
    Unchanged { since: u32 },
    /// A diff against the turn-`since` output, which must stay in context; this output is
    /// also the new baseline.
    Diff { since: u32 },
}

/// Structured facts about a result, so the engine never parses `output`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct ToolMeta {
    /// A command's exit code (signals as 128 + n); None when it was killed or never ran.
    pub exit_code: Option<i32>,
    /// A stable name for why the call failed (e.g. `not_found`, `ambiguous`, `stale`).
    /// Read back from a log, a name this version does not know becomes `unknown`.
    pub error_kind: Option<&'static str>,
    /// Passed and failed counts a test-output filter parsed from a command's output (set
    /// by `bash` for recognized test tools, whatever the output's size).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub counts: Option<TestCounts>,
}

/// What a test tool's output said: tests passed, and tests failed or errored (a crashed
/// test binary or a collection error counts as a failure).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestCounts {
    pub passed: u32,
    pub failed: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResult {
    pub output: String,
    pub is_error: bool,
    pub effects: Vec<AppliedEffect>,
    pub meta: ToolMeta,
    /// The diff a successful `edit` or `write` applied (spec 7.11.4).
    /// Boxed to keep `ToolResult` small (it is the error type of many helpers).
    pub applied_diff: Option<Box<DiffPreview>>,
}

impl ToolResult {
    pub fn ok(output: impl Into<String>) -> Self {
        Self {
            output: output.into(),
            is_error: false,
            effects: Vec::new(),
            meta: ToolMeta::default(),
            applied_diff: None,
        }
    }

    pub fn error(output: impl Into<String>) -> Self {
        Self {
            is_error: true,
            ..Self::ok(output)
        }
    }

    /// An error result with a stable [`ToolMeta::error_kind`].
    pub fn error_kind(output: impl Into<String>, kind: &'static str) -> Self {
        let mut r = Self::error(output);
        r.meta.error_kind = Some(kind);
        r
    }
}

/// The lines of a file a read asked for: 1-based, inclusive, `None` = open end.
/// Two reads are "the same read" when their ranges are equal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LineRange {
    pub start: Option<usize>,
    pub end: Option<usize>,
}

impl LineRange {
    /// The whole file.
    pub const ALL: Self = Self {
        start: None,
        end: None,
    };

    /// Whether the two ranges share a line (an open end reaches the end of the file).
    fn overlaps(&self, other: &Self) -> bool {
        let (a_start, a_end) = (self.start.unwrap_or(1), self.end.unwrap_or(usize::MAX));
        let (b_start, b_end) = (other.start.unwrap_or(1), other.end.unwrap_or(usize::MAX));
        a_start <= b_end && b_start <= a_end
    }
}

/// What the tracker remembers of the last read of one path and range. The content itself
/// lives on disk at `content_path` (a sandbox-relative spill file).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrevRead {
    /// Hash of the whole file at that time.
    pub hash: blake3::Hash,
    /// The edit format active when it was shown: other formats show other text.
    pub format: EditFormat,
    /// Hash of the range's content, which decides whether a re-read is unchanged.
    pub content_hash: blake3::Hash,
    pub turn: u32,
    pub mtime: SystemTime,
    pub content_path: PathBuf,
}

/// Remembers the mtime and content hash of each file the model has read or written, and
/// the last content returned per path and range (for diff-on-re-read, spec 7.2).
#[derive(Debug, Default)]
pub struct ReadTracker {
    seen: Mutex<HashMap<PathBuf, (SystemTime, blake3::Hash)>>,
    ranges: Mutex<HashMap<(PathBuf, LineRange), PrevRead>>,
}

impl ReadTracker {
    /// Record that `range` of `path` was returned at `turn` with `content` (the range's
    /// lines, newline-joined, no line numbers). `hash` is the hash of the whole file.
    /// The content is spilled through the sandbox; only a reference stays in memory. If
    /// the spill fails the range is simply not remembered, so the next read is full. The
    /// new view supersedes every baseline of `path` whose range overlaps `range`.
    #[allow(clippy::too_many_arguments)]
    pub async fn record(
        &self,
        sb: &dyn Sandbox,
        path: &Path,
        range: LineRange,
        format: EditFormat,
        meta: &FileMeta,
        hash: blake3::Hash,
        turn: u32,
        content: &str,
    ) {
        // Spill first: everything after is synchronous, so a dropped future cannot leave
        // a half-applied record.
        let spilled = sb.spill(content.as_bytes()).await;
        self.refresh(path, meta, hash);
        let prev = spilled.ok().map(|content_path| PrevRead {
            hash,
            format,
            content_hash: blake3::hash(content.as_bytes()),
            turn,
            mtime: meta.mtime,
            content_path,
        });
        self.supersede(path, range, prev);
    }

    /// Make `new` the baseline of `range` of `path`, dropping every baseline of the path
    /// that overlaps it. `None` (the content could not be kept) only drops them.
    ///
    /// Every baseline goes through here. Its `turn` must be the 1-based turn it was shown
    /// in (debug builds check): a call whose `ToolCx` lacks `with_turn` would record turn
    /// 0, answer `unchanged since turn 0`, and escape compaction's `forget_shown_at`.
    pub fn supersede(&self, path: &Path, range: LineRange, new: Option<PrevRead>) {
        debug_assert!(
            new.as_ref().is_none_or(|p| p.turn >= 1),
            "a re-read baseline needs the 1-based turn it was shown in (ToolCx::with_turn)"
        );
        let mut ranges = self.ranges.lock().unwrap_or_else(|e| e.into_inner());
        ranges.retain(|(p, r), _| p != path || !r.overlaps(&range));
        if let Some(prev) = new {
            ranges.insert((path.to_path_buf(), range), prev);
        }
    }

    /// Drop the whole-file baseline of `path` unless it describes a file with `hash`: the
    /// guard has moved on to newer content that the model has not seen whole.
    pub fn drop_stale_whole(&self, path: &Path, hash: blake3::Hash) {
        let mut ranges = self.ranges.lock().unwrap_or_else(|e| e.into_inner());
        let key = (path.to_path_buf(), LineRange::ALL);
        if ranges.get(&key).is_some_and(|prev| prev.hash != hash) {
            ranges.remove(&key);
        }
    }

    /// Drop the remembered content of `range` of `path`: the model did not see all of it
    /// (the output was truncated), so the next read of the range must return content.
    pub fn forget(&self, path: &Path, range: LineRange) {
        let mut ranges = self.ranges.lock().unwrap_or_else(|e| e.into_inner());
        ranges.remove(&(path.to_path_buf(), range));
    }

    /// Drop every re-read baseline of `path` (a read result of it left the context).
    /// Like [`clear`](Self::clear), this keeps the stale-read guard, which tracks the disk.
    pub fn forget_path(&self, path: &Path) {
        let mut ranges = self.ranges.lock().unwrap_or_else(|e| e.into_inner());
        ranges.retain(|(p, _), _| p != path);
    }

    /// Drop every baseline shown at or before `turn` (those turns were compacted away).
    pub fn forget_shown_at(&self, turn: u32) {
        let mut ranges = self.ranges.lock().unwrap_or_else(|e| e.into_inner());
        ranges.retain(|_, prev| prev.turn > turn);
    }

    /// Drop every baseline shown at `turn`: the engine abandoned a call of that turn
    /// (it did not stop in time after a cancel), so what the model saw then is unknown.
    pub fn forget_recorded_at(&self, turn: u32) {
        let mut ranges = self.ranges.lock().unwrap_or_else(|e| e.into_inner());
        ranges.retain(|_, prev| prev.turn != turn);
    }

    /// Drop every baseline. The stale-read guard is kept: it says what is on disk, not
    /// what is in context.
    pub fn clear(&self) {
        let mut ranges = self.ranges.lock().unwrap_or_else(|e| e.into_inner());
        ranges.clear();
    }

    /// Update only the stale-read guard's view of `path`.
    pub fn refresh(&self, path: &Path, meta: &FileMeta, hash: blake3::Hash) {
        let mut seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
        seen.insert(path.to_path_buf(), (meta.mtime, hash));
    }

    pub fn last(&self, path: &Path) -> Option<(SystemTime, blake3::Hash)> {
        let seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
        seen.get(path).copied()
    }

    /// Whether the model has seen lines `first..=last` (1-based) of `path` as they are now:
    /// its whole-file baseline describes the file with `hash`, or a baseline whose range
    /// covers those lines showed exactly what they hold now. `lines` are the file's current
    /// lines, as `read` splits them. Tolerant repairs may only match such content (spec 7.9).
    pub fn has_seen(
        &self,
        path: &Path,
        hash: blake3::Hash,
        lines: &[&str],
        first: usize,
        last: usize,
    ) -> bool {
        let ranges = self.ranges.lock().unwrap_or_else(|e| e.into_inner());
        if ranges
            .get(&(path.to_path_buf(), LineRange::ALL))
            .is_some_and(|prev| prev.hash == hash)
        {
            return true;
        }
        let total = lines.len();
        ranges.iter().any(|((p, r), prev)| {
            let start = r.start.unwrap_or(1);
            let end = r.end.unwrap_or(total).min(total);
            p == path
                && start >= 1
                && start <= first
                && last <= end
                && prev.content_hash == blake3::hash(lines[start - 1..end].join("\n").as_bytes())
        })
    }

    /// The last read of exactly this `range` of `path` shown in `format`, if its content
    /// was kept.
    pub fn previous(&self, path: &Path, range: LineRange, format: EditFormat) -> Option<PrevRead> {
        let ranges = self.ranges.lock().unwrap_or_else(|e| e.into_inner());
        let prev = ranges.get(&(path.to_path_buf(), range))?;
        (prev.format == format).then(|| prev.clone())
    }
}

/// A write tool saw the cancel before its commit point.
pub(crate) const CANCELLED_BEFORE_WRITE: &str =
    "cancelled: the turn was cancelled before this write; nothing was changed";

/// Most lines a read returns when no end line is given.
pub(crate) const MAX_READ_LINES: usize = 2000;

/// Record the post-edit state of `full` (`bytes` as just written) so a later read of the
/// whole file is diffed against what the edit left, not against the pre-edit read.
///
/// After `write` the model supplied the entire content. After an edit it has seen the whole
/// file only if its whole-file baseline describes the file the edit started from (`from`,
/// the pre-edit bytes): an edit after a partial read, or after a change it never saw, must
/// not make a whole-file read answer `unchanged`. Nor may a repaired edit: the file then
/// differs from what the model believes it wrote (re-indented, normalized, ...).
pub(crate) enum WrittenBy<'a> {
    Write,
    /// An edit of a file that had these bytes (none: the edit created the file).
    Edit {
        from: Option<&'a [u8]>,
        /// A tolerant stage located some of it (spec 7.9).
        repaired: bool,
    },
}

pub(crate) async fn record_written_file(
    sb: &dyn Sandbox,
    cx: &ToolCx,
    full: &Path,
    meta: &FileMeta,
    bytes: &[u8],
    by: WrittenBy<'_>,
) {
    let hash = blake3::hash(bytes);
    // Cancelled after the commit: keep only the guard (cheap), no spilled baseline, so
    // the result comes back at once.
    if cx.cancel().is_cancelled() {
        cx.reads().refresh(full, meta, hash);
        cx.reads().forget(full, LineRange::ALL);
        return;
    }
    // Hashline edits move line numbers and the model has not seen the new anchors, so a
    // read after one must return the anchored lines, never `unchanged`: keep the older
    // baselines (a changed range then comes back in full) and only refresh the guard.
    if cx.edit_format() == EditFormat::Hashline {
        cx.reads().refresh(full, meta, hash);
        return;
    }
    let seen_whole = match by {
        WrittenBy::Write => true,
        WrittenBy::Edit { repaired: true, .. } => false,
        WrittenBy::Edit { from, .. } => from.is_some_and(|from| {
            cx.reads()
                .previous(full, LineRange::ALL, cx.edit_format())
                .is_some_and(|prev| prev.hash == blake3::hash(from))
        }),
    };
    if !seen_whole {
        cx.reads().refresh(full, meta, hash);
        cx.reads().forget(full, LineRange::ALL);
        return;
    }
    // A file beyond the 2,000-line window is only partly shown by an open-ended `read`,
    // so no baseline: a read that did not see the rest must not be answered `unchanged`.
    let content = crate::spec::decode_text(bytes)
        .ok()
        .filter(|t| t.lines().count() <= MAX_READ_LINES)
        .map(|t| t.lines().collect::<Vec<_>>().join("\n"));
    match content {
        Some(content) => {
            cx.reads()
                .record(
                    sb,
                    full,
                    LineRange::ALL,
                    cx.edit_format(),
                    meta,
                    hash,
                    cx.turn(),
                    &content,
                )
                .await
        }
        None => {
            cx.reads().refresh(full, meta, hash);
            cx.reads().forget(full, LineRange::ALL);
        }
    }
}

/// Per-call context handed to tools. Fields are private so later tasks can add more.
pub struct ToolCx {
    edit_format: EditFormat,
    reads: Arc<ReadTracker>,
    spill_dir: PathBuf,
    cancel: CancellationToken,
    output_tx: Option<mpsc::Sender<String>>,
    index: Option<Arc<Index>>,
    memory: Option<Arc<RwLock<MemoryStore>>>,
    turn: u32,
    groups: ProcessGroups,
    approved_bases: Option<Vec<(PathBuf, Option<blake3::Hash>)>>,
    preview_diff: bool,
    side: Option<Arc<dyn SideModel>>,
}

impl std::fmt::Debug for ToolCx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolCx")
            .field("edit_format", &self.edit_format)
            .field("spill_dir", &self.spill_dir)
            .field("index", &self.index.is_some())
            .field("memory", &self.memory.is_some())
            .field("turn", &self.turn)
            .finish_non_exhaustive()
    }
}

impl ToolCx {
    pub fn new(edit_format: EditFormat, spill_dir: PathBuf) -> Self {
        Self {
            edit_format,
            reads: Arc::new(ReadTracker::default()),
            spill_dir,
            cancel: CancellationToken::new(),
            output_tx: None,
            index: None,
            memory: None,
            turn: 0,
            groups: ProcessGroups::default(),
            approved_bases: None,
            preview_diff: true,
            side: None,
        }
    }

    pub fn with_reads(mut self, reads: Arc<ReadTracker>) -> Self {
        self.reads = reads;
        self
    }

    pub fn with_cancel(mut self, cancel: CancellationToken) -> Self {
        self.cancel = cancel;
        self
    }

    pub fn with_output_tx(mut self, tx: mpsc::Sender<String>) -> Self {
        self.output_tx = Some(tx);
        self
    }

    /// The code index the index tools query and edits keep current.
    pub fn with_index(mut self, index: Arc<Index>) -> Self {
        self.index = Some(index);
        self
    }

    /// The project memory `recall` searches and `remember` validates drafts against.
    pub fn with_memory(mut self, memory: Arc<RwLock<MemoryStore>>) -> Self {
        self.memory = Some(memory);
        self
    }

    /// The 1-based turn this call belongs to.
    pub fn with_turn(mut self, turn: u32) -> Self {
        self.turn = turn;
        self
    }

    /// The session's registry of process groups (`bash` records each command's group).
    pub fn with_process_groups(mut self, groups: ProcessGroups) -> Self {
        self.groups = groups;
        self
    }

    /// The file hashes an approved [`Tool::preview`] started from: `run` then refuses with
    /// `changed_after_approval` if any file no longer has its hash.
    pub fn with_approved_bases(mut self, bases: Vec<(PathBuf, Option<blake3::Hash>)>) -> Self {
        self.approved_bases = Some(bases);
        self
    }

    /// [`Tool::preview`] skips computing the diff (left empty) and only says whether the
    /// call would succeed, with its bases: nobody will see the diff (headless `ask`).
    pub fn without_preview_diff(mut self) -> Self {
        self.preview_diff = false;
        self
    }

    /// The cheap side model (spec 7.5) for tools that answer questions about what they
    /// fetched (`web_fetch`). Its usage is counted by whoever built it.
    pub fn with_side(mut self, side: Arc<dyn SideModel>) -> Self {
        self.side = Some(side);
        self
    }

    pub fn edit_format(&self) -> EditFormat {
        self.edit_format
    }

    pub fn reads(&self) -> &Arc<ReadTracker> {
        &self.reads
    }

    pub fn spill_dir(&self) -> &Path {
        &self.spill_dir
    }

    pub fn cancel(&self) -> &CancellationToken {
        &self.cancel
    }

    pub fn output_tx(&self) -> Option<&mpsc::Sender<String>> {
        self.output_tx.as_ref()
    }

    pub fn index(&self) -> Option<&Arc<Index>> {
        self.index.as_ref()
    }

    pub fn memory(&self) -> Option<&Arc<RwLock<MemoryStore>>> {
        self.memory.as_ref()
    }

    pub fn turn(&self) -> u32 {
        self.turn
    }

    pub fn process_groups(&self) -> &ProcessGroups {
        &self.groups
    }

    pub fn approved_bases(&self) -> Option<&[(PathBuf, Option<blake3::Hash>)]> {
        self.approved_bases.as_deref()
    }

    pub fn preview_diff(&self) -> bool {
        self.preview_diff
    }

    pub fn side(&self) -> Option<&Arc<dyn SideModel>> {
        self.side.as_ref()
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    /// Built-in tools have fixed names; MCP tools (`mcp__<server>__<tool>`) do not.
    fn name(&self) -> &str;
    fn spec(&self, caps: &Capabilities) -> ToolSpec;
    /// The side-effect class of this call, used for permission checks before `run`.
    fn effect(&self, args: &Value) -> Result<Effect, ToolError>;
    /// Never panics on bad arguments: returns an error result instead.
    async fn run(&self, args: Value, sb: &dyn Sandbox, cx: &ToolCx) -> ToolResult;
    /// What `run` would change, computed the same way (stale guard, matching) without
    /// writing anything: None for tools that do not change files, `Err` with the result
    /// `run` would return when the call would fail.
    async fn preview(
        &self,
        _args: &Value,
        _sb: &dyn Sandbox,
        _cx: &ToolCx,
    ) -> Option<Result<Preview, ToolResult>> {
        None
    }
}

/// The tools available for a model: the built-in ones in a fixed order, then `extra`
/// (MCP tools). Requests list tools sorted by name, whatever this order.
pub fn registry(_caps: &Capabilities, extra: Vec<Arc<dyn Tool>>) -> Vec<Arc<dyn Tool>> {
    let builtin: Vec<Arc<dyn Tool>> = vec![
        Arc::new(ReadTool),
        Arc::new(EditTool),
        Arc::new(WriteTool),
        Arc::new(BashTool),
        Arc::new(GrepTool),
        Arc::new(GlobTool),
        Arc::new(FindTool),
        Arc::new(OutlineTool),
        Arc::new(SymbolTool),
        Arc::new(RefsTool),
        Arc::new(WebFetchTool),
        Arc::new(RecallTool),
        Arc::new(RememberTool),
        Arc::new(PinTool),
        Arc::new(StateTool),
    ];
    builtin.into_iter().chain(extra).collect()
}
