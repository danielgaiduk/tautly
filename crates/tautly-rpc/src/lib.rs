//! Tautly's headless protocol (spec 3.4): a JSON-RPC 2.0 server over newline-delimited
//! JSON on stdio ([`serve`], [`Server`]), where commands are methods and events are `event`
//! notifications, and the `tautly -p` one-shot mode ([`run_oneshot`]). Both are plain
//! engine clients: they speak only `tautly-core`'s `Command` and `Event`.

pub mod jsonrpc;
pub mod oneshot;
pub mod server;

pub use oneshot::{OutputFormat, exit_code, run_oneshot, run_oneshot_to};
pub use server::{EngineFactory, FactoryError, Server, SessionSpec, command_for, serve};

/// What ends a server with an error.
#[derive(Debug, thiserror::Error)]
pub enum RpcError {
    /// Reading the client's input failed.
    #[error("cannot read the client's input: {0}")]
    Input(std::io::Error),
}
