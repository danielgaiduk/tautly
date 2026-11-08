//! Session store operations outside a running engine (spec 6.5): `tautly sessions
//! delete`, and the session `--continue` picks.

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
    /// The worktree of a worktree fork, removed with it.
    pub worktree: Option<PathBuf>,
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
    /// Not a `#[source]`: the message says it (review M4).
    #[error("cannot remove the fork's worktree: {0}")]
    Worktree(crate::worktree::WtError),
    #[error(
        "this is run inside the fork's own worktree {}: run it from another worktree of \
         the repository (the main one, say), since it removes {}",
        .0.display(),
        .0.display()
    )]
    InsideWorktree(PathBuf),
    #[error(
        "another live session uses the fork's worktree {}; close it first",
        .0.display()
    )]
    WorktreeBusy(PathBuf),
}

impl From<crate::worktree::WtError> for DeleteError {
    fn from(e: crate::worktree::WtError) -> Self {
        DeleteError::Worktree(e)
    }
}

/// Delete session `id` of the project the sandbox root belongs to (`tautly sessions
/// delete <id>`): [`delete_with`] without `force`.
pub async fn delete(
    data_home: &Path,
    sandbox: &dyn Sandbox,
    id: &str,
) -> Result<Deleted, DeleteError> {
    delete_with(data_home, sandbox, id, false).await
}

/// Delete session `id` of the project the sandbox root belongs to: the worktree of a
/// worktree fork (spec 7.12.4; [`crate::worktree::remove`], which refuses one with
/// modified, untracked or ignored files unless `force`; refused from inside that worktree,
/// and while a live session holds its working-tree lease), its checkpoint refs (`refs/tautly/checkpoints/<id>/`,
/// when the root is in a git work tree), then its log and last-request file. Blobs are
/// shared by the project's sessions and kept (blob garbage collection is post-v1); so are
/// its branches, whose logs hold their own copies and whose checkpoints have refs of their
/// own. A session an engine holds open is refused; so is a dirty fork worktree without
/// `force`, and then nothing is deleted.
pub async fn delete_with(
    data_home: &Path,
    sandbox: &dyn Sandbox,
    id: &str,
    force: bool,
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
    // A worktree fork's worktree goes first: a dirty one refuses the whole deletion.
    let header = tautly_session::SessionReader::<SessionRecord>::open(&log)?
        .header()
        .clone();
    let fork = header.origin == tautly_session::Origin::WhatIf;
    if fork && header.root == root {
        // It could not remove the worktree it runs in (review I1).
        return Err(DeleteError::InsideWorktree(root));
    }
    let worktree = (fork && header.root.exists()).then_some(header.root);
    if let Some(dir) = &worktree {
        // Not while a live session uses it (review I2): its working-tree lease, held
        // exclusively for the removal.
        let busy = || DeleteError::WorktreeBusy(dir.clone());
        let lease = crate::lease::Lease::try_shared(data_home, dir)
            .map_err(|source| DeleteError::Io {
                path: dir.clone(),
                source,
            })?
            .ok_or_else(busy)?;
        let _alone = lease.try_exclusive().map_err(|e| match e {
            crate::lease::LeaseError::Busy => busy(),
            crate::lease::LeaseError::Io(source) => DeleteError::Io {
                path: dir.clone(),
                source,
            },
        })?;
        let start = fork_start(&log)?;
        crate::worktree::remove(sandbox, &root, dir, start.as_deref(), force).await?;
    }
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
    Ok(Deleted {
        log,
        refs,
        worktree,
    })
}

