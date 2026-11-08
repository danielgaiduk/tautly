use std::path::PathBuf;

/// Errors from the session store.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Reading or writing a file failed.
    #[error("session i/o error: {0}")]
    Io(#[from] std::io::Error),
    /// A JSON value does not (de)serialize.
    #[error("session json error: {0}")]
    Json(#[from] serde_json::Error),
    /// The first line is not a valid session header.
    #[error("{}: missing or invalid header: {reason}", path.display())]
    BadHeader {
        /// The log file.
        path: PathBuf,
        /// What is wrong with it.
        reason: String,
    },
    /// A record line that does not parse.
    #[error("{}: corrupt record at line {line}: {reason}", path.display())]
    Corrupt {
        /// The log file.
        path: PathBuf,
        /// The record's line number.
        line: u64,
        /// What is wrong with it.
        reason: String,
    },
    /// Another writer holds the log's lock.
    #[error("{} is already open for writing by another writer", .0.display())]
    AlreadyOpen(PathBuf),
    /// An earlier write failed, so the log's end is unknown.
    #[error("session writer is poisoned by an earlier i/o error; reopen the session")]
    Poisoned,
    /// The log was written by a newer version.
    #[error("{}: schema_version {found} is newer than the supported {supported}", path.display())]
    UnsupportedSchema {
        /// The log file.
        path: PathBuf,
        /// The header's `schema_version`.
        found: u32,
        /// The newest version this build reads.
        supported: u32,
    },
    /// A blob whose bytes do not hash to its name.
    #[error("blob {0} does not match its content hash")]
    BlobCorrupt(String),
    /// A blob reference that is not a valid hash.
    #[error("invalid blob reference {0:?}")]
    InvalidBlobRef(String),
    /// No blob of that hash is stored.
    #[error("blob {0} not found")]
    BlobNotFound(String),
    /// A string that cannot be a session id.
    #[error("{0:?} is not a session id")]
    InvalidSessionId(String),
    /// A root that is not in the project the log belongs to.
    #[error(
        "{} is not in the project of the session log {}",
        root.display(),
        log.display()
    )]
    OtherProject {
        /// The root given.
        root: PathBuf,
        /// The session log.
        log: PathBuf,
    },
    /// The data directory cannot be found or used.
    #[error("no usable data directory: {0}")]
    DataHome(String),
}

/// A result with this crate's [`Error`].
pub type Result<T> = std::result::Result<T, Error>;
