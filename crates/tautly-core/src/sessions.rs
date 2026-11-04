//! Session store operations outside a running engine (spec 6.5): `tautly sessions
//! delete`, and the session `--continue` picks.

use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use tautly_sandbox::Sandbox;

use crate::checkpoint::{self, CheckpointError};
use crate::engine::EngineError;
use crate::record::SessionRecord;

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
    // Only a repository checkpoints would have written to (see `checkpoint::git_dir`).
    let refs = if checkpoint::git_dir(&root).is_some() {
        let excludes = log.parent().map(|d| d.join(checkpoint::EXCLUDES_DIR));
        checkpoint::delete_session_refs(sandbox, &root, id, excludes.as_deref()).await?
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
/// ([`session_root`](crate::session_root)) and has a user turn: what `--continue` resumes
/// and `tautly undo` works on. A session that never got a message (a client that opened
/// one and left, `tautly memory consolidate`) is not one. None when there is none.
pub fn latest_here(data_home: &Path, start: &Path) -> Result<Option<String>, EngineError> {
    Ok(latest_log_here(data_home, start)?.map(|s| s.id))
}

/// [`latest_here`] with the session's log.
pub fn latest_log_here(
    data_home: &Path,
    start: &Path,
) -> Result<Option<tautly_session::SessionSummary>, EngineError> {
    let root = crate::session_root(start)?;
    let here = tautly_session::list_here_in(data_home, &root)?;
    Ok(here.into_iter().find(|s| first_prompt(&s.path).is_some()))
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

/// Longest [`SessionSummary::first_prompt`], in characters.
pub const FIRST_PROMPT_CHARS: usize = 60;

/// One session of a work tree, as `/resume` lists it.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionSummary {
    pub id: String,
    /// When the session started.
    pub started: SystemTime,
    /// The first line of the first user message, at most [`FIRST_PROMPT_CHARS`] characters
    /// (empty: the session never got a message).
    pub first_prompt: String,
    /// What the session cost, from the cost ledger (none: no priced request).
    pub cost_usd: Option<f64>,
}

/// The newest `limit` sessions that ran in the work tree `project` is in (the ones
/// `--continue` and `/resume` may open; another worktree's sessions are not listed), under
/// [`tautly_session::paths::data_home`]. Sessions that cannot be read are left out; so is
/// everything when the data directory cannot be found. Blocking file I/O.
pub fn list(project: &Path, limit: usize) -> Vec<SessionSummary> {
    match tautly_session::paths::data_home() {
        Ok(home) => list_in(&home, project, limit),
        Err(_) => Vec::new(),
    }
}

/// [`list`] with an explicit data home.
pub fn list_in(data_home: &Path, project: &Path, limit: usize) -> Vec<SessionSummary> {
    let Ok(root) = crate::session_root(project) else {
        return Vec::new();
    };
    let Ok(found) = tautly_session::list_here_in(data_home, &root) else {
        return Vec::new();
    };
    // Not created by a listing: a data home that never ran a session has no ledger.
    let ledger_file = data_home.join(crate::ledger::LEDGER_FILE);
    let ledger = ledger_file
        .exists()
        .then(|| crate::Ledger::open(&ledger_file).ok())
        .flatten();
    // A session without a user turn (opened and left) is not listed.
    found
        .into_iter()
        .filter_map(|s| first_prompt(&s.path).map(|p| (s, p)))
        .take(limit)
        .map(|(s, first_prompt)| {
            let cost_usd = ledger
                .as_ref()
                .and_then(|l| l.session_totals(&s.id).ok())
                .filter(|t| t.requests > t.unpriced)
                .map(|t| t.usd);
            SessionSummary {
                first_prompt,
                id: s.id,
                started: s.created,
                cost_usd,
            }
        })
        .collect()
}

/// The first line of the log's first user message, cut to [`FIRST_PROMPT_CHARS`]; none when
/// the log has no user turn (or cannot be read).
fn first_prompt(log: &Path) -> Option<String> {
    let reader = tautly_session::SessionReader::<SessionRecord>::open(log).ok()?;
    for rec in reader.records() {
        let Ok((_, rec)) = rec else {
            break;
        };
        let text = match rec {
            SessionRecord::TurnStart { text, .. } => text,
            SessionRecord::Command(crate::Command::SendMessage { text }) => text,
            _ => continue,
        };
        let line = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
        return Some(line.trim().chars().take(FIRST_PROMPT_CHARS).collect());
    }
    None
}