/// The commit a worktree fork's worktree began at: the `restored` checkpoint its log wrote
/// after `Forked` (none: not found).
fn fork_start(log: &Path) -> Result<Option<String>, DeleteError> {
    let reader = tautly_session::SessionReader::<SessionRecord>::open(log)?;
    let mut forked = false;
    for rec in reader.records() {
        match rec?.1 {
            SessionRecord::Forked => forked = true,
            SessionRecord::Checkpoint {
                commit,
                point: Some(crate::record::CheckpointPoint::Restored),
                ..
            } if forked => return Ok(Some(commit)),
            _ => {}
        }
    }
    Ok(None)
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
    // A busy lock is tried again briefly: the session tree's liveness probe holds a shared
    // lock for an instant.
    match tautly_session::files::lock_exclusive_retrying(&file) {
        Ok(true) => Ok(file),
        Ok(false) => Err(DeleteError::Running(id.to_string())),
        Err(e) => Err(io(e)),
    }
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
            SessionRecord::Command(crate::Command::SendMessage { id: _, text }) => text,
            _ => continue,
        };
        let line = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
        return Some(line.trim().chars().take(FIRST_PROMPT_CHARS).collect());
    }
    None
}

/// What [`restore_backup`] put back (with `dry_run`: would put back); root-relative.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BackupRestored {
    /// Files written with their content from before the restore.
    pub written: Vec<String>,
    /// Files removed (they did not exist before it).
    pub removed: Vec<String>,
    /// Files changed since the restore wrote them (a human edit): left alone unless
    /// listed in `overwrite`.
    pub conflicts: Vec<String>,
    /// Files whose content was not kept (a link, or over 64 MiB), or that cannot be written
    /// (a link or a file is above them).
    pub not_kept: Vec<String>,
    /// The restore's snapshot, which holds the rest (git).
    pub backup_ref: Option<String>,
    /// Every path the restore was about to write.
    pub paths: Vec<String>,
    /// The backup put back is one an earlier `restore-backup` kept of what it replaced
    /// (`include_own`), not a rewind's or switch's.
    pub own: bool,
    /// Later backups an earlier `restore-backup` kept were passed over (without
    /// `include_own`).
    pub skipped_own: bool,
}

