//! `Command::Diff` (spec 7.11.4): the net change per file that the session's tools made,
//! from the session log's `Edit` records (each holds the file's content before that
//! change) and the disk as it is now. Changes made by commands are not included.

use std::collections::HashSet;
use std::path::PathBuf;

use tautly_sandbox::{Sandbox, SandboxError};
use tautly_session::{BlobRef, SessionReader};
use tautly_tools::preview::diff_owned;

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
    #[error("the diff failed: {0}")]
    Crashed(String),
}

/// A changed file and its earliest pre-image in scope (None: the scope created it).
pub type PreImage = (PathBuf, Option<Vec<u8>>);

/// What [`session_diff`] starts from, for the whole session (`turn` None) or user turn
/// `turn` (1-based: the turn of the n-th `SendMessage`): every file the session's tools
/// edited, created or deleted in that scope, with its earliest pre-image in scope (from
/// the blob store), in log order.
///
/// Blocking I/O (the log and the blob store): call it off the async workers.
pub fn pre_images(
    reader: &SessionReader<SessionRecord>,
    turn: Option<u32>,
) -> Result<Vec<PreImage>, DiffError> {
    let (first, turns) = earliest_pre_images(reader, turn)?;
    if let Some(t) = turn
        && (t == 0 || t > turns)
    {
        return Err(DiffError::NoSuchTurn { turn: t, turns });
    }
    first
        .into_iter()
        .map(|(path, pre)| Ok((path, pre.map(|b| reader.read_blob(&b)).transpose()?)))
        .collect()
}

/// The [`Event::SessionDiff`] of `scope` (from [`pre_images`]): each file's pre-image
/// against its current disk content. Files that end as they started (including one
/// created and then deleted) are left out; the rest come sorted by path. Capped at
/// `max_lines` lines and [`SESSION_DIFF_MAX_BYTES`]; a large diff is computed on the
/// blocking pool.
pub async fn session_diff(
    scope: Vec<PreImage>,
    sb: &dyn Sandbox,
    turn: Option<u32>,
    max_lines: u32,
) -> Result<Event, DiffError> {
    let mut sides = Vec::with_capacity(scope.len());
    for (path, before) in scope {
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
        if before != after {
            sides.push((shown, before, after));
        }
    }
    sides.sort_by(|a, b| a.0.cmp(&b.0));
    let statuses: Vec<(String, DiffStatus)> = sides
        .iter()
        .map(|(shown, before, after)| {
            let status = match (before, after) {
                (None, _) => DiffStatus::Added,
                (_, None) => DiffStatus::Deleted,
                _ => DiffStatus::Modified,
            };
            (shown.to_string_lossy().into_owned(), status)
        })
        .collect();
    let (diff, counts) = diff_owned(sides, max_lines, SESSION_DIFF_MAX_BYTES).await;
    let files = statuses
        .into_iter()
        .zip(counts)
        .map(|((path, status), c)| DiffFile {
            path,
            status,
            added: c.added,
            removed: c.removed,
        })
        .collect();
    Ok(Event::SessionDiff {
        turn,
        files,
        unified: diff.unified,
        truncated: diff.truncated,
    })
}

/// A file a tool changed: its path and a reference to its content before the change
/// (None: created).
type PreImageRef = (PathBuf, Option<BlobRef>);

/// The first `Edit` record per path in scope, in log order, and the number of user turns.
///
/// A `SendMessage` is logged when it arrives, which may be while the turn before it still
/// runs; turns run one at a time in that order, so a record belongs to the turn after the
/// last `TurnEnd` before it.
fn earliest_pre_images(
    reader: &SessionReader<SessionRecord>,
    turn: Option<u32>,
) -> Result<(Vec<PreImageRef>, u32), DiffError> {
    let mut messages = 0u32;
    let mut ended = 0u32;
    let mut edits: Vec<PreImageRef> = Vec::new();
    for rec in reader.records() {
        match rec?.1 {
            SessionRecord::Command(Command::SendMessage { .. }) => messages += 1,
            SessionRecord::TurnEnd { .. } => ended += 1,
            SessionRecord::Edit {
                path,
                pre_image,
                asserted: false,
                ..
            } if turn.is_none_or(|t| t == ended + 1) => {
                edits.push((PathBuf::from(path.as_str()), pre_image))
            }
            _ => {}
        }
    }
    let mut seen = HashSet::new();
    edits.retain(|(path, _)| seen.insert(path.clone()));
    Ok((edits, messages))
}

impl Session {
    /// Answer `Command::Diff` (between turns) with a `SessionDiff`, or an `Error`. The
    /// reply waits (bounded) for room in the event channel: the client asked for it.
    pub(crate) async fn diff(&self, turn: Option<u32>) {
        let ev = match self.session_diff(turn).await {
            Ok(ev) => ev,
            Err(e) => {
                let hint = match &e {
                    DiffError::NoSuchTurn { turns, .. } => Some(format!(
                        "this session has {turns} user turn{}",
                        if *turns == 1 { "" } else { "s" }
                    )),
                    _ => None,
                };
                Event::Error {
                    id: None,
                    kind: ErrorKind::Internal,
                    message: format!("diff: {e}"),
                    retryable: false,
                    hint,
                }
            }
        };
        self.events.send_or_drop(ev).await;
    }

    async fn session_diff(&self, turn: Option<u32>) -> Result<Event, DiffError> {
        // The reader must see every record written so far.
        self.checkpoint().await;
        let path = self.log.path();
        let scope = tokio::task::spawn_blocking(move || {
            let reader = SessionReader::<SessionRecord>::open(&path)?;
            pre_images(&reader, turn)
        })
        .await
        .map_err(|e| DiffError::Crashed(e.to_string()))??;
        session_diff(scope, self.sandbox.as_ref(), turn, SESSION_DIFF_MAX_LINES).await
    }
}
