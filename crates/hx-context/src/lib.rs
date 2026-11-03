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

use hx_provider::RawUsage;
use hx_tools::{AppliedEffect, ContextOp};

pub use assemble::STABLE_LEN;
pub use blocks::{Breakdown, ContextStatus, ReasoningWire, Turn};
pub use compact::{CompactError, CompactionRecord, UncompactError};
pub use gc::{GcElision, GcRecord, ReadTarget, ResultMeta};
pub use pinned::{PinError, PinSource, Pinned};
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
    /// Pins added after session start ([`Context::pin`], the model's `pin` calls). They
    /// travel in the uncached state tail as `<pinned_this_session>`, so a pin never
    /// rewrites the cached history, until the engine folds them into `pinned` at the next
    /// cache epoch ([`Context::fold_session_pins`]).
    pub session_pins: Vec<Pinned>,
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
            session_pins: Vec::new(),
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
    pub fn apply_effects(&mut self, effects: &[AppliedEffect], turn: u32) {
        self.state.apply_effects(effects, turn);
    }

    /// Calibrate token estimation after a request: `status` is what [`assemble`](Self::assemble)
    /// returned for it and `usage` the provider's usage for the same request. The
    /// provider's count of the whole request is `input + cache_read + cache_write`: every
    /// adapter reports `input` without the cached tokens, which with a warm cache is only
    /// the newest few thousand. Images are a flat estimate the ratio never scales, so the
    /// flat tokens the request contained (`status.flat_tokens`) are left out of the sample.
    pub fn calibrate(&mut self, status: &ContextStatus, usage: &RawUsage) {
        let actual = usage
            .input
            .saturating_add(usage.cache_read)
            .saturating_add(usage.cache_write);
        self.tokens
            .calibrate_excluding(status.used, actual, status.flat_tokens);
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
    /// last cache breakpoint) was `cached_prefix_tokens` long: the request's
    /// [`ContextStatus::cached_prefix`]. The scheduler measures the cache's age from `at`.
    pub fn note_request(&mut self, at: Instant, cached_prefix_tokens: u32) {
        self.clock = CacheClock {
            last_request: Some(at),
            cached_prefix_tokens,
        };
    }

    /// Apply a `pin` or `state` tool call. A pin becomes a session pin ([`Context::pin`]);
    /// an oversize or empty one is refused. A `remember` proposal changes nothing here: the
    /// engine takes it to the user (`MemoryProposed`) and the memory store, not the context.
    pub fn apply_op(&mut self, op: ContextOp) -> Result<(), PinError> {
        match op {
            ContextOp::Pin(text) => {
                self.pin(text, PinSource::Model)?;
            }
            ContextOp::StateUpdate(patch) => self.state.apply_patch(patch),
            ContextOp::Remember(_) => {}
        }
        Ok(())
    }

    /// Pin `text` mid-session: it joins [`session_pins`](Self::session_pins) and is shown
    /// in the state tail until the next cache epoch. False when the same text is already
    /// pinned. Refused when empty or over [`pinned::MAX_PIN_BYTES`] (trimmed), which the
    /// `pin` tool refuses as well, and when [`pinned::MAX_SESSION_PINS`] session pins are
    /// already waiting. The `pin` tool cannot know the count, so the engine reports a
    /// refused model pin back to the model (as that call's result). Session-start pins
    /// (config, AGENTS.md, the user's startup pins) go into `pinned` directly.
    pub fn pin(&mut self, text: impl Into<String>, source: PinSource) -> Result<bool, PinError> {
        let text = text.into();
        let bytes = text.trim().len();
        if bytes == 0 {
            return Err(PinError::Empty);
        }
        if bytes > pinned::MAX_PIN_BYTES {
            return Err(PinError::TooLong {
                bytes,
                max: pinned::MAX_PIN_BYTES,
            });
        }
        if self
            .pinned
            .iter()
            .chain(&self.session_pins)
            .any(|p| p.text == text)
        {
            return Ok(false);
        }
        if self.session_pins.len() >= pinned::MAX_SESSION_PINS {
            return Err(PinError::TooMany {
                max: pinned::MAX_SESSION_PINS,
            });
        }
        self.session_pins.push(Pinned::new(text, source));
        Ok(true)
    }

    /// Move the session pins into `pinned`, where they become part of the cached prefix.
    /// That rewrites the first message and so the whole cached history, so the engine calls
    /// this only at a step start where nothing cached is lost or the prefix breaks anyway:
    /// when it applies a collection or compaction the scheduler allowed, and whenever the
    /// cache is cold ([`MutationReason::ColdCache`]: no request within the cache's TTL),
    /// even if nothing else is applied then. True when anything moved.
    pub fn fold_session_pins(&mut self) -> bool {
        if self.session_pins.is_empty() {
            return false;
        }
        for pin in std::mem::take(&mut self.session_pins) {
            pinned::add(&mut self.pinned, pin);
        }
        true
    }
}
