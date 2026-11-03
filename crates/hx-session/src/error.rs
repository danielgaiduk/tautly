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
    #[error("invalid blob reference {0:?}")]
    InvalidBlobRef(String),
    #[error("blob {0} not found")]
    BlobNotFound(String),
}

pub type Result<T> = std::result::Result<T, Error>;
