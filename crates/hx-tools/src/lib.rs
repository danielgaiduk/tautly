//! The `Tool` trait, shared tool context, and the built-in read-side tools.

mod bash;
mod cap;
pub mod edit;
mod glob;
mod grep;
mod index_tools;
mod pin_state;
mod read;
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

/// Remembers the mtime and content hash of each file the model has read or written.
#[derive(Debug, Default)]
pub struct ReadTracker {
    seen: Mutex<HashMap<PathBuf, (SystemTime, blake3::Hash)>>,
}

impl ReadTracker {
    pub fn record(&self, path: &Path, meta: &FileMeta, hash: blake3::Hash) {
        let mut seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
        seen.insert(path.to_path_buf(), (meta.mtime, hash));
    }

    pub fn last(&self, path: &Path) -> Option<(SystemTime, blake3::Hash)> {
        let seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
        seen.get(path).copied()
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
        Arc::new(OutlineTool),
        Arc::new(SymbolTool),
        Arc::new(RefsTool),
        Arc::new(WebFetchTool),
        Arc::new(PinTool),
        Arc::new(StateTool),
    ]
}
