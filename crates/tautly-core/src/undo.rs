//! Undo of one user turn (spec 7.11.11, 6.5 H7): `Command::Undo` and `tautly undo`.
//!
//! [`undo`] reverts what user turn `turn` (default: the last one that started) changed,
//! walking its records in reverse: an `Edit` gives back its pre-image, a `Checkpoint`
//! the work tree it snapshotted. So the files go back to the turn's first checkpoint, with
//! the tool edits made before it (before the turn's first command) reverted to their
//! pre-images:
//!
//! - **With git checkpoints** (a [`Checkpointer`] and at least one `Checkpoint` record in
//!   the turn), commands' changes are undone too: files they changed or deleted come back
//!   from the checkpoint, files they added (untracked, not ignored) are deleted. Ignored
//!   files and submodules are never touched. A backup snapshot
//!   (`refs/tautly/checkpoints/<session>/undo-<n>`) is taken before the first write.
//! - **Otherwise** only tool edits are reverted, from their pre-images (Task 26), and
//!   `note` says command side effects were not undone.
//!
//! It never overwrites what it did not write (spec 7.12): a file must be as the turn left
//! it (or already back at its content before the turn) — for a file the turn's tools wrote
//! last, the content (and mode) they wrote; for one a command may have changed after, its
//! content in the turn's last checkpoint. Otherwise the undo is refused before anything is
//! written: [`UndoError::LaterTurns`] when a later turn (not undone) changed the file —
//! undo those first, newest first — else [`UndoError::Changed`] (a human edit, `chmod`
//! or other process). A file already back at its content before the turn is logged as
//! undone without a write. The undo is logged as `Undone` with each file's content just
//! before (so it can be reverted in turn) and the backup ref; with `dry_run` nothing is
//! written or logged and the report lists what would change.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tautly_sandbox::{Sandbox, SandboxError};
use tautly_session::{BlobRef, ConfineError, RelPath, SessionReader, SessionWriter};
use tautly_tools::{AppliedEffect, PersistedEffect};

use crate::checkpoint::{self, Change, CheckpointError, Checkpointer, Skip};
use crate::protocol::{ErrorKind, Event};
use crate::record::{CheckpointPoint, SessionRecord, UndoneFile};
use crate::session::{Log, Session};
use crate::turn::TurnControl;

/// `note` when the turn ran commands and checkpoints restored them.
pub const NOTE_GIT: &str = "ignored files and submodules are not restored";
/// `note` when the turn ran commands outside a git work tree.
pub const NOTE_NOT_GIT: &str = "command side effects not undone (not a git repository)";
/// `note` when the turn ran commands in git, but no checkpoint can restore them.
const NOTE_NO_CHECKPOINT: &str =
    "command side effects not undone (no checkpoint of this turn is available)";
/// Added to the note when one of the turn's checkpoints was skipped.
const NOTE_SKIPPED: &str =
    "a checkpoint of this turn was skipped, so some command effects may remain";
/// Added to the note, with the paths, for files the turn left that were not deleted
/// because the ignore rules at the turn's start ignore them (they may have been there).
const NOTE_KEPT: &str = "kept (ignored before the turn)";

/// Largest file undo reads from the work tree (to compare it and to log what it held).
/// A larger one is compared by a streaming hash, and what it held is not logged.
pub(crate) const MAX_DISK_READ: u64 = 64 * 1024 * 1024;
const NOTE_LARGE: &str = "prior content over 64 MiB not kept in the session log";

/// What an undo did (with `dry_run`: would do). Paths are absolute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UndoReport {
    /// The user turn undone (1-based).
    pub turn: u32,
    /// Files written back.
    pub restored: Vec<PathBuf>,
    /// Files deleted (the turn created them).
    pub deleted: Vec<PathBuf>,
    pub dry_run: bool,
    /// The snapshot taken before writing.
    pub backup_ref: Option<String>,
    /// What was not undone.
    pub note: Option<String>,
}

