//! Incremental tree-sitter symbol and reference index (spec 7.1).

pub mod extract;
mod lang;
mod store;
mod tree;
mod trigram;
mod update;
mod watch;

pub use extract::{Extracted, extract};
pub use lang::Lang;
pub use store::{Index, IndexError, IndexStats, RefRow, RefreshStats, SymbolKind, SymbolRow};
pub use tree::{FileEntry, TreeView};
pub use trigram::Candidates;
pub use watch::WatchHandle;
