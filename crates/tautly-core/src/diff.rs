//! `Command::Diff` (spec 7.11.4): the net change per file that the session's tools made,
//! from the session log's `Edit` records (each holds the file's content before that
//! change) and the disk as it is now. Changes made by commands are not included.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use tautly_sandbox::{Sandbox, SandboxError};
use tautly_session::{BlobRef, SessionReader};
use tautly_tools::preview::{cap, file_diff};

use crate::protocol::{Command, DiffFile, DiffStatus, ErrorKind, Event};
use crate::record::SessionRecord;
use crate::session::Session;

/// Most lines of a session diff.
pub const SESSION_DIFF_MAX_LINES: u32 = 2000;
/// Most bytes of a session diff: the line cap alone would let a few minified lines make
/// an event of many megabytes.
pub const SESSION_DIFF_MAX_BYTES: usize = 256 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum DiffError {
    #[error("cannot read the session log: {0}")]
    Session(#[from] tautly_session::Error),
    #[error("cannot read {path}: {source}")]
    Disk { path: String, source: SandboxError },
    #[error("there is no turn {turn}; this session has {turns}")]
    NoSuchTurn { turn: u32, turns: u32 },
}

/// The [`Event::SessionDiff`] for the whole session (`turn` None) or user turn `turn`
/// (1-based: the turn of the n-th `SendMessage`): for every file the session's tools
/// edited, created or deleted in that scope, the earliest pre-image in scope against the
/// current disk content. Files that end as they started (including one created and then
/// deleted) are left out; the rest come sorted by path. Capped at `max_lines` lines (and
/// [`SESSION_DIFF_MAX_BYTES`]).
///
/// Reads the log and the blob store with blocking I/O: call it off the async workers.
pub async fn session_diff(
    reader: &SessionReader<SessionRecord>,
    sb: &dyn Sandbox,
    turn: Option<u32>,
    max_lines: u32,
) -> Result<Event, DiffError> {
    let (first, turns) = earliest_pre_images(reader, turn)?;
    if let Some(t) = turn {
        if t == 0 || t > turns {
            return Err(DiffError::NoSuchTurn { turn: t, turns });
        }
    }
    let mut sides = Vec::with_capacity(first.len());
    for (path, pre) in first {
        let before = pre.map(|b| reader.read_blob(&b)).transpose()?;
        let shown = path
            .strip_prefix(sb.root())
            .map_or_else(|_| path.clone(), PathBuf::from);
        let after = match sb.read(&path).await {
            Ok(bytes) => Some(bytes),
            Err(SandboxError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(source) => {
                return Err(DiffError::Disk {
                    path: shown.to_string_lossy().into_owned(),
                    source,
                });
            }
        };
        sides.push((shown, before, after));
    }
    sides.sort_by(|a, b| a.0.cmp(&b.0));

    let mut files = Vec::new();
    let mut full = String::new();
    for (shown, before, after) in &sides {
        let status = match (before, after) {
            (None, None) => continue,
            (Some(b), Some(a)) if b == a => continue,
            (None, Some(_)) => DiffStatus::Added,
            (Some(_), None) => DiffStatus::Deleted,
            (Some(_), Some(_)) => DiffStatus::Modified,
        };
        let d = file_diff(shown, before.as_deref(), after.as_deref());
        files.push(DiffFile {
            path: shown.to_string_lossy().into_owned(),
            status,
            added: d.added,
            removed: d.removed,
        });
        full.push_str(&d.text);
    }
    let capped = cap(full, max_lines, SESSION_DIFF_MAX_BYTES);
    Ok(Event::SessionDiff {
        turn,
        files,
        unified: capped.unified,
        truncated: capped.truncated,
    })
}

/// A file a tool changed: its path and its content before the change (None: created).
type PreImage = (PathBuf, Option<BlobRef>);

/// The first `Edit` record per path in scope, in log order, and the number of user turns.
///
/// A `SendMessage` is logged when it arrives, which may be while the turn before it still
/// runs; turns run one at a time in that order, so a record belongs to the turn after the
/// last `TurnEnd` before it.
fn earliest_pre_images(
    reader: &SessionReader<SessionRecord>,
    turn: Option<u32>,
) -> Result<(Vec<PreImage>, u32), DiffError> {
    let mut messages = 0u32;
    let mut ended = 0u32;
    let mut edits: Vec<PreImage> = Vec::new();
    for rec in reader.records() {
        match rec?.1 {
            SessionRecord::Command(Command::SendMessage { .. }) => messages += 1,
            SessionRecord::TurnEnd { .. } => ended += 1,
            SessionRecord::Edit {
                path, pre_image, ..
            } if turn.is_none_or(|t| t == ended + 1) => edits.push((path, pre_image)),
            _ => {}
        }
    }
    let mut seen = HashSet::new();
    edits.retain(|(path, _)| seen.insert(path.clone()));
    Ok((edits, messages))
}

impl Session {
    /// Answer `Command::Diff` (between turns) with a `SessionDiff`, or an `Error`.
    pub(crate) async fn diff(&self, turn: Option<u32>) {
        // The reader must see every record written so far.
        self.checkpoint().await;
        let path = self.log.path();
        let sb = Arc::clone(&self.sandbox);
        let runtime = tokio::runtime::Handle::current();
        let answer = tokio::task::spawn_blocking(move || {
            let reader = SessionReader::<SessionRecord>::open(&path)?;
            runtime.block_on(session_diff(
                &reader,
                sb.as_ref(),
                turn,
                SESSION_DIFF_MAX_LINES,
            ))
        })
        .await;
        let message = match answer {
            Ok(Ok(ev)) => return self.emit(ev).await,
            Ok(Err(e)) => e.to_string(),
            Err(e) => format!("the diff failed: {e}"),
        };
        self.emit(Event::Error {
            kind: ErrorKind::Internal,
            message: format!("diff: {message}"),
            retryable: false,
            hint: None,
        })
        .await;
    }
}