impl UndoReport {
    /// The answer event, with root-relative paths.
    pub fn event(&self, root: &Path) -> Event {
        let rel = |paths: &[PathBuf]| -> Vec<String> {
            paths
                .iter()
                .map(|p| {
                    p.strip_prefix(root)
                        .unwrap_or(p)
                        .to_string_lossy()
                        .into_owned()
                })
                .collect()
        };
        Event::Undone {
            id: None,
            turn: self.turn,
            restored: rel(&self.restored),
            deleted: rel(&self.deleted),
            dry_run: self.dry_run,
            backup_ref: self.backup_ref.clone(),
            note: self.note.clone(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum UndoError {
    #[error("cannot read or write the session log: {0}")]
    Session(#[from] tautly_session::Error),
    #[error("the session log could not be written: {0}")]
    Log(String),
    #[error("there is no turn {turn}; this session has {turns}")]
    NoSuchTurn { turn: u32, turns: u32 },
    #[error("a path in the session log cannot be used here: {0}")]
    Path(#[from] ConfineError),
    #[error("{path}: {source}")]
    Disk { path: String, source: SandboxError },
    #[error("changed since the turn: {}", list(paths))]
    Changed { paths: Vec<PathBuf> },
    #[error("changed by later turns: {}", list(paths))]
    LaterTurns {
        turns: Vec<u32>,
        paths: Vec<PathBuf>,
    },
    #[error("git checkpoint: {0}")]
    Checkpoint(#[from] CheckpointError),
}

impl From<Skip> for UndoError {
    fn from(s: Skip) -> Self {
        UndoError::Checkpoint(s.into())
    }
}

fn list(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Where an undo logs: the session's own writer (offline) or the engine's log.
pub(crate) trait UndoLog: Send {
    fn blob(&mut self, bytes: &[u8]) -> Result<BlobRef, UndoError>;
    /// Append and make durable.
    fn record(&mut self, rec: &SessionRecord) -> Result<(), UndoError>;
}

impl UndoLog for SessionWriter<SessionRecord> {
    fn blob(&mut self, bytes: &[u8]) -> Result<BlobRef, UndoError> {
        Ok(SessionWriter::blob(self, bytes)?)
    }

    fn record(&mut self, rec: &SessionRecord) -> Result<(), UndoError> {
        self.append(rec)?;
        Ok(self.checkpoint()?)
    }
}

/// The engine's log (blocking file I/O, like every log write).
pub(crate) struct LiveLog(pub Log);

impl UndoLog for LiveLog {
    fn blob(&mut self, bytes: &[u8]) -> Result<BlobRef, UndoError> {
        self.0
            .blob(bytes)
            .map_err(|_| UndoError::Log("a blob could not be stored".into()))
    }

    fn record(&mut self, rec: &SessionRecord) -> Result<(), UndoError> {
        self.0
            .append(rec)
            .and_then(|()| self.0.checkpoint())
            .map_err(|_| UndoError::Log("the Undone record could not be written".into()))
    }
}

/// Undo user turn `turn` (default: the last one that started) of the session at
/// `reader`, offline (`tautly undo [--turn N]`): see the module docs. `cp` is the work
/// tree's checkpointer (none outside git or with checkpoints off). The log is reopened for
/// writing (not for a dry run), so the session must not be running in an engine
/// ([`tautly_session::Error::AlreadyOpen`]). Blocking file I/O on the log.
pub async fn undo(
    reader: &SessionReader<SessionRecord>,
    sandbox: &dyn Sandbox,
    cp: Option<&mut Checkpointer>,
    turn: Option<u32>,
    dry_run: bool,
) -> Result<UndoReport, UndoError> {
    let mut writer = if dry_run {
        None
    } else {
        Some(SessionWriter::<SessionRecord>::reopen(reader.path())?)
    };
    undo_with(
        reader,
        sandbox,
        cp,
        turn,
        dry_run,
        writer.as_mut().map(|w| w as &mut dyn UndoLog),
    )
    .await
}

/// [`undo`] logging to `log` (none: a dry run).
pub(crate) async fn undo_with(
    reader: &SessionReader<SessionRecord>,
    sandbox: &dyn Sandbox,
    cp: Option<&mut Checkpointer>,
    turn: Option<u32>,
    dry_run: bool,
    log: Option<&mut dyn UndoLog>,
) -> Result<UndoReport, UndoError> {
    let scan = Scan::read(reader)?;
    let turn = turn.unwrap_or(scan.turns);
    if turn == 0 || turn > scan.turns {
        return Err(UndoError::NoSuchTurn {
            turn,
            turns: scan.turns,
        });
    }
    let recs = scan.per_turn.get(&turn).cloned().unwrap_or_default();
    let mut cp = cp;
    if let Some(c) = cp.as_deref_mut() {
        c.continue_after(scan.max_n, None);
    }
    // The turn's checkpoints, if the repository still has them (refs pruned and objects
    // collected since, or a log from another clone, fall back to the pre-images).
    let commits: Vec<&str> = recs.checkpoints().map(|(_, c)| c).collect();
    let usable = match cp.as_deref() {
        Some(c) if !commits.is_empty() => {
            c.have_commits(&commits, Checkpointer::housekeeping_deadline())
                .await
        }
        _ => false,
    };
    match cp {
        Some(cp) if usable => {
            let undo = GitUndo {
                reader,
                sandbox,
                cp,
                scan: &scan,
                turn,
                recs: &recs,
            };
            undo.run(dry_run, log).await
        }
        cp => {
            let note = recs.ran_commands.then(|| {
                if std::fs::symlink_metadata(sandbox.root().join(".git")).is_err() {
                    NOTE_NOT_GIT
                } else {
                    NOTE_NO_CHECKPOINT
                }
                .to_string()
            });
            preimage_undo(reader, sandbox, cp, Some(turn), dry_run, log, note).await
        }
    }
}

// ------------------------------------------------------------------- the log, scanned

/// One record of a user turn that matters to an undo, in log order.
#[derive(Debug, Clone)]
enum Ev {
    /// A tool changed `path`: its content before (none: created), its mode before, and
    /// the hash it left (none: deleted).
    Edit {
        path: PathBuf,
        pre: Option<BlobRef>,
        pre_mode: Option<u32>,
        post: Option<String>,
    },
    /// A checkpoint (`end`: the turn-end one, right before `TurnEnd`).
    Checkpoint { commit: String, end: bool },
    /// A command ran (`bash`, a stdio MCP call).
    Command,
}

#[derive(Debug, Clone, Default)]
struct TurnRecs {
    events: Vec<Ev>,
    ran_commands: bool,
    /// A checkpoint of the turn was skipped.
    skipped: bool,
    /// Its end checkpoint was skipped by a cancel (and not recovered after it).
    end_skipped: bool,
}

impl TurnRecs {
    fn checkpoints(&self) -> impl Iterator<Item = (usize, &str)> {
        self.events.iter().enumerate().filter_map(|(i, e)| match e {
            Ev::Checkpoint { commit, .. } => Some((i, commit.as_str())),
            _ => None,
        })
    }
}

/// What the log says, per user turn.
struct Scan {
    turns: u32,
    per_turn: HashMap<u32, TurnRecs>,
    /// The paths each turn's undos restored (root-relative).
    undone: HashMap<u32, HashSet<PathBuf>>,
    /// The highest checkpoint number used (backups included).
    max_n: u32,
}

impl Scan {
    fn read(reader: &SessionReader<SessionRecord>) -> Result<Scan, UndoError> {
        let mut scan = Scan {
            turns: 0,
            per_turn: HashMap::new(),
            undone: HashMap::new(),
            max_n: 0,
        };
        let mut current = 0u32;
        // The turn that just ended with its end checkpoint skipped by a cancel: its end is
        // recovered in the background and logged after its `TurnEnd` (Task 49 N1).
        let mut recovering = 0u32;
        for rec in reader.records() {
            match rec?.1 {
                SessionRecord::TurnStart { turn, .. } => {
                    current = turn;
                    recovering = 0;
                    scan.turns = scan.turns.max(turn);
                }
                SessionRecord::TurnEnd { .. } => {
                    if let Some(Ev::Checkpoint { end, .. }) = scan
                        .per_turn
                        .get_mut(&current)
                        .and_then(|t| t.events.last_mut())
                    {
                        *end = true;
                    }
                    if scan.per_turn.get(&current).is_some_and(|t| t.end_skipped) {
                        recovering = current;
                    }
                    current = 0;
                }
                // The recovered end of a cancelled turn: that turn's end after all.
                SessionRecord::Checkpoint {
                    n,
                    commit,
                    turn,
                    point: Some(CheckpointPoint::TurnEnd),
                    ..
                } if current == 0 && recovering > 0 && turn == recovering => {
                    scan.max_n = scan.max_n.max(n);
                    let t = scan.per_turn.entry(turn).or_default();
                    t.events.push(Ev::Checkpoint { commit, end: true });
                    t.end_skipped = false;
                    recovering = 0;
                }
                // A turn's own: the session-start checkpoint, taken in the background, can
                // be logged after the next turn's `TurnStart`.
                SessionRecord::Checkpoint {
                    n, commit, turn, ..
                } => {
                    scan.max_n = scan.max_n.max(n);
                    if current > 0 && turn == current {
                        let t = scan.per_turn.entry(current).or_default();
                        t.events.push(Ev::Checkpoint { commit, end: false });
                    }
                }
                SessionRecord::CheckpointSkipped {
                    turn,
                    reason,
                    point,
                    ..
                } if current > 0 && turn == current => {
                    let t = scan.per_turn.entry(current).or_default();
                    if reason == "cancelled" && point == Some(CheckpointPoint::TurnEnd) {
                        t.end_skipped = true;
                    } else {
                        t.skipped = true;
                    }
                }
                SessionRecord::ToolResult { effects, .. } if current > 0 => {
                    let t = scan.per_turn.entry(current).or_default();
                    for e in effects {
                        match e {
                            PersistedEffect::Edited {
                                path,
                                pre_image_blob,
                                pre_mode,
                                post_hash,
                                ..
                            } => t.events.push(Ev::Edit {
                                path: PathBuf::from(path.as_str()),
                                pre: pre_image_blob.map(|b| BlobRef::parse(&b)).transpose()?,
                                pre_mode,
                                post: Some(post_hash.to_hex().to_string()),
                            }),
                            PersistedEffect::Deleted {
                                path,
                                pre_image_blob,
                                pre_mode,
                            } => t.events.push(Ev::Edit {
                                path: PathBuf::from(path.as_str()),
                                pre: Some(BlobRef::parse(&pre_image_blob)?),
                                pre_mode: Some(pre_mode),
                                post: None,
                            }),
                            PersistedEffect::Exec { may_write: true }
                            | PersistedEffect::Unknown => {
                                t.ran_commands = true;
                                t.events.push(Ev::Command);
                            }
                            _ => {}
                        }
                    }
                }
                SessionRecord::Undone {
                    turn,
                    files,
                    backup_ref,
                } => {
                    let paths = scan.undone.entry(turn).or_default();
                    paths.extend(files.iter().map(|f| PathBuf::from(f.path.as_str())));
                    if let Some(n) = backup_ref
                        .as_deref()
                        .and_then(|r| r.rsplit_once("/undo-"))
                        .and_then(|(_, n)| n.parse::<u32>().ok())
                    {
                        scan.max_n = scan.max_n.max(n);
                    }
                }
                _ => {}
            }
        }
        Ok(scan)
    }
}

// ----------------------------------------------------------------- the checkpoint undo

/// What one path goes back to.
#[derive(Debug, Clone)]
enum Back {
    /// A tool edit's pre-image (none: the file did not exist) and mode.
    Blob {
        pre: Option<BlobRef>,
        mode: Option<u32>,
    },
    /// The file as in the turn's first checkpoint (`present`: it is in it).
    Checkpoint { present: bool },
}

impl Back {
    fn present(&self) -> bool {
        match self {
            Back::Blob { pre, .. } => pre.is_some(),
            Back::Checkpoint { present } => *present,
        }
    }
}

/// What is at a path now, as git sees it: symbolic links are never followed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Disk {
    /// Nothing (or the path is below a link or a file, where git does not look).
    Absent,
    /// A regular file and its permission bits.
    File { bytes: Vec<u8>, mode: u32 },
    /// A regular file over the read limit: its streaming blake3, permission bits and
    /// absolute path, never its bytes (its prior content is not kept in the log).
    Large {
        hash: blake3::Hash,
        mode: u32,
        path: PathBuf,
    },
    /// A symbolic link and its target.
    Link(Vec<u8>),
    /// A directory or another kind of file: never written over.
    Other,
}

impl Disk {
    fn exists(&self) -> bool {
        *self != Disk::Absent
    }

    /// Its tree entry's mode, for a file or link.
    fn tree_mode(&self) -> Option<&'static str> {
        match self {
            Disk::File { mode, .. } | Disk::Large { mode, .. } if mode & 0o100 != 0 => {
                Some("100755")
            }
            Disk::File { .. } | Disk::Large { .. } => Some("100644"),
            Disk::Link(_) => Some("120000"),
            Disk::Absent | Disk::Other => None,
        }
    }

    /// Content and mode as an `Undone` record keeps them: a link as its target, with
    /// git's mode for links (`0o120000`); nothing for a directory.
    fn logged(&self) -> (Option<&[u8]>, Option<u32>) {
        match self {
            Disk::File { bytes, mode } => (Some(bytes), Some(*mode)),
            Disk::Large { mode, .. } => (None, Some(*mode)),
            Disk::Link(target) => (Some(target), Some(0o120000)),
            Disk::Absent | Disk::Other => (None, None),
        }
    }

    /// Its blake3 hex, for a regular file.
    fn file_hash(&self) -> Option<(String, u32)> {
        match self {
            Disk::File { bytes, mode } => Some((blake3::hash(bytes).to_hex().to_string(), *mode)),
            Disk::Large { hash, mode, .. } => Some((hash.to_hex().to_string(), *mode)),
            _ => None,
        }
    }

    /// The file `pre` (none: absent) with permission bits `mode`.
    fn is(&self, pre: Option<&[u8]>, mode: Option<u32>) -> bool {
        match (self, pre) {
            (Disk::Absent, None) => true,
            (Disk::File { bytes, mode: m }, Some(pre)) => bytes == pre && Some(*m) == mode,
            (Disk::Large { hash, mode: m, .. }, Some(pre)) => {
                blake3::hash(pre) == *hash && Some(*m) == mode
            }
            _ => false,
        }
    }
}

/// A tree entry: `(mode, object id)`; none: absent.
type Side = Option<(String, String)>;

/// A path an undo writes.
struct Planned {
    rel: PathBuf,
    back: Back,
    /// What is there now.
    disk: Disk,
}

struct GitUndo<'a> {
    reader: &'a SessionReader<SessionRecord>,
    sandbox: &'a dyn Sandbox,
    cp: &'a mut Checkpointer,
    scan: &'a Scan,
    turn: u32,
    recs: &'a TurnRecs,
}

/// The three trees of a checkpoint undo, as pairwise diffs: the turn's first and last
/// checkpoints and the work tree now.
struct Trees {
    by_turn: HashMap<PathBuf, Change>,
    since_start: HashMap<PathBuf, Change>,
    since_end: HashMap<PathBuf, Change>,
}

impl Trees {
    /// `rel` in the first checkpoint, the last one and the work tree now; none when all
    /// three are the same.
    fn sides(&self, rel: &Path) -> Option<(Side, Side, Side)> {
        if let Some(c) = self.since_start.get(rel) {
            let end = match self.since_end.get(rel) {
                Some(e) => e.old.clone(),
                None => c.new.clone(),
            };
            return Some((c.old.clone(), end, c.new.clone()));
        }
        // The same at the start and now.
        self.by_turn
            .get(rel)
            .map(|c| (c.old.clone(), c.new.clone(), c.old.clone()))
    }
}

impl GitUndo<'_> {
    async fn run(
        self,
        dry_run: bool,
        log: Option<&mut dyn UndoLog>,
    ) -> Result<UndoReport, UndoError> {
        let root = self.sandbox.root().to_path_buf();
        let deadline = Checkpointer::housekeeping_deadline();
        let cps: Vec<(usize, &str)> = self.recs.checkpoints().collect();
        let (start_at, start) = cps[0];
        let end = cps[cps.len() - 1].1;
        // The work tree now, as a tree (within the untracked-file limits).
        let now = self.cp.tree_id(true, deadline).await?;
        let trees = Trees {
            by_turn: index_changes(self.cp.diff(start, end, deadline).await?),
            since_start: index_changes(self.cp.diff(start, &now, deadline).await?),
            since_end: index_changes(self.cp.diff(end, &now, deadline).await?),
        };

        // Each tool-edited path: its earliest pre-image before the first checkpoint (what
        // it goes back to), and its last edit with whether a command ran after it.
        let mut pre_edit: HashMap<PathBuf, Back> = HashMap::new();
        let mut last_edit: HashMap<PathBuf, (Option<String>, Option<u32>, bool)> = HashMap::new();
        for (i, ev) in self.recs.events.iter().enumerate() {
            match ev {
                Ev::Edit {
                    path,
                    pre,
                    pre_mode,
                    post,
                } => {
                    if i < start_at {
                        pre_edit.entry(path.clone()).or_insert(Back::Blob {
                            pre: pre.clone(),
                            mode: *pre_mode,
                        });
                    }
                    last_edit.insert(path.clone(), (post.clone(), *pre_mode, false));
                }
                Ev::Command => {
                    for e in last_edit.values_mut() {
                        e.2 = true;
                    }
                }
                Ev::Checkpoint { .. } => {}
            }
        }
        let mut paths: BTreeSet<PathBuf> = trees
            .by_turn
            .values()
            .filter(|c| !c.is_gitlink())
            .map(|c| c.path.clone())
            .collect();
        paths.extend(last_edit.keys().cloned());

        let mut planned: Vec<Planned> = Vec::new();
        let mut already: Vec<(PathBuf, Disk)> = Vec::new();
        let mut refused = Vec::new();
        for rel in paths {
            if trees.since_start.get(&rel).is_some_and(Change::is_gitlink) {
                continue;
            }
            let disk = read_disk(&root, &rel, MAX_DISK_READ).await?;
            let sides = trees.sides(&rel);
            let back = match pre_edit.get(&rel) {
                Some(b) => b.clone(),
                None => Back::Checkpoint {
                    present: match &sides {
                        Some((start, _, _)) => start.is_some(),
                        None => disk.exists(),
                    },
                },
            };
            // Where the trees stand: back at the start, and as the turn left it. A file
            // the work tree's tree leaves out (ignored now) is compared by its blob id.
            let (tree_target, tree_left) = match &sides {
                None => (true, true),
                Some((start, end, now)) => {
                    let here = match (now, disk.tree_mode()) {
                        (Some(_), _) | (None, None) => now.clone(),
                        (None, Some(mode)) => {
                            let id = match &disk {
                                Disk::File { bytes, .. } => {
                                    self.cp.blob_id(bytes.clone(), deadline).await?
                                }
                                Disk::Link(target) => {
                                    self.cp.blob_id(target.clone(), deadline).await?
                                }
                                Disk::Large { path, .. } => {
                                    self.cp.blob_id_of_file(path, deadline).await?
                                }
                                Disk::Absent | Disk::Other => unreachable!("no tree mode"),
                            };
                            Some((mode.to_string(), id))
                        }
                    };
                    (*start == here, *end == here)
                }
            };
            let at_target = match &back {
                Back::Blob { pre, mode } => {
                    let to = pre.as_ref().map(|b| self.reader.read_blob(b)).transpose()?;
                    disk.is(to.as_deref(), *mode)
                }
                // (A directory where the start had no file counts as no file.)
                Back::Checkpoint { .. } => tree_target,
            };
            if at_target || (!back.present() && !disk.exists()) {
                already.push((rel, disk));
                continue;
            }
            let as_left = match last_edit.get(&rel) {
                // The turn's tools wrote it last: what they wrote, in the mode they kept.
                Some((post, tip_mode, false)) => match &disk {
                    Disk::Absent => post.is_none(),
                    Disk::File { .. } | Disk::Large { .. } => {
                        disk.file_hash().is_some_and(|(hash, mode)| {
                            post.as_deref() == Some(hash.as_str())
                                && (tip_mode.is_none() || *tip_mode == Some(mode))
                        })
                    }
                    Disk::Link(_) | Disk::Other => false,
                },
                // A command may have changed it after: as in the turn's last checkpoint.
                _ => tree_left,
            };
            // Git replaces a directory in the way with everything below it: never.
            if !as_left || (back.present() && disk == Disk::Other) {
                refused.push(rel);
                continue;
            }
            planned.push(Planned { rel, back, disk });
        }
        if !refused.is_empty() {
            return Err(self.refusal(refused, &root, deadline).await);
        }
        // A file the first checkpoint lacks may have been there, ignored: deleted only
        // when the ignore rules of the turn's start (restored with it) do not ignore it.
        let unseen: Vec<PathBuf> = planned
            .iter()
            .filter(|p| matches!(p.back, Back::Checkpoint { present: false }))
            .map(|p| p.rel.clone())
            .collect();
        let kept = self.cp.ignored_under(start, &unseen, deadline).await?;
        planned.retain(|p| !kept.contains(&p.rel));
        let mut kept: Vec<PathBuf> = kept.into_iter().collect();
        kept.sort();

        let mut notes = Vec::new();
        if self.recs.ran_commands {
            notes.push(NOTE_GIT.to_string());
        }
        if self.recs.skipped || self.recs.end_skipped {
            notes.push(NOTE_SKIPPED.to_string());
        }
        if !kept.is_empty() {
            notes.push(format!("{NOTE_KEPT}: {}", list(&kept)));
        }
        let large: Vec<PathBuf> = planned
            .iter()
            .map(|p| (&p.rel, &p.disk))
            .chain(already.iter().map(|(rel, disk)| (rel, disk)))
            .filter(|(_, d)| matches!(d, Disk::Large { .. }))
            .map(|(rel, _)| rel.clone())
            .collect();
        if !large.is_empty() {
            notes.push(format!("{NOTE_LARGE}: {}", list(&large)));
        }
        let mut report = UndoReport {
            turn: self.turn,
            restored: Vec::new(),
            deleted: Vec::new(),
            dry_run,
            backup_ref: None,
            note: (!notes.is_empty()).then(|| notes.join("; ")),
        };
        for p in &planned {
            if p.back.present() {
                report.restored.push(root.join(&p.rel));
            } else {
                report.deleted.push(root.join(&p.rel));
            }
        }
        let Some(log) = log.filter(|_| !dry_run) else {
            return Ok(report);
        };
        if planned.is_empty() && already.is_empty() {
            return Ok(report);
        }
        let mut files = Vec::with_capacity(planned.len() + already.len());
        for (rel, disk) in already {
            let (bytes, mode) = disk.logged();
            let blob = bytes.map(|b| log.blob(b)).transpose()?;
            files.push(UndoneFile {
                path: rel_path(&root, &rel)?,
                from_blob: blob.clone(),
                from_mode: mode,
                to: blob,
                to_mode: mode,
            });
        }
        if !planned.is_empty() {
            let backup = self.cp.commit_backup(&now, deadline).await?;
            report.backup_ref = Some(backup.refname);
        }
        // Every file's content just before, as blobs, before the first write.
        let mut from = Vec::with_capacity(planned.len());
        for p in &planned {
            let (bytes, mode) = p.disk.logged();
            from.push((bytes.map(|b| log.blob(b)).transpose()?, mode));
        }
        let mut applied = vec![false; planned.len()];
        let written = self
            .write(&planned, &mut applied, start, &root, deadline)
            .await;
        self.cp.invalidate();
        // Only what was written is logged (a failed undo leaves the rest as it was).
        for ((p, (from_blob, from_mode)), done) in planned.iter().zip(from).zip(&applied) {
            if !done {
                continue;
            }
            let (to, to_mode) = match &p.back {
                Back::Blob { pre, mode } => (pre.clone(), pre.as_ref().and(*mode)),
                Back::Checkpoint { present: false } => (None, None),
                Back::Checkpoint { present: true } => {
                    // Written already, so it must be logged whatever its size: what
                    // the checkpoint held (bounded by its untracked-file limits for
                    // untracked files), not what a command left behind.
                    let disk = read_disk(&root, &p.rel, u64::MAX).await?;
                    let (bytes, mode) = disk.logged();
                    (bytes.map(|b| log.blob(b)).transpose()?, mode)
                }
            };
            files.push(UndoneFile {
                path: rel_path(&root, &p.rel)?,
                from_blob,
                from_mode,
                to,
                to_mode,
            });
        }
        log.record(&SessionRecord::Undone {
            turn: self.turn,
            files,
            backup_ref: report.backup_ref.clone(),
        })?;
        written?;
        Ok(report)
    }

    /// Delete what goes (links are unlinked, never followed), then write back what the
    /// first checkpoint or a pre-image holds, marking each path `applied` once written.
    async fn write(
        &self,
        planned: &[Planned],
        applied: &mut [bool],
        start: &str,
        root: &Path,
        deadline: tokio::time::Instant,
    ) -> Result<(), UndoError> {
        for (i, p) in planned.iter().enumerate() {
            if !p.back.present() {
                checkpoint::remove(self.sandbox, &root.join(&p.rel)).await?;
                applied[i] = true;
            }
        }
        let from_checkpoint: Vec<usize> = (0..planned.len())
            .filter(|&i| matches!(planned[i].back, Back::Checkpoint { present: true }))
            .collect();
        let rels: Vec<PathBuf> = from_checkpoint
            .iter()
            .map(|&i| planned[i].rel.clone())
            .collect();
        self.cp.checkout(start, &rels, deadline).await?;
        for i in from_checkpoint {
            applied[i] = true;
        }
        for (i, p) in planned.iter().enumerate() {
            let Back::Blob {
                pre: Some(pre),
                mode,
            } = &p.back
            else {
                continue;
            };
            let full = root.join(&p.rel);
            // A link in its place goes first: writing through it would change its target.
            if matches!(p.disk, Disk::Link(_)) {
                checkpoint::remove(self.sandbox, &full).await?;
            }
            let bytes = self.reader.read_blob(pre)?;
            self.sandbox
                .write_atomic(&full, &bytes, *mode)
                .await
                .map_err(|source| UndoError::Disk {
                    path: full.display().to_string(),
                    source,
                })?;
            applied[i] = true;
        }
        Ok(())
    }

    /// The error for paths that are not as the turn left them: a later turn that is not
    /// undone changed them (its tool edits, or between its checkpoints), else someone else.
    async fn refusal(
        &self,
        refused: Vec<PathBuf>,
        root: &Path,
        deadline: tokio::time::Instant,
    ) -> UndoError {
        let mut later = BTreeMap::<PathBuf, BTreeSet<u32>>::new();
        let mut turns: Vec<u32> = self
            .scan
            .per_turn
            .keys()
            .copied()
            .filter(|&k| k > self.turn)
            .collect();
        turns.sort_unstable();
        for k in turns {
            let recs = &self.scan.per_turn[&k];
            let undone = self.scan.undone.get(&k);
            let mut touched: HashSet<PathBuf> = recs
                .events
                .iter()
                .filter_map(|e| match e {
                    Ev::Edit { path, .. } => Some(path.clone()),
                    _ => None,
                })
                .collect();
            let cps: Vec<&str> = recs.checkpoints().map(|(_, c)| c).collect();
            if recs.ran_commands
                && let (Some(first), Some(last)) = (cps.first(), cps.last())
                && first != last
                && let Ok(changes) = self.cp.diff(first, last, deadline).await
            {
                touched.extend(changes.into_iter().map(|c| c.path));
            }
            for p in &refused {
                if touched.contains(p) && !undone.is_some_and(|u| u.contains(p)) {
                    later.entry(p.clone()).or_default().insert(k);
                }
            }
        }
        if later.is_empty() {
            return UndoError::Changed {
                paths: refused.iter().map(|p| root.join(p)).collect(),
            };
        }
        let turns: BTreeSet<u32> = later.values().flatten().copied().collect();
        UndoError::LaterTurns {
            turns: turns.into_iter().collect(),
            paths: later.keys().map(|p| root.join(p)).collect(),
        }
    }
}

/// Changes by path.
fn index_changes(changes: Vec<Change>) -> HashMap<PathBuf, Change> {
    changes.into_iter().map(|c| (c.path.clone(), c)).collect()
}

/// `rel` (below `root`) as a log path.
fn rel_path(root: &Path, rel: &Path) -> Result<RelPath, UndoError> {
    RelPath::from_abs(root, &root.join(rel)).ok_or_else(|| UndoError::Disk {
        path: rel.to_string_lossy().into_owned(),
        source: SandboxError::Denied("the path cannot be logged (it is not valid UTF-8)".into()),
    })
}

/// What is at `rel` below `root` now, as git sees it: no symbolic link is followed, and a
/// path below a link or a file is absent. A regular file longer than `limit` bytes is
/// hashed as it is read ([`Disk::Large`]) rather than read whole into memory (a command
/// can leave a multi-GB file behind).
pub(crate) async fn read_disk(root: &Path, rel: &Path, limit: u64) -> Result<Disk, UndoError> {
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::MetadataExt;
    let full = root.join(rel);
    let err = |e: std::io::Error| UndoError::Disk {
        path: full.display().to_string(),
        source: SandboxError::Io(e),
    };
    let (root, rel) = (root.to_path_buf(), rel.to_path_buf());
    let read = tokio::task::spawn_blocking(move || -> std::io::Result<Disk> {
        let names: Vec<_> = rel.components().collect();
        let mut at = root;
        for (i, name) in names.iter().enumerate() {
            at.push(name);
            let meta = match std::fs::symlink_metadata(&at) {
                Ok(m) => m,
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) =>
                {
                    return Ok(Disk::Absent);
                }
                Err(e) => return Err(e),
            };
            let kind = meta.file_type();
            if i + 1 < names.len() {
                if !kind.is_dir() {
                    return Ok(Disk::Absent);
                }
                continue;
            }
            return Ok(if kind.is_symlink() {
                Disk::Link(std::fs::read_link(&at)?.into_os_string().into_vec())
            } else if kind.is_file() {
                let mode = meta.mode() & 0o7777;
                match read_capped(&at, meta.len(), limit)? {
                    Ok(bytes) => Disk::File { bytes, mode },
                    Err(hash) => Disk::Large {
                        hash,
                        mode,
                        path: at.clone(),
                    },
                }
            } else {
                Disk::Other
            });
        }
        Ok(Disk::Other)
    });
    read.await
        .map_err(|e| err(std::io::Error::other(e)))?
        .map_err(err)
}

/// A file's content and permission bits now (none: it does not exist).
async fn read_now(
    sandbox: &dyn Sandbox,
    full: &Path,
) -> Result<(Option<Vec<u8>>, Option<u32>), UndoError> {
    let disk = |source: SandboxError| UndoError::Disk {
        path: full.display().to_string(),
        source,
    };
    let bytes = match sandbox.read(full).await {
        Ok(bytes) => bytes,
        Err(SandboxError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok((None, None));
        }
        Err(e) => return Err(disk(e)),
    };
    let mode = sandbox.metadata(full).await.map_err(disk)?.mode & 0o7777;
    Ok((Some(bytes), Some(mode)))
}

// ---------------------------------------------------------- the pre-image undo (Task 26)

/// One file to undo: its log path, what it is restored to (none: deleted), and what the
/// log says its content is now (blake3 hex; none: absent) and its mode (none: unknown, as
/// for a file a tool created).
struct Target {
    path: RelPath,
    pre: Option<BlobRef>,
    mode: Option<u32>,
    tip: Option<String>,
    tip_mode: Option<u32>,
}

/// Undo the file changes Tautly's tools made in user turn `turn` from their pre-images:
/// each file the turn changed gets the content and mode it had before the turn's first
/// change of it, newest change first; a file the turn created is deleted. Each file's
/// content just before is stored as a blob first, and the undo is logged as `Undone`.
/// Changes made by commands are not undone. A file already as it was before the turn is
/// not written but is listed in `Undone` too, so earlier turns that changed it can be
/// undone next. Nothing is logged when nothing was left to undo.
///
/// Refusals (before any write): [`UndoError::LaterTurns`] when a later turn changed one
/// of the files and is not undone; [`UndoError::Changed`] when a file's content or mode is
/// not what the log says the session left it at. With `cp`, a backup snapshot is taken
/// before the first write.
async fn preimage_undo(
    reader: &SessionReader<SessionRecord>,
    sandbox: &dyn Sandbox,
    cp: Option<&mut Checkpointer>,
    turn: Option<u32>,
    dry_run: bool,
    log: Option<&mut dyn UndoLog>,
    note: Option<String>,
) -> Result<UndoReport, UndoError> {
    let plan = undo_plan(reader, turn)?;
    let turn = plan.turn;
    if !plan.later.is_empty() {
        return Err(UndoError::LaterTurns {
            turns: plan.later_turns,
            paths: plan
                .later
                .iter()
                .map(|p| p.resolve(sandbox.root()))
                .collect::<Result<_, _>>()?,
        });
    }
    let disk = |path: &PathBuf, source: SandboxError| UndoError::Disk {
        path: path.display().to_string(),
        source,
    };
    // First what every file is now, checked against what the session left it at.
    let mut planned = Vec::with_capacity(plan.targets.len());
    let mut already = Vec::new();
    let mut changed = Vec::new();
    for t in plan.targets {
        let full = t.path.resolve(sandbox.root())?;
        let (now, now_mode) = read_now(sandbox, &full).await?;
        let to = t.pre.as_ref().map(|b| reader.read_blob(b)).transpose()?;
        if now == to && (to.is_none() || now_mode == t.mode) {
            // Nothing to write; logged as undone all the same (its content is the blob
            // it is restored to).
            already.push(UndoneFile {
                path: t.path,
                from_blob: t.pre.clone(),
                from_mode: now_mode,
                to_mode: to.as_ref().and(t.mode),
                to: t.pre,
            });
            continue;
        }
        let now_hash = now.as_deref().map(|b| blake3::hash(b).to_hex().to_string());
        let mode_kept = now.is_none() || t.tip_mode.is_none() || now_mode == t.tip_mode;
        if now_hash != t.tip || !mode_kept {
            changed.push(full);
            continue;
        }
        planned.push((t, full, now, to, now_mode));
    }
    if !changed.is_empty() {
        changed.sort();
        return Err(UndoError::Changed { paths: changed });
    }
    let mut report = UndoReport {
        turn,
        restored: Vec::new(),
        deleted: Vec::new(),
        dry_run,
        backup_ref: None,
        note,
    };
    for (_, full, now, to, _) in &planned {
        match to {
            Some(_) => report.restored.push(full.clone()),
            None if now.is_some() => report.deleted.push(full.clone()),
            None => {}
        }
    }
    let Some(log) = log.filter(|_| !dry_run) else {
        return Ok(report);
    };
    if planned.is_empty() && already.is_empty() {
        return Ok(report);
    }
    if let Some(cp) = cp
        && !planned.is_empty()
    {
        let deadline = Checkpointer::housekeeping_deadline();
        let tree = cp.tree_id(true, deadline).await?;
        report.backup_ref = Some(cp.commit_backup(&tree, deadline).await?.refname);
        cp.invalidate();
    }
    // Then their current content, as blobs.
    let mut blobbed = Vec::with_capacity(planned.len());
    for (t, full, now, to, now_mode) in planned {
        let from_blob = now.as_deref().map(|b| log.blob(b)).transpose()?;
        blobbed.push((t, full, to, from_blob, now_mode));
    }
    let mut files = already;
    let mut failed = None;
    let (mut restored, mut deleted) = (Vec::new(), Vec::new());
    for (t, full, to, from_blob, from_mode) in blobbed {
        let written = match &to {
            Some(bytes) => sandbox.write_atomic(&full, bytes, t.mode).await,
            None if from_blob.is_some() => sandbox.remove_file(&full).await,
            None => Ok(()),
        };
        if let Err(e) = written {
            failed = Some(disk(&full, e));
            break;
        }
        match &to {
            Some(_) => restored.push(full),
            None if from_blob.is_some() => deleted.push(full),
            None => {}
        }
        files.push(UndoneFile {
            path: t.path,
            from_blob,
            from_mode,
            to: t.pre,
            to_mode: to.as_ref().and(t.mode),
        });
    }
    log.record(&SessionRecord::Undone {
        turn,
        files,
        backup_ref: report.backup_ref.clone(),
    })?;
    match failed {
        Some(e) => Err(e),
        None => {
            report.restored = restored;
            report.deleted = deleted;
            Ok(report)
        }
    }
}

/// What [`preimage_undo`] does for one user turn.
struct UndoPlan {
    turn: u32,
    /// Its files, newest change first; changes an earlier undo of the turn reverted are
    /// left out.
    targets: Vec<Target>,
    /// Of those, the files a later turn changed (and was not undone for), and those turns.
    later: Vec<RelPath>,
    later_turns: Vec<u32>,
}

/// The user turn to undo, its files and what the log says of them since.
fn undo_plan(
    reader: &SessionReader<SessionRecord>,
    turn: Option<u32>,
) -> Result<UndoPlan, UndoError> {
    let mut current = 0u32;
    let mut turns = 0u32;
    // Per user turn: each path's earliest pre-image, in order of its latest change.
    let mut changes: HashMap<u32, Vec<Target>> = HashMap::new();
    // Per path: its content (and mode, if known) as the session last left it, and the
    // turns that changed it (in order), less those an undo reverted.
    let mut tips: HashMap<RelPath, (Option<String>, Option<u32>)> = HashMap::new();
    let mut changed_in: HashMap<RelPath, Vec<u32>> = HashMap::new();
    for rec in reader.records() {
        match rec?.1 {
            SessionRecord::TurnStart { turn, .. } => {
                current = turn;
                turns = turns.max(turn);
            }
            SessionRecord::ToolResult { effects, .. } => {
                for e in effects {
                    // An edit keeps the mode it found (none: it created the file, whose
                    // mode is not logged).
                    let (path, pre, mode, tip) = match e {
                        PersistedEffect::Edited {
                            path,
                            pre_image_blob,
                            pre_mode,
                            post_hash,
                            ..
                        } => (
                            path,
                            pre_image_blob,
                            pre_mode,
                            (Some(post_hash.to_hex().to_string()), pre_mode),
                        ),
                        PersistedEffect::Deleted {
                            path,
                            pre_image_blob,
                            pre_mode,
                        } => (path, Some(pre_image_blob), Some(pre_mode), (None, None)),
                        _ => continue,
                    };
                    let pre = pre.map(|b| BlobRef::parse(&b)).transpose()?;
                    tips.insert(path.clone(), tip);
                    let by = changed_in.entry(path.clone()).or_default();
                    if by.last() != Some(&current) {
                        by.push(current);
                    }
                    let list = changes.entry(current).or_default();
                    let first = match list.iter().position(|t| t.path == path) {
                        Some(i) => list.remove(i),
                        None => Target {
                            path,
                            pre,
                            mode,
                            tip: None,
                            tip_mode: None,
                        },
                    };
                    list.push(first);
                }
            }
            SessionRecord::Undone { turn, files, .. } => {
                for f in files {
                    // A blob reference is the blake3 hex of its content.
                    tips.insert(
                        f.path.clone(),
                        (f.to.map(|b| b.as_str().to_string()), f.to_mode),
                    );
                    if let Some(by) = changed_in.get_mut(&f.path) {
                        by.retain(|&t| t != turn);
                    }
                    if let Some(list) = changes.get_mut(&turn) {
                        list.retain(|t| t.path != f.path);
                    }
                }
            }
            _ => {}
        }
    }
    let turn = turn.unwrap_or(turns);
    if turn == 0 || turn > turns {
        return Err(UndoError::NoSuchTurn { turn, turns });
    }
    let mut targets = changes.remove(&turn).unwrap_or_default();
    targets.reverse();
    let mut later = Vec::new();
    let mut later_turns = Vec::new();
    for t in &mut targets {
        (t.tip, t.tip_mode) = tips.get(&t.path).cloned().unwrap_or_default();
        let after: Vec<u32> = changed_in
            .get(&t.path)
            .into_iter()
            .flatten()
            .copied()
            .filter(|&k| k > turn)
            .collect();
        if !after.is_empty() {
            later.push(t.path.clone());
            later_turns.extend(after);
        }
    }
    later.sort();
    later_turns.sort_unstable();
    later_turns.dedup();
    Ok(UndoPlan {
        turn,
        targets,
        later,
        later_turns,
    })
}

// ------------------------------------------------------------------ the engine's undo

impl Session {
    /// `Command::Undo`, as a job the client can cancel while it waits for the
    /// session-start checkpoint (the undo reads it from the log). Once the undo starts it
    /// runs to its end (each git step is bounded), so no file is left half restored.
    pub(crate) async fn undo_job(&mut self, turn: Option<u32>, dry_run: bool, ctl: &TurnControl) {
        let unbound = self.events.clone();
        self.events = unbound.bound(ctl.cancel.clone());
        if self.settle_start_checkpoint_or(&ctl.cancel).await && !ctl.cancel.is_cancelled() {
            self.undo_command(turn, dry_run).await;
        } else {
            self.emit(Event::Error {
                id: None,
                kind: ErrorKind::Tool,
                message: "undo cancelled: nothing was changed".into(),
                retryable: false,
                hint: None,
            })
            .await;
        }
        self.events = unbound;
    }

    /// `Command::Undo` (between turns): undo user turn `turn` of this session through its
    /// own log, answered with `Undone` or an `Error`. The files it wrote are no longer
    /// known as the model last saw them.
    async fn undo_command(&mut self, turn: Option<u32>, dry_run: bool) {
        // Make everything logged so far readable.
        self.checkpoint().await;
        // Exclusive across the check and the write, as for rewind and switch: another live
        // session's tool must not write in between (CP5 review M3).
        let _lease = match (!dry_run).then(|| self.exclusive()).transpose() {
            Ok(guard) => guard,
            Err(e) => return self.time_travel_failed("undo", e).await,
        };
        let path = self.log.path();
        let guard = match &self.checkpoints {
            Some(c) => Some(Arc::clone(c).lock_owned().await),
            None => None,
        };
        let sandbox = Arc::clone(&self.sandbox);
        let log = self.log.clone();
        // The undo reads the whole log and its blobs, writes and fsyncs the log, and reads
        // and writes files: all blocking I/O, so it runs off the async workers, on a
        // runtime of its own for its git steps.
        let result = tokio::task::spawn_blocking(move || {
            let reader = SessionReader::<SessionRecord>::open(&path)?;
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| UndoError::Log(e.to_string()))?;
            let mut guard = guard;
            let mut log = LiveLog(log);
            rt.block_on(undo_with(
                &reader,
                sandbox.as_ref(),
                guard.as_deref_mut().map(|g| &mut g.cp),
                turn,
                dry_run,
                (!dry_run).then_some(&mut log as &mut dyn UndoLog),
            ))
        })
        .await
        .unwrap_or_else(|e| Err(UndoError::Log(e.to_string())));
        match result {
            Ok(report) => {
                if !report.dry_run {
                    self.forget_undone(report.restored.iter().chain(&report.deleted));
                }
                self.emit(report.event(self.sandbox.root())).await;
            }
            Err(e) => self.undo_failed(e).await,
        }
    }

    /// Files an undo wrote are not what the model last saw: their notes are stale, their
    /// read baselines gone, and the write generation moves (as a rebuild does for
    /// `Undone`).
    fn forget_undone<'a>(&mut self, paths: impl Iterator<Item = &'a PathBuf>) {
        for p in paths {
            if let Some(note) = self.context.state.files.get_mut(p) {
                note.maybe_stale = true;
            }
            self.reads.forget_path(p);
        }
        self.ledger
            .note_effects(&[AppliedEffect::Exec { may_write: true }]);
    }

    async fn undo_failed(&self, e: UndoError) {
        let hint = match &e {
            UndoError::LaterTurns { turns, .. } => Some(format!(
                "undo the later turns first, newest first ({})",
                turns
                    .iter()
                    .map(u32::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
            UndoError::Changed { .. } => Some(
                "nothing was changed; the files were edited after the turn (by you or \
                 another program)"
                    .to_string(),
            ),
            _ => None,
        };
        self.emit(Event::Error {
            id: None,
            kind: ErrorKind::Tool,
            message: format!("undo: {e}"),
            retryable: false,
            hint,
        })
        .await;
    }
}

/// The bytes of the regular file at `path` (`len` long when looked at), or (`Err`) the
/// blake3 of its content when it is longer than `limit`: read in pieces, never held whole.
fn read_capped(
    path: &Path,
    len: u64,
    limit: u64,
) -> std::io::Result<Result<Vec<u8>, blake3::Hash>> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    // Sized from the length seen, so reading never doubles the buffer past the limit.
    let size = len.min(limit.saturating_add(1));
    let mut bytes = Vec::with_capacity(usize::try_from(size).unwrap_or(0));
    (&mut file)
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 <= limit {
        return Ok(Ok(bytes));
    }
    let mut hasher = blake3::Hasher::new();
    hasher.update(&bytes);
    drop(bytes);
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(Err(hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Review B-M6/N8: a file over the limit is hashed as it is read, never held whole.
    #[tokio::test]
    async fn read_disk_hashes_a_file_over_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(root.join("small"), b"0123456789").unwrap();
        std::fs::write(root.join("big"), b"0123456789A").unwrap();
        match read_disk(&root, Path::new("small"), 10).await.unwrap() {
            Disk::File { bytes, .. } => assert_eq!(bytes, b"0123456789"),
            other => panic!("{other:?}"),
        }
        match read_disk(&root, Path::new("big"), 10).await.unwrap() {
            Disk::Large { hash, path, .. } => {
                assert_eq!(hash, blake3::hash(b"0123456789A"));
                assert_eq!(path, root.join("big"));
            }
            other => panic!("{other:?}"),
        }
        let large = read_disk(&root, Path::new("big"), 10).await.unwrap();
        assert!(large.is(Some(b"0123456789A"), Some(0o644)));
        assert!(!large.is(Some(b"0123456789B"), Some(0o644)));
        assert_eq!(large.logged(), (None, Some(0o644)));
        // No limit (what was just written must be logged).
        assert!(matches!(
            read_disk(&root, Path::new("big"), u64::MAX).await.unwrap(),
            Disk::File { .. }
        ));
        // Links and absent paths are not files: no limit applies.
        std::os::unix::fs::symlink("big", root.join("link")).unwrap();
        assert!(matches!(
            read_disk(&root, Path::new("link"), 1).await.unwrap(),
            Disk::Link(_)
        ));
        assert_eq!(
            read_disk(&root, Path::new("none"), 1).await.unwrap(),
            Disk::Absent
        );
    }
}
