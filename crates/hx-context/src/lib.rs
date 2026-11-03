//! Context blocks, token accounting, pinned items and working state. The engine keeps one
//! [`Context`] per session and calls [`Context::assemble`] before every provider request.

pub mod agents_md;
pub mod assemble;
pub mod blocks;
pub mod compact;
pub mod gc;
pub mod pinned;
pub mod prompt;
pub mod repo_map;
pub mod sanitize;
pub mod schedule;
pub mod state;
pub mod tokens;

use std::path::PathBuf;
use std::time::Instant;

use hx_tools::{AppliedEffect, ContextOp};

pub use assemble::STABLE_LEN;
pub use blocks::{Breakdown, ContextStatus, Turn};
pub use compact::{CompactError, CompactionRecord, UncompactError};
pub use gc::{GcRecord, ReadTarget, ResultMeta};
pub use pinned::{PinSource, Pinned};
pub use schedule::{CacheClock, MutationReason};
pub use state::{FileNote, WorkingState};
pub use tokens::TokenEstimator;

/// Everything the engine assembles into a request.
#[derive(Debug, Clone)]
pub struct Context {
    pub pinned: Vec<Pinned>,
    pub state: WorkingState,
    pub history: Vec<Turn>,
    /// Absolute session turn number of `history[0]`; turns are 1-based like the read
    /// tracker's (`ToolCx::with_turn`), so a fresh session has 1. Compaction keeps
    /// `history[i]` at turn `first_turn + i`, see [`CompactionRecord`] and
    /// [`Context::next_turn`].
    pub first_turn: u32,
    pub agents_md: Option<String>,
    /// Set by the engine from the memory store; rendered in the stable prefix after AGENTS.md.
    pub memory_index: Option<String>,
    /// Generated once at session start ([`Context::init_repo_map`]) and never rebuilt, so
    /// the cached prefix stays byte-stable; rendered after the memory index.
    pub repo_map: Option<String>,
    pub tokens: TokenEstimator,
    /// Project root; file paths in the working state are displayed relative to it.
    pub root: Option<PathBuf>,
    /// One entry per tool result recorded with [`Context::push_tool_result`], oldest first;
    /// the facts [`Context::collect`] decides on.
    pub results: Vec<ResultMeta>,
    /// When the provider last saw the prefix, see [`Context::note_request`].
    pub clock: CacheClock,
    /// Problems found while loading the session's inputs (an AGENTS.md that was truncated
    /// or refused), repeated in the warnings of every [`ContextStatus`].
    pub load_warnings: Vec<String>,
}

impl Default for Context {
    fn default() -> Self {
        Self {
            pinned: Vec::new(),
            state: WorkingState::default(),
            history: Vec::new(),
            first_turn: 1,
            agents_md: None,
            memory_index: None,
            repo_map: None,
            tokens: TokenEstimator::default(),
            root: None,
            results: Vec::new(),
            clock: CacheClock::default(),
            load_warnings: Vec::new(),
        }
    }
}

impl Context {
    /// Take what [`agents_md::load`] found: its text for the `<agents_md>` block, its pins
    /// (added to the session-start pins) and its warnings (shown in every status).
    pub fn set_agents_md(&mut self, found: agents_md::AgentsMd) {
        self.agents_md = found.text;
        for pin in found.pins {
            pinned::add(&mut self.pinned, pin);
        }
        self.load_warnings.extend(found.warnings);
    }

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
    /// [`Context::assemble`] only renders the stored text and never rebuilds it. The map is
    /// set once: a later call changes nothing and returns false, as does a call that finds
    /// nothing to map yet (an empty workspace), which leaves the slot free. The budget is
    /// for the whole rendered block, tag wrapper included.
    pub fn init_repo_map(&mut self, index: &hx_index::Index) -> bool {
        if self.repo_map.is_some() {
            return false;
        }
        let wrapper = self
            .tokens
            .estimate(&assemble::tagged("repo_map", Some("x")));
        // One more for the rounding of the two estimates.
        let budget = repo_map::BUDGET.saturating_sub(wrapper + 1);
        let map = repo_map::build(index, &self.tokens, budget);
        if map.is_empty() {
            return false;
        }
        self.repo_map = Some(map);
        true
    }

    /// Record that a provider request went out at `at` whose cache-marked prefix (up to the
    /// last cache breakpoint) was `cached_prefix_tokens` long. The scheduler measures the
    /// cache's age from `at`.
    pub fn note_request(&mut self, at: Instant, cached_prefix_tokens: u32) {
        self.clock = CacheClock {
            last_request: Some(at),
            cached_prefix_tokens,
        };
    }

    /// Apply a `pin` or `state` tool call. A `remember` proposal changes nothing here: the
    /// engine takes it to the user (`MemoryProposed`) and the memory store, not the context.
    pub fn apply_op(&mut self, op: ContextOp) {
        match op {
            ContextOp::Pin(text) => {
                pinned::add(&mut self.pinned, Pinned::new(text, PinSource::Model))
            }
            ContextOp::StateUpdate(patch) => self.state.apply_patch(patch),
            ContextOp::Remember(_) => {}
        }
    }
}
