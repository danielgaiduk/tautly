//! `tautly acp` (spec 7.11.9): an Agent Client Protocol agent over stdio, so editors (Zed,
//! later VS Code and the desktop app) drive Tautly sessions. Like the JSON-RPC server it
//! is a plain engine client: it speaks `tautly-core`'s `Command` and `Event`, never a
//! layer on top of `tautly-rpc` (whose line framing it reuses).
//!
//! - `initialize` answers protocol version [`ACP_PROTOCOL_VERSION`] whatever the client
//!   asks for, with `loadSession: true`, embedded-context prompts and no auth methods
//!   (keys come from the environment or the keychain).
//! - `session/new {cwd, mcpServers}` opens an engine at the git top level of `cwd`, through
//!   the [`AcpEngineFactory`]; the editor's MCP servers are trusted like the user's own.
//!   `session/load` resumes a session and replays its messages and tool calls first.
//! - `session/prompt` is a user message (or `/compact`, `/undo`, `/diff`, `/cost`,
//!   `/pin`), streamed back as `session/update`s, and answered with the stop reason when
//!   the turn ends. Approvals become `session/request_permission`; `allow_always` sends the
//!   exact-target session rule. `session/cancel` cancels the turn.
//! - `session/set_config_option {configId: "model"}` switches the model.
//!
//! Nothing but protocol frames is written to the output; diagnostics go to stderr.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tautly_core::{Engine, EngineError, Event, ServerSpec, Workspace, Workspaces};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;

pub mod bridge;
pub mod map;
pub mod permission;
pub mod schema;

pub use bridge::Bridge;

/// The ACP wire `protocolVersion` this bridge speaks.
pub const ACP_PROTOCOL_VERSION: u16 = 1;

/// Opens a session: the work tree root (the git top level of the client's `cwd`), the
/// session to resume (`session/load`), and the client's MCP servers (trusted). Called on
/// the blocking pool (building may read the keychain). An interactive engine is expected:
/// the bridge answers its approval requests.
pub type AcpEngineFactory = Arc<
    dyn Fn(
            PathBuf,
            Option<String>,
            Vec<(String, ServerSpec)>,
        ) -> Result<(Engine, mpsc::Receiver<Event>), EngineError>
        + Send
        + Sync,
>;

/// The models a session on a root can switch to: the one it starts on and those whose
/// provider has a key. Called on the blocking pool (it may read the keychain).
pub type ModelLister = Arc<dyn Fn(&Path) -> Option<ModelChoices> + Send + Sync>;

/// What the `model` config option offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelChoices {
    /// The model a new session on the root runs on.
    pub current: String,
    /// Models whose provider has a key (the current one is always offered).
    pub available: Vec<String>,
}

/// What ends a bridge with an error.
#[derive(Debug, thiserror::Error)]
pub enum AcpError {
    #[error("cannot read the client's input: {0}")]
    Input(std::io::Error),
}

/// Serve ACP on `input`/`output` with sessions from `factory`, until the input ends (every
/// session is then shut down and its last events written).
pub async fn serve(
    factory: AcpEngineFactory,
    input: impl AsyncRead + Unpin,
    output: impl AsyncWrite + Unpin,
) -> Result<(), AcpError> {
    Bridge::new(factory).serve(input, output).await
}

/// A factory that hands every session on one root the same [`Workspace`] (code index,
/// watcher, memory store; spec 3.4) from `workspaces`, and builds the engine with `build`
/// (root, resume, client MCP servers, workspace).
pub fn shared_factory<F>(workspaces: Workspaces, build: F) -> AcpEngineFactory
where
    F: Fn(
            PathBuf,
            Option<String>,
            Vec<(String, ServerSpec)>,
            Arc<Workspace>,
        ) -> Result<(Engine, mpsc::Receiver<Event>), EngineError>
        + Send
        + Sync
        + 'static,
{
    Arc::new(move |root, resume, mcp| {
        let workspace = workspaces.get(&root)?;
        build(root, resume, mcp, workspace)
    })
}
