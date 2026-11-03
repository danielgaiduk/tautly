//! Context blocks, token accounting, pinned items and working state. The engine keeps one
//! [`Context`] per session and calls [`Context::assemble`] before every provider request.

pub mod agents_md;
pub mod assemble;
pub mod blocks;
pub mod pinned;
pub mod prompt;
pub mod state;
pub mod tokens;

use std::path::PathBuf;

use hx_tools::{AppliedEffect, ContextOp};

pub use assemble::STABLE_LEN;
pub use blocks::{Breakdown, ContextStatus, Turn};
pub use pinned::{PinSource, Pinned};
pub use state::{FileNote, WorkingState};
pub use tokens::TokenEstimator;

/// Everything the engine assembles into a request.
#[derive(Debug, Clone, Default)]
pub struct Context {
    pub pinned: Vec<Pinned>,
    pub state: WorkingState,
    pub history: Vec<Turn>,
    pub agents_md: Option<String>,
    /// Set by the engine from the memory store; rendered in the stable prefix after AGENTS.md.
    pub memory_index: Option<String>,
    pub tokens: TokenEstimator,
    /// Project root; file paths in the working state are displayed relative to it.
    pub root: Option<PathBuf>,
}

impl Context {
    /// Record what tools actually did to files. The files list is maintained from these
    /// effects only, never from what the model says.
    pub fn apply_effects(&mut self, effects: &[AppliedEffect]) {
        self.state.apply_effects(effects);
    }

    /// Apply a `pin` or `state` tool call.
    pub fn apply_op(&mut self, op: ContextOp) {
        match op {
            ContextOp::Pin(text) => {
                pinned::add(&mut self.pinned, Pinned::new(text, PinSource::Model))
            }
            ContextOp::StateUpdate(patch) => self.state.apply_patch(patch),
        }
    }
}
