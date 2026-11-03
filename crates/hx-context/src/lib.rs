//! Context blocks, token accounting, pinned items and working state. The engine keeps one
//! [`Context`] per session and calls [`Context::assemble`] before every provider request.

pub mod agents_md;
pub mod assemble;
pub mod blocks;
pub mod pinned;
pub mod prompt;
pub mod repo_map;
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
    /// Generated once at session start ([`Context::init_repo_map`]) and never rebuilt, so
    /// the cached prefix stays byte-stable; rendered after the memory index.
    pub repo_map: Option<String>,
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

    fn flat_tokens(&self) -> u32 {
        self.history
            .iter()
            .map(|t| blocks::flat_tokens(&t.messages))
            .sum()
    }

    /// Calibrate token estimation: `estimated` is the `used` of a request assembled from the
    /// current history and `actual` the provider's input count for it. Images are a flat
    /// estimate the ratio never scaled, so their share is left out of the sample.
    pub fn calibrate(&mut self, estimated: u32, actual: u64) {
        let flat = self.flat_tokens();
        self.tokens.calibrate_excluding(estimated, actual, flat);
    }

    /// Build the repo map from `index` and keep it for the rest of the session. The engine
    /// calls this once at session start, off the startup path (see [`repo_map::build`]);
    /// [`Context::assemble`] only renders the stored text and never rebuilds it.
    pub fn init_repo_map(&mut self, index: &hx_index::Index) {
        let map = repo_map::build(index, &self.tokens, repo_map::BUDGET);
        self.repo_map = Some(map).filter(|m| !m.is_empty());
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
