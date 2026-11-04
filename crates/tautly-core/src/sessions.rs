//! Session store operations outside a running engine (spec 6.5): `tautly sessions
//! delete`, and the session `--continue` picks.

use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use tautly_sandbox::Sandbox;

use crate::checkpoint::{self, CheckpointError};
use crate::engine::EngineError;

/// What [`delete`] removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deleted {
    /// The session log.
    pub log: PathBuf,
    /// How many checkpoint refs.
    pub refs: u32,
}

#[derive(Debug, thiserror::Error)]
pub enum DeleteError {
    #[error("no session {0} in this project")]
    NotFound(String),
    #[error("session {0} is running; stop it first")]
    Running(String),
    #[error("cannot read the sessions: {0}")]
    Session(#[from] tautly_session::Error),
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("cannot delete the session's checkpoint refs: {0}")]
    Checkpoint(#[from] CheckpointError),
}

/// Delete session `id` of the project the sandbox root belongs to (`tautly sessions
/// delete <id>`): its checkpoint refs (`refs/tautly/checkpoints/<id>/`, when the root is in
/// a git work tree), then its log and last-request file. Blobs are shared by the project's
/// sessions and kept (blob garbage collection is post-v1); so are its branches, whose logs
/// hold their own copies and whose checkpoints have refs of their own. A session an engine
/// holds open is refused. (Removing a worktree fork's worktree, spec 7.12.4, comes with
/// those forks.)
pub async fn delete(
    data_home: &Path,
    sandbox: &dyn Sandbox,
    id: &str,
) -> Result<Deleted, DeleteError> {
    let root = sandbox.root().to_path_buf();
    let found = {
        let (home, root) = (data_home.to_path_buf(), root.clone());
        tokio::task::spawn_blocking(move || tautly_session::list_in(&home, &root))
            .await
            .map_err(|e| DeleteError::Io {
                path: data_home.to_path_buf(),
                source: std::io::Error::other(e),
            })??
    };
    let log = found
        .into_iter()
        .find(|s| s.id == id)
        .map(|s| s.path)
        .ok_or_else(|| DeleteError::NotFound(id.to_string()))?;
    // Held until the log is gone, so no engine opens it meanwhile.
    let lock = lock_if_idle(&log, id)?;
    let refs = if std::fs::symlink_metadata(root.join(".git")).is_ok() {
        checkpoint::delete_session_refs(sandbox, &root, id).await?
    } else {
        0
    };
    std::fs::remove_file(&log).map_err(|source| DeleteError::Io {
        path: log.clone(),
        source,
    })?;
    let _ = std::fs::remove_file(log.with_extension("last-request.json"));
    drop(lock);
    Ok(Deleted { log, refs })
}

/// The newest session that ran in the work tree a session started in `start` runs on
/// ([`session_root`](crate::session_root)): what `--continue` resumes. None when there is
/// none.
pub fn latest_here(data_home: &Path, start: &Path) -> Result<Option<String>, EngineError> {
    let root = crate::session_root(start)?;
    let here = tautly_session::list_here_in(data_home, &root)?;
    Ok(here.into_iter().next().map(|s| s.id))
}

/// The log, opened and locked exclusively (as an engine's writer does); refused when it is
/// held.
fn lock_if_idle(log: &Path, id: &str) -> Result<std::fs::File, DeleteError> {
    let io = |source| DeleteError::Io {
        path: log.to_path_buf(),
        source,
    };
    let file = std::fs::OpenOptions::new()
        .read(true)
        .open(log)
        .map_err(io)?;
    // SAFETY: flock on a file descriptor we own; no memory is shared.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let err = std::io::Error::last_os_error();
        return Err(match err.raw_os_error() {
            Some(libc::EWOULDBLOCK) => DeleteError::Running(id.to_string()),
            _ => io(err),
        });
    }
    Ok(file)
}
