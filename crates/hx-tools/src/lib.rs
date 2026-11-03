//! The `Tool` trait, shared tool context, and the built-in read-side tools.

mod bash;
mod cap;
pub mod edit;
pub mod filters;
mod find;
mod glob;
mod grep;
mod index_tools;
mod pin_state;
mod read;
mod reread;
mod spec;
mod web_fetch;
mod write;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use async_trait::async_trait;
use hx_index::Index;
use hx_provider::{Capabilities, EditFormat, ToolSpec};
use hx_sandbox::{Effect, FileMeta, Sandbox};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub use bash::{BashTool, cap_output};
pub use cap::cap_text;
pub use edit::EditTool;
pub use find::FindTool;
pub use glob::GlobTool;
pub use grep::GrepTool;
pub use index_tools::{OutlineTool, RefsTool, SymbolTool};
pub use pin_state::{
    ContextOp, PinTool, PlanStep, StatePatch, StateTool, StepStatus, parse_context_op,
};
pub use read::ReadTool;
pub use web_fetch::WebFetchTool;
pub use write::WriteTool;

/// Invalid tool arguments.
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("invalid arguments: {0}")]
    BadArgs(String),
}

/// A side effect a tool actually applied, reported to the engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppliedEffect {
    Read(PathBuf, blake3::Hash),
    Edited {
        path: PathBuf,
        pre_image: Option<Vec<u8>>,
        post_hash: blake3::Hash,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResult {
    pub output: String,
    pub is_error: bool,
    pub effects: Vec<AppliedEffect>,
}

impl ToolResult {
    pub fn ok(output: impl Into<String>) -> Self {
        Self {
            output: output.into(),
            is_error: false,
            effects: Vec::new(),
        }
    }

    pub fn error(output: impl Into<String>) -> Self {
        Self {
            output: output.into(),
            is_error: true,
            effects: Vec::new(),
        }
    }
}

/// The lines of a file a read asked for: 1-based, inclusive, `None` = open end.
/// Two reads are "the same read" when their ranges are equal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
}

/// What the tracker remembers of the last read of one path and range. The content itself
/// lives on disk at `content_path` (a sandbox-relative spill file).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrevRead {
    /// Hash of the whole file at that time.
    pub hash: blake3::Hash,
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
    /// the spill fails the range is simply not remembered, so the next read is full.
    #[allow(clippy::too_many_arguments)]
    pub async fn record(
        &self,
        sb: &dyn Sandbox,
        path: &Path,
        range: LineRange,
        meta: &FileMeta,
        hash: blake3::Hash,
        turn: u32,
        content: &str,
    ) {
        self.refresh(path, meta, hash);
        let key = (path.to_path_buf(), range);
        match sb.spill(content.as_bytes()).await {
            Ok(content_path) => {
                let prev = PrevRead {
                    hash,
                    content_hash: blake3::hash(content.as_bytes()),
                    turn,
                    mtime: meta.mtime,
                    content_path,
                };
                let mut ranges = self.ranges.lock().unwrap_or_else(|e| e.into_inner());
                ranges.insert(key, prev);
            }
            Err(_) => {
                let mut ranges = self.ranges.lock().unwrap_or_else(|e| e.into_inner());
                ranges.remove(&key);
            }
        }
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

    /// The last read of exactly this `range` of `path`, if its content was kept.
    pub fn previous(&self, path: &Path, range: LineRange) -> Option<PrevRead> {
        let ranges = self.ranges.lock().unwrap_or_else(|e| e.into_inner());
        ranges.get(&(path.to_path_buf(), range)).cloned()
    }
}

/// Most lines a read returns when no end line is given.
pub(crate) const MAX_READ_LINES: usize = 2000;

/// Record the post-edit state of `full` (`bytes` as just written) so a later read of the
/// whole file is diffed against what the edit left, not against the pre-edit read.
pub(crate) async fn record_written_file(
    sb: &dyn Sandbox,
    cx: &ToolCx,
    full: &Path,
    meta: &FileMeta,
    bytes: &[u8],
) {
    let hash = blake3::hash(bytes);
    // Hashline edits move line numbers and the model has not seen the new anchors, so a
    // read after one must return the anchored lines, never `unchanged`: keep the older
    // baselines (a changed range then comes back in full) and only refresh the guard.
    if cx.edit_format() == EditFormat::Hashline {
        cx.reads().refresh(full, meta, hash);
        return;
    }
    // `read` shows only the first 2,000 lines of an open-ended range, so that is what a
    // whole-file baseline holds.
    let window = crate::spec::decode_text(bytes).map(|t| {
        t.lines()
            .take(MAX_READ_LINES)
            .collect::<Vec<_>>()
            .join("\n")
    });
    match window {
        Ok(content) => {
            let reads = cx.reads();
            reads
                .record(sb, full, LineRange::ALL, meta, hash, cx.turn(), &content)
                .await
        }
        Err(_) => cx.reads().refresh(full, meta, hash),
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
    turn: u32,
}

impl std::fmt::Debug for ToolCx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolCx")
            .field("edit_format", &self.edit_format)
            .field("spill_dir", &self.spill_dir)
            .field("index", &self.index.is_some())
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
            turn: 0,
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

    /// The 1-based turn this call belongs to.
    pub fn with_turn(mut self, turn: u32) -> Self {
        self.turn = turn;
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

    pub fn turn(&self) -> u32 {
        self.turn
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn spec(&self, caps: &Capabilities) -> ToolSpec;
    /// The side-effect class of this call, used for permission checks before `run`.
    fn effect(&self, args: &Value) -> Result<Effect, ToolError>;
    /// Never panics on bad arguments: returns an error result instead.
    async fn run(&self, args: Value, sb: &dyn Sandbox, cx: &ToolCx) -> ToolResult;
}

/// The tools available for a model, in a fixed order.
pub fn registry(_caps: &Capabilities) -> Vec<Arc<dyn Tool>> {
    vec![
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
        Arc::new(PinTool),
        Arc::new(StateTool),
    ]
}