/// How [`restore_backup`] runs.
#[derive(Debug, Clone, Default)]
pub struct RestoreBackupOptions {
    /// Change nothing; say what would change.
    pub dry_run: bool,
    /// Put the backup back although the restore finished (`RestoreEnded` is logged).
    pub force: bool,
    /// Conflicting paths (changed since the restore wrote them) to put back all the same.
    pub overwrite: Vec<String>,
    /// Also consider the backups `restore-backup` itself kept (of what it replaced), so the
    /// last one puts back what an earlier run wrote. Without it, those are passed over: a
    /// run again after one was cut off finishes it instead of reversing it.
    pub include_own: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum BackupError {
    #[error("no session {0} in this project")]
    NotFound(String),
    #[error("session {0} has no rewind or switch backup")]
    NoBackup(String),
    #[error(
        "the last rewind or switch of session {0} finished ({1}); its backup is put back \
         only with --force"
    )]
    Finished(String, &'static str),
    #[error("cannot read or write the session: {0}")]
    Session(#[from] tautly_session::Error),
    #[error("{0}")]
    Lease(#[from] crate::lease::LeaseError),
    #[error("cannot lock the working tree: {0}")]
    Io(#[from] std::io::Error),
    #[error("{path}: {source}")]
    Disk {
        path: String,
        source: tautly_sandbox::SandboxError,
    },
    #[error("a path in the session log cannot be used here: {0}")]
    Path(#[from] tautly_session::ConfineError),
    #[error("{0}")]
    Other(String),
}

/// Put back the files the last rewind or switch of session `id` kept in the session log
/// before it wrote them (its `RestoreBegun` record; spec 7.12.2), through `sandbox`:
/// `tautly sessions restore-backup <id>`. It never destroys a human edit:
///
/// - It is refused while any engine runs on the working tree (the lease), and when that
///   restore finished (`RestoreEnded`), unless `force`.
/// - A file whose content or mode is no longer what the restore wrote there is a
///   conflict, left alone unless listed in `overwrite`.
/// - Before it writes, what it replaces is kept in the log as a `RestoreBegun` of its own
///   (marked `restore_backup`), and a `RestoreEnded` follows. Those are passed over unless
///   `include_own`: run again (say after it was cut off), it goes on with the rewind's or
///   switch's backup instead of reversing itself; with `include_own`, the last one puts
///   back what the previous run replaced.
/// - A path with a link or a file above it, or that no longer resolves to itself, is never
///   written.
///
/// A snapshot ref the record names holds the other files; it is returned, with the paths,
/// to point the user at it. With `dry_run` nothing is written or logged.
pub async fn restore_backup(
    data_home: &Path,
    sandbox: &dyn Sandbox,
    id: &str,
    opts: &RestoreBackupOptions,
) -> Result<BackupRestored, BackupError> {
    use crate::record::{BackupFile, RestoreBy, RestoreOutcome, Wrote};
    use crate::undo::{Disk, MAX_DISK_READ, read_disk};
    let root = sandbox.root().to_path_buf();
    let lease = crate::lease::Lease::try_shared(data_home, &root)?
        .ok_or(BackupError::Lease(crate::lease::LeaseError::Busy))?;
    let _alone = lease.try_exclusive()?;
    let log = tautly_session::list_here_in(data_home, &root)?
        .into_iter()
        .find(|s| s.id == id)
        .map(|s| s.path)
        .ok_or_else(|| BackupError::NotFound(id.to_string()))?;
    let reader = tautly_session::SessionReader::<SessionRecord>::open(&log)?;
    let mut last = None;
    let mut ended = None;
    // The last `RestoreBegun` was passed over (its own, without `include_own`): the
    // `RestoreEnded` that follows is that one's.
    let mut passed = false;
    let mut skipped_own = false;
    for rec in reader.records() {
        match rec?.1 {
            SessionRecord::RestoreBegun {
                by: RestoreBy::RestoreBackup,
                ..
            } if !opts.include_own => {
                passed = true;
                skipped_own = true;
            }
            SessionRecord::RestoreBegun {
                backup_ref,
                files,
                paths,
                by,
            } => {
                last = Some((backup_ref, files, paths, by == RestoreBy::RestoreBackup));
                ended = None;
                passed = false;
                skipped_own = false;
            }
            SessionRecord::RestoreEnded { outcome } if !passed => {
                ended = Some(match outcome {
                    RestoreOutcome::Applied => "it was applied",
                    RestoreOutcome::RolledBack => "it was put back already",
                    RestoreOutcome::Failed => "it failed",
                    RestoreOutcome::Unknown => "it ended",
                });
            }
            _ => {}
        }
    }
    let (backup_ref, files, paths, own) =
        last.ok_or_else(|| BackupError::NoBackup(id.to_string()))?;
    if let Some(how) = ended.filter(|_| !opts.force) {
        return Err(BackupError::Finished(id.to_string(), how));
    }
    let mut done = BackupRestored {
        backup_ref,
        paths: paths.iter().map(|p| p.as_str().to_string()).collect(),
        own,
        skipped_own,
        ..BackupRestored::default()
    };
    // What to put back, after checking each path against what the restore wrote.
    let mut planned: Vec<(BackupFile, Disk)> = Vec::new();
    for f in files {
        f.path.resolve(&root)?;
        let rel = PathBuf::from(f.path.as_str());
        let name = f.path.as_str().to_string();
        if f.existed && f.from_blob.is_none() {
            done.not_kept.push(name);
            continue;
        }
        if crate::timetravel::confined(sandbox, &root, &rel).is_err() {
            done.not_kept.push(name);
            continue;
        }
        let disk = read_disk(&root, &rel, MAX_DISK_READ)
            .await
            .map_err(|e| BackupError::Other(e.to_string()))?;
        let b3 = |bytes: &[u8]| blake3::hash(bytes).to_hex().to_string();
        // The mode the restore left (Task 49 M-R3c): recorded, or for an older record, a
        // file's own (a rewind writes a kept file's content over it with its mode).
        let wrote_mode = f.to_mode.or(match &f.to {
            Some(Wrote::Blob(_)) if f.existed && !own => f.from_mode,
            _ => None,
        });
        let mode_kept = |m: &u32| wrote_mode.is_none_or(|w| w == *m);
        let as_written = match (&f.to, &disk) {
            (Some(Wrote::Absent), Disk::Absent) => true,
            (Some(Wrote::Blob(b)), Disk::File { bytes, mode }) => {
                b3(bytes) == b.as_str() && mode_kept(mode)
            }
            (Some(Wrote::Blob(b)), Disk::Large { hash, mode, .. }) => {
                hash.to_hex().as_str() == b.as_str() && mode_kept(mode)
            }
            _ => false,
        };
        let back_already = match (&f.from_blob, &disk) {
            (None, Disk::Absent) => true,
            (Some(b), Disk::File { bytes, .. }) => b3(bytes) == b.as_str(),
            _ => false,
        };
        if back_already {
            continue;
        }
        if !as_written && !opts.overwrite.contains(&name) {
            done.conflicts.push(name);
            continue;
        }
        if matches!(disk, Disk::Large { .. } | Disk::Link(_) | Disk::Other) {
            // What it would replace cannot be kept.
            done.not_kept.push(name);
            continue;
        }
        match f.existed {
            true => done.written.push(name),
            false => done.removed.push(name),
        }
        planned.push((f, disk));
    }
    if opts.dry_run || planned.is_empty() {
        return Ok(done);
    }
    // What it replaces, kept first.
    let mut writer = tautly_session::SessionWriter::<SessionRecord>::reopen(&log)?;
    let mut kept = Vec::with_capacity(planned.len());
    for (f, disk) in &planned {
        let (existed, blob, mode) = match disk {
            Disk::File { bytes, mode } => (true, Some(writer.blob(bytes)?), Some(*mode)),
            _ => (false, None, None),
        };
        kept.push(BackupFile {
            path: f.path.clone(),
            existed,
            from_blob: blob,
            from_mode: mode,
            to: Some(match &f.from_blob {
                Some(b) => Wrote::Blob(b.clone()),
                None => Wrote::Absent,
            }),
            // Written with the backup's mode, else over the file with its own.
            to_mode: f.from_blob.as_ref().and(f.from_mode.or(mode)),
        });
    }
    writer.append(&SessionRecord::RestoreBegun {
        backup_ref: None,
        files: kept,
        paths: planned.iter().map(|(f, _)| f.path.clone()).collect(),
        by: RestoreBy::RestoreBackup,
    })?;
    writer.checkpoint()?;
    let mut failed = None;
    for (f, _) in &planned {
        let rel = PathBuf::from(f.path.as_str());
        let full = match crate::timetravel::confined(sandbox, &root, &rel) {
            Ok(full) => full,
            Err(e) => {
                failed = Some(BackupError::Other(e.to_string()));
                break;
            }
        };
        let wrote = match &f.from_blob {
            Some(blob) => match reader.read_blob(blob) {
                Ok(bytes) => sandbox.write_atomic(&full, &bytes, f.from_mode).await,
                Err(e) => {
                    failed = Some(e.into());
                    break;
                }
            },
            None => match sandbox.remove_file(&full).await {
                Err(tautly_sandbox::SandboxError::Io(e))
                    if e.kind() == std::io::ErrorKind::NotFound =>
                {
                    Ok(())
                }
                other => other,
            },
        };
        if let Err(source) = wrote {
            failed = Some(BackupError::Disk {
                path: full.display().to_string(),
                source,
            });
            break;
        }
    }
    writer.append(&SessionRecord::RestoreEnded {
        outcome: match failed {
            None => RestoreOutcome::Applied,
            Some(_) => RestoreOutcome::Failed,
        },
    })?;
    writer.checkpoint()?;
    match failed {
        Some(e) => Err(e),
        None => Ok(done),
    }
}
