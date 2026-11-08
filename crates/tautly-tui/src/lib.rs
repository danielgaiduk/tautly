//! Tautly's daily-driver REPL (spec 7.11.8): a plain line interface over one engine.
//!
//! [`run_repl`] reads lines from a [`LineSource`] (the terminal through `reedline`, or a
//! script in tests), sends them as messages or slash commands, and prints what the engine
//! answers: streamed text, one `thinking…` line per step, one compact row per tool call,
//! coloured diffs, approval prompts, and a receipt footer after each turn. Output is plain
//! lines (no alternate screen), and [`render::event`] is a pure function of the events and
//! a [`render::RenderState`], so nothing here needs a terminal to be tested.
//!
//! Everything the model, a tool, the repository or an MCP server wrote is escaped before
//! it is printed (spec 3.4): control characters never reach the terminal.

use std::path::PathBuf;
use std::sync::Arc;

use tautly_core::{Engine, EngineError, Event};
use tokio::sync::mpsc;

pub mod approve;
pub mod input;
pub mod render;
mod repl;
pub mod slash;

pub use input::{Line, LineSource, ReedlineSource, ScriptedSource};
pub use repl::run_repl;

/// Builds the session the REPL talks to: a new one (`None`) or the session to resume. It
/// may block (configuration files, the keychain); the REPL calls it off the async workers.
pub type EngineFactory = Box<
    dyn Fn(Option<String>) -> Result<(Engine, mpsc::Receiver<Event>), EngineError> + Send + Sync,
>;

/// The models `/model` offers: those whose provider has a key. Called when the user asks
/// (it may read the keychain), off the async workers.
pub type ModelLister = Arc<dyn Fn() -> Vec<String> + Send + Sync>;

/// How the REPL looks and where it keeps things.
#[derive(Clone, Default)]
pub struct ReplOptions {
    /// ANSI colour: on when stdout is a terminal and `NO_COLOR` is unset.
    pub color: bool,
    /// The receipt footer after each turn (`[receipts] footer`).
    pub footer: bool,
    /// The history file (`<data home>/repl_history`); none keeps no history.
    pub history: Option<PathBuf>,
    /// Where the session runs: `/resume` lists this work tree's sessions.
    pub root: PathBuf,
    /// The data home holding session logs and the cost ledger (`/resume`, and the
    /// session total of a resumed session); none disables both.
    pub data_home: Option<PathBuf>,
    /// The model the session starts on, as `/model` shows it until the engine names
    /// another.
    pub model: Option<String>,
    /// What `/model` lists besides the current model.
    pub models: Option<ModelLister>,
}

/// What stops the REPL with an error.
#[derive(Debug, thiserror::Error)]
pub enum ReplError {
    /// The terminal could not be read or written.
    #[error("terminal: {0}")]
    Io(#[from] std::io::Error),
    /// The engine failed.
    #[error("{0}")]
    Engine(#[from] EngineError),
}
