//! Incremental tree-sitter symbol and reference index (spec 7.1).
//!
//! [`Index`] keeps a SQLite database per work tree (under the data home) with the definitions
//! ([`SymbolRow`]) and references ([`RefRow`]) of Rust, TypeScript, JavaScript, Python and
//! Go files, a file tree ([`TreeView`]) and a trigram index for text search. It is built in
//! the background, refreshed from a file watcher and after the agent's own edits, and
//! queried by the `outline`, `symbol`, `refs`, `find` and `read` tools.

pub mod extract;
mod fresh;
mod lang;
mod peek;
mod store;
mod tree;
mod trigram;
mod update;
mod watch;

pub use extract::{Extracted, extract};
pub use lang::Lang;
pub use peek::{IndexPeek, peek};
pub use store::{
    Index, IndexError, IndexStats, RefRow, RefreshHold, RefreshStats, SymbolKind, SymbolRow,
};
pub use tree::{DirInfo, FileEntry, TreeView};
pub use trigram::Candidates;
pub use watch::WatchHandle;
