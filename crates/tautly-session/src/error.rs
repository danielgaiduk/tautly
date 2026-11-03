use std::path::PathBuf;

/// Errors from the session store.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("session i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("session json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{}: missing or invalid header: {reason}", path.display())]
    BadHeader { path: PathBuf, reason: String },
    #[error("{}: corrupt record at line {line}: {reason}", path.display())]
    Corrupt {
        path: PathBuf,
        line: u64,
        reason: String,
    },
    #[error("{} is already open for writing by another writer", .0.display())]
    AlreadyOpen(PathBuf),
    #[error("session writer is poisoned by an earlier i/o error; reopen the session")]
    Poisoned,
    #[error("{}: schema_version {found} is newer than the supported {supported}", path.display())]
    UnsupportedSchema {
        path: PathBuf,
        found: u32,
        supported: u32,
    },
    #[error("blob {0} does not match its content hash")]
    BlobCorrupt(String),
    #[error("invalid blob reference {0:?}")]
    InvalidBlobRef(String),
    #[error("blob {0} not found")]
    BlobNotFound(String),
    #[error("no usable data directory: {0}")]
    DataHome(String),
}

pub type Result<T> = std::result::Result<T, Error>;
