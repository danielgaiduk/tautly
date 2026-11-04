//! Time travel (spec 7.12): the files at a step boundary ([`state_at`]), the files the
//! agent left ([`expected_tip`]), and materializing one in place of the other without
//! clobbering human edits ([`materialize`]); plus the engine's `Rewind` and `Switch`.
//!
//! # File states
//!
//! A [`FileState`] is a checkpoint commit (`base`) with an overlay of files from the
//! session's blob store. [`state_at`] goes by log offset (spec 7.12.1): the first
//! `Checkpoint` after the last record of step N, with every `Edit` (to its pre-image) and
//! `Undone` (to its `from_blob`) between them reverted. A skipped checkpoint between, a
//! `resume_start` or `before_restore` checkpoint, one whose `point` is missing or unknown
//! (with no `turn_end` checkpoint of the step's turn before it), writes of unknown outcome
//! between, or no checkpoint at all make the state *partial* (its `partial` says why).
//! [`expected_tip`] is the latest checkpoint other than a `before_restore` one (which
//! holds whatever was on disk before a restore, human edits included) with the later
//! tool-edit post-images and undos applied.
//!
//! # Materializing
//!
//! [`materialize`] changes only the paths where the target differs from the expected tip.
//! A path that would change and whose disk content is not the expected one (or whose
//! expected content is not known) is a conflict: unless every conflict is listed in
//! `overwrite`, nothing is written. A directory is never removed or written over. Before
//! the first write a backup snapshot is taken (`undo-<n>`, with git checkpoints); a write
//! that fails puts the backup back. Contents are compared as git blob ids (with a
//! checkpointer) or blake3 digests (without); disk files are read without following links,
//! large ones by a streaming hash.
//!
//! # Rewind and switch
//!
//! `Command::Rewind` (spec 7.12.2): the exclusive working-tree lease, a `before_restore`
//! checkpoint appended to the current session (so its tip stays restorable), the target
//! and expected states, the dry run's report; when applied: backup, materialize, the branch
//! log (a fork with `origin: rewind`, whose own last record is a `restored` checkpoint of
//! exactly the materialized state, or outside git an `Edit` per overlay file asserting its
//! content), its checkpoint refs, its rebuilt context. A failure after the files changed
//! puts the backup back. `Command::Switch` (spec 7.12.3) materializes another session's
//! [`expected_tip`] the same way and continues that session.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tautly_sandbox::{Sandbox, SandboxError};
use tautly_session::{
    BlobRef, ConfineError, HeaderInit, Origin, RelPath, SessionReader, SessionWriter,
};
use tautly_tools::PersistedEffect;

use crate::checkpoint::{self, CheckpointError, Checkpointer, Point, Skip};
use crate::lease::{ExclusiveGuard, LeaseError};
use crate::protocol::{ErrorKind, Event, FinishReason};
use crate::record::{CheckpointPoint, SessionRecord};
use crate::resume::{Continue, Rebuilt, ResumeError, branch, discard, rebuild, rebuild_with};
use crate::session::Session;
use crate::turn::TurnControl;
use crate::undo::{Disk, MAX_DISK_READ, UndoError, read_disk};

/// The note of a state with no checkpoint after its boundary.
const NOTE_NO_CHECKPOINT: &str = "only tool edits are restored; command effects are not \
     (no checkpoint follows that step: outside git, checkpoints off, or the session stopped \
     mid-turn)";
/// The note of a materialize with git checkpoints.
const NOTE_GIT: &str = "ignored files and submodules are not restored";

/// The files of a session at some point: the tree of checkpoint commit `base` with
/// `overlay` applied (each path's content as a blob of the log's store; none: absent).
/// Without a `base`, paths outside `overlay` are not known (left as they are when
/// materialized). `partial` says why the state may not be exact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileState {
    pub base: Option<String>,
    pub overlay: Vec<(RelPath, Option<BlobRef>)>,
    pub partial: Option<String>,
    /// The log it was read from: its blob store holds the overlay's contents.
    log: PathBuf,
}

impl FileState {
    /// The log the state was read from.
    pub fn log(&self) -> &Path {
        &self.log
    }
}

/// What materializing a state did (with `dry_run`: would do). Paths are root-relative.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MaterializeReport {
    /// Files written (with `applied`; else that would be written) with the target's
    /// content, conflicts included.
    pub restored: Vec<String>,
    /// Files deleted (or that would be), conflicts included.
    pub deleted: Vec<String>,
    /// Paths that would change and whose content is not what the agent left (a human
    /// edit): nothing is written unless each is listed in `overwrite`.
    pub conflicts: Vec<String>,
    pub dry_run: bool,
    /// The files were made the target (nothing was left to write counts too).
    pub applied: bool,
    /// The snapshot taken before the first write.
    pub backup_ref: Option<String>,
    /// What was not restored, or why the state may not be exact.
    pub note: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum TtError {
    #[error("cannot read or write the session log: {0}")]
    Session(#[from] tautly_session::Error),
    #[error("there is no step {at} in this session; its last step is {last}")]
    NoSuchStep { at: u32, last: u32 },
    #[error("a path in the session log cannot be used here: {0}")]
    Path(#[from] ConfineError),
    #[error("`{0}` is not a path inside the work tree")]
    BadPath(String),
    #[error("{path}: {source}")]
    Disk { path: String, source: SandboxError },
    #[error("git checkpoint: {0}")]
    Checkpoint(#[from] CheckpointError),
    #[error("{0}")]
    Lease(#[from] LeaseError),
    #[error("this engine holds no working-tree lease, so it cannot rewind or switch")]
    NoLease,
    #[error("cancelled; nothing was changed")]
    Cancelled,
    #[error("no session {0} ran in this work tree")]
    NotFound(String),
    #[error("already on session {0}")]
    SameSession(String),
    #[error("{0}")]
    Other(String),
}

impl From<ResumeError> for TtError {
    fn from(e: ResumeError) -> Self {
        match e {
            ResumeError::Session(e) => TtError::Session(e),
            ResumeError::NoSuchStep { at, last } => TtError::NoSuchStep { at, last },
        }
    }
}

impl From<UndoError> for TtError {
    fn from(e: UndoError) -> Self {
        match e {
            UndoError::Disk { path, source } => TtError::Disk { path, source },
            UndoError::Session(e) => TtError::Session(e),
            UndoError::Checkpoint(e) => TtError::Checkpoint(e),
            other => TtError::Other(other.to_string()),
        }
    }
}

impl From<Skip> for TtError {
    fn from(s: Skip) -> Self {
        TtError::Checkpoint(s.into())
    }
}

// ------------------------------------------------------------------- the log, walked

/// A log's records, with the step each belongs to: a step's `Request`, `Provider`,
/// `ToolCall`, `ToolResult` and `Edit` records, and a user message's `TurnStart`.
struct Walked {
    recs: Vec<SessionRecord>,
    /// Per record: `(step, user turn)` of the step it is part of.
    owner: Vec<Option<(u32, u32)>>,
    last_step: u32,
}

/// Steps as the context turn counter numbers them (spec 7.12.1): a user message is the
/// step after the last one, a provider step is its `Request`'s.
#[derive(Default)]
struct StepCounter {
    last: u32,
    open: Option<(u32, u32)>,
}

impl StepCounter {
    /// The step `rec` is part of, if any.
    fn see(&mut self, rec: &SessionRecord) -> Option<(u32, u32)> {
        match rec {
            SessionRecord::TurnStart { turn, .. } => {
                self.last += 1;
                self.open = None;
                Some((self.last, *turn))
            }
            SessionRecord::Request { step, turn, .. } => {
                self.last = *step;
                self.open = Some((*step, *turn));
                self.open
            }
            SessionRecord::TurnEnd { .. } => {
                self.open = None;
                None
            }
            SessionRecord::Provider(_)
            | SessionRecord::ToolCall { .. }
            | SessionRecord::ToolResult { .. }
            | SessionRecord::Edit { .. } => self.open,
            _ => None,
        }
    }
}

fn walk(reader: &SessionReader<SessionRecord>) -> Result<Walked, TtError> {
    let mut steps = StepCounter::default();
    let mut recs = Vec::new();
    let mut owner = Vec::new();
    for rec in reader.records() {
        let (_, rec) = rec?;
        owner.push(steps.see(&rec));
        recs.push(rec);
    }
    Ok(Walked {
        recs,
        owner,
        last_step: steps.last,
    })
}

/// The step whose last record is at `offset` or before: for a fork's `Header.parent`
/// offset, which names the last record its log copied, the step it branched at. (Spec
/// 7.12.3 words it as "the step whose last record is at `offset − 1`", counting the
/// offset past the copy; the header stores the last record's own offset.) 0 before the
/// first step. A record that cannot be read ends the count there.
pub fn step_at_offset(reader: &SessionReader<SessionRecord>, offset: u64) -> u32 {
    let mut steps = StepCounter::default();
    for rec in reader.records() {
        let Ok((at, rec)) = rec else {
            break;
        };
        if at > offset {
            break;
        }
        steps.see(&rec);
    }
    steps.last
}

/// The overlay of reverts in log order: each path's earliest value wins.
fn fold(reverts: Vec<(RelPath, Option<BlobRef>)>) -> Vec<(RelPath, Option<BlobRef>)> {
    let mut map = BTreeMap::new();
    for (path, value) in reverts.into_iter().rev() {
        map.insert(path, value);
    }
    map.into_iter().collect()
}

/// The files at the end of step `at` (spec 7.12.1; `at = 0`: before the first step): the
/// first `Checkpoint` after the step's last record, with the `Edit`s and `Undone`s between
/// reverted; see the module docs for when it is partial. A step past the log's last is
/// [`TtError::NoSuchStep`]. Blocking I/O (the log).
pub fn state_at(reader: &SessionReader<SessionRecord>, at: u32) -> Result<FileState, TtError> {
    let w = walk(reader)?;
    let no_step = || TtError::NoSuchStep {
        at,
        last: w.last_step,
    };
    if at > w.last_step {
        return Err(no_step());
    }
    let (start, turn) = if at == 0 {
        (0, None)
    } else {
        let last = w
            .owner
            .iter()
            .rposition(|o| o.is_some_and(|(s, _)| s == at))
            .ok_or_else(no_step)?;
        (last + 1, w.owner[last].map(|(_, t)| t))
    };
    let turn_end_seen = turn.is_some_and(|t| {
        w.recs[..start].iter().any(|r| {
            matches!(r, SessionRecord::Checkpoint {
                point: Some(CheckpointPoint::TurnEnd),
                turn,
                ..
            } if *turn == t)
        })
    });
    let mut reverts = Vec::new();
    let (mut skipped, mut lost) = (false, false);
    for rec in &w.recs[start..] {
        match rec {
            SessionRecord::Edit {
                path, pre_image, ..
            } => reverts.push((path.clone(), pre_image.clone())),
            SessionRecord::Undone { files, .. } => {
                reverts.extend(files.iter().map(|f| (f.path.clone(), f.from_blob.clone())));
            }
            SessionRecord::CheckpointSkipped { .. } => skipped = true,
            SessionRecord::WritesLost { .. } => lost = true,
            SessionRecord::Checkpoint { commit, point, .. } => {
                let mut notes = Vec::new();
                if skipped {
                    notes.push(format!(
                        "a checkpoint after step {at} was skipped, so command effects may not \
                         be restored exactly"
                    ));
                }
                if lost {
                    notes.push(format!(
                        "writes of unknown outcome happened after step {at}"
                    ));
                }
                match point {
                    Some(CheckpointPoint::ResumeStart | CheckpointPoint::BeforeRestore) => {
                        notes.push(format!(
                            "the checkpoint after step {at} was taken when the session resumed \
                             or before a restore, and may hold changes made outside the agent"
                        ));
                    }
                    None | Some(CheckpointPoint::Unknown) if !turn_end_seen => {
                        notes.push(format!(
                            "the checkpoint after step {at} was written by another version of \
                             tautly and is read as partial"
                        ));
                    }
                    _ => {}
                }
                return Ok(FileState {
                    base: Some(commit.clone()),
                    overlay: fold(reverts),
                    partial: (!notes.is_empty()).then(|| notes.join("; ")),
                    log: reader.path().to_path_buf(),
                });
            }
            _ => {}
        }
    }
    Ok(FileState {
        base: None,
        overlay: fold(reverts),
        partial: Some(NOTE_NO_CHECKPOINT.to_string()),
        log: reader.path().to_path_buf(),
    })
}

/// The files as the agent left them (spec 7.12.1): the latest checkpoint (other than a
/// `before_restore` one) with the post-images of later tool edits and the results of later
/// undos applied. A tool edit whose result was not captured leaves its path out (it is then
/// compared with the checkpoint's) and makes the state partial. Blocking I/O (the log).
pub fn expected_tip(reader: &SessionReader<SessionRecord>) -> Result<FileState, TtError> {
    let recs: Vec<SessionRecord> = reader
        .records()
        .map(|r| r.map(|(_, rec)| rec))
        .collect::<Result<_, _>>()?;
    let base_at = recs.iter().rposition(|r| {
        matches!(r, SessionRecord::Checkpoint { point, .. }
            if *point != Some(CheckpointPoint::BeforeRestore))
    });
    let base = base_at.map(|i| match &recs[i] {
        SessionRecord::Checkpoint { commit, .. } => commit.clone(),
        _ => unreachable!("found as a checkpoint"),
    });
    let mut overlay: BTreeMap<RelPath, Option<BlobRef>> = BTreeMap::new();
    let mut unknown: BTreeSet<RelPath> = BTreeSet::new();
    // What the `ToolResult` an `Edit` follows says of its path: deleted or edited. An
    // `Edit` that follows none is a rewind's assertion of a file's content.
    let mut kinds: HashMap<RelPath, bool> = HashMap::new();
    for rec in &recs[base_at.map_or(0, |i| i + 1)..] {
        match rec {
            SessionRecord::ToolResult { effects, .. } => {
                kinds.clear();
                for e in effects {
                    match e {
                        PersistedEffect::Edited { path, .. } => {
                            kinds.insert(path.clone(), false);
                        }
                        PersistedEffect::Deleted { path, .. } => {
                            kinds.insert(path.clone(), true);
                        }
                        _ => {}
                    }
                }
            }
            SessionRecord::Edit {
                path,
                pre_image,
                post_image_blob,
                ..
            } => {
                let value = match (post_image_blob, kinds.get(path)) {
                    (Some(post), _) => Some(Some(post.clone())),
                    (None, Some(true)) => Some(None),
                    (None, Some(false)) => None,
                    (None, None) => pre_image.is_none().then_some(None),
                };
                match value {
                    Some(v) => {
                        overlay.insert(path.clone(), v);
                        unknown.remove(path);
                    }
                    None => {
                        overlay.remove(path);
                        unknown.insert(path.clone());
                    }
                }
            }
            SessionRecord::Undone { files, .. } => {
                kinds.clear();
                for f in files {
                    overlay.insert(f.path.clone(), f.to.clone());
                    unknown.remove(&f.path);
                }
            }
            _ => kinds.clear(),
        }
    }
    let partial = (!unknown.is_empty()).then(|| {
        let names: Vec<&str> = unknown.iter().map(RelPath::as_str).collect();
        format!(
            "the content a tool left in {} was not captured",
            names.join(", ")
        )
    });
    Ok(FileState {
        base,
        overlay: overlay.into_iter().collect(),
        partial,
        log: reader.path().to_path_buf(),
    })
}

// -------------------------------------------------------------------- materializing

/// A file's content as compared: a blob id (a git blob id with a checkpointer, else the
/// blake3 hex) and its tree mode when known.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Val {
    Absent,
    Blob {
        id: String,
        mode: Option<String>,
    },
    /// A directory or another kind of file.
    Other,
}

const LINK_MODE: &str = "120000";

fn same(a: &Val, b: &Val) -> bool {
    match (a, b) {
        (Val::Absent, Val::Absent) => true,
        (Val::Blob { id: x, mode: m }, Val::Blob { id: y, mode: n }) => {
            x == y
                && match (m, n) {
                    (Some(m), Some(n)) => m == n,
                    // A content without a mode is a file's, never a link's.
                    (Some(l), None) | (None, Some(l)) => l != LINK_MODE,
                    (None, None) => true,
                }
        }
        _ => false,
    }
}

/// Where a path's target content comes from.
#[derive(Debug, Clone)]
enum Src {
    /// The target's base commit.
    Base,
    /// A blob of the target's log.
    Blob(BlobRef),
    Absent,
}

/// A path to write.
struct Planned {
    rel: PathBuf,
    src: Src,
    disk: Disk,
}

/// How to put the files back if a write fails half way.
pub(crate) enum Rollback {
    /// The backup snapshot's commit.
    Git(String),
    /// Each written path's content before (outside git).
    Files(Vec<(PathBuf, Disk)>),
}

/// Blob contents and ids, read from the states' logs once each.
struct Blobs<'a> {
    cp: Option<&'a Checkpointer>,
    readers: HashMap<PathBuf, SessionReader<SessionRecord>>,
    ids: HashMap<BlobRef, String>,
}

impl Blobs<'_> {
    fn bytes(&mut self, log: &Path, b: &BlobRef) -> Result<Vec<u8>, TtError> {
        if !self.readers.contains_key(log) {
            let reader = SessionReader::<SessionRecord>::open(log)?;
            self.readers.insert(log.to_path_buf(), reader);
        }
        Ok(self.readers[log].read_blob(b)?)
    }

    /// The id a blob is compared by.
    async fn id(&mut self, log: &Path, b: &BlobRef) -> Result<String, TtError> {
        let Some(cp) = self.cp else {
            return Ok(b.as_str().to_string());
        };
        if let Some(id) = self.ids.get(b) {
            return Ok(id.clone());
        }
        let bytes = self.bytes(log, b)?;
        let id = cp
            .blob_id(bytes, Checkpointer::housekeeping_deadline())
            .await?;
        self.ids.insert(b.clone(), id.clone());
        Ok(id)
    }
}

/// What is at a path, as compared.
async fn disk_val(cp: Option<&Checkpointer>, disk: &Disk) -> Result<Val, TtError> {
    let deadline = Checkpointer::housekeeping_deadline();
    let mode = |m: u32| {
        if m & 0o100 != 0 {
            "100755".to_string()
        } else {
            "100644".to_string()
        }
    };
    Ok(match (disk, cp) {
        (Disk::Absent, _) => Val::Absent,
        (Disk::Other, _) => Val::Other,
        (Disk::File { bytes, mode: m }, Some(cp)) => Val::Blob {
            id: cp.blob_id(bytes.clone(), deadline).await?,
            mode: Some(mode(*m)),
        },
        (Disk::File { bytes, mode: m }, None) => Val::Blob {
            id: blake3::hash(bytes).to_hex().to_string(),
            mode: Some(mode(*m)),
        },
        (Disk::Large { path, mode: m, .. }, Some(cp)) => Val::Blob {
            id: cp.blob_id_of_file(path, deadline).await?,
            mode: Some(mode(*m)),
        },
        (Disk::Large { hash, mode: m, .. }, None) => Val::Blob {
            id: hash.to_hex().to_string(),
            mode: Some(mode(*m)),
        },
        (Disk::Link(target), Some(cp)) => Val::Blob {
            id: cp.blob_id(target.clone(), deadline).await?,
            mode: Some(LINK_MODE.into()),
        },
        (Disk::Link(target), None) => Val::Blob {
            id: blake3::hash(target).to_hex().to_string(),
            mode: Some(LINK_MODE.into()),
        },
    })
}

/// A state's overlay by root-relative path, each path checked to stay inside the root.
fn overlay_of(root: &Path, s: &FileState) -> Result<HashMap<PathBuf, Option<BlobRef>>, TtError> {
    let mut out = HashMap::new();
    for (p, b) in &s.overlay {
        p.resolve(root)?;
        out.insert(PathBuf::from(p.as_str()), b.clone());
    }
    Ok(out)
}

/// One state's view of the paths: its overlay, its base's entries.
struct View<'s> {
    state: &'s FileState,
    overlay: HashMap<PathBuf, Option<BlobRef>>,
    /// The base's entries, when it has a base that can be read.
    entries: Option<HashMap<PathBuf, (String, String)>>,
}

impl View<'_> {
    /// The content at `rel` and where it comes from; none when the state does not know it.
    async fn value(
        &self,
        blobs: &mut Blobs<'_>,
        rel: &Path,
    ) -> Result<Option<(Val, Src)>, TtError> {
        if let Some(v) = self.overlay.get(rel) {
            return Ok(Some(match v {
                Some(b) => (
                    Val::Blob {
                        id: blobs.id(&self.state.log, b).await?,
                        mode: None,
                    },
                    Src::Blob(b.clone()),
                ),
                None => (Val::Absent, Src::Absent),
            }));
        }
        let Some(entries) = &self.entries else {
            return Ok(None);
        };
        Ok(Some(match entries.get(rel) {
            Some((mode, id)) => (
                Val::Blob {
                    id: id.clone(),
                    mode: Some(mode.clone()),
                },
                Src::Base,
            ),
            None => (Val::Absent, Src::Absent),
        }))
    }
}

/// Make the work tree `target` where it differs from `expected` (spec 7.12.2): see the
/// module docs. `cp` is the work tree's checkpointer (none outside git or with checkpoints
/// off: then only overlays count, and command effects are not restored). With `dry_run`,
/// or while conflicts remain that `overwrite` does not list, nothing is written.
pub async fn materialize(
    sb: &dyn Sandbox,
    cp: Option<&mut Checkpointer>,
    target: &FileState,
    expected: &FileState,
    overwrite: &[RelPath],
    dry_run: bool,
) -> Result<MaterializeReport, TtError> {
    Ok(
        materialize_inner(sb, cp, target, expected, overwrite, dry_run)
            .await?
            .0,
    )
}

/// [`materialize`], with how to put the files back when it wrote (for a caller that fails
/// after it).
pub(crate) async fn materialize_inner(
    sb: &dyn Sandbox,
    mut cp: Option<&mut Checkpointer>,
    target: &FileState,
    expected: &FileState,
    overwrite: &[RelPath],
    dry_run: bool,
) -> Result<(MaterializeReport, Option<Rollback>), TtError> {
    let root = sb.root().to_path_buf();
    let deadline = Checkpointer::housekeeping_deadline();
    let overwrite: Vec<PathBuf> = overwrite
        .iter()
        .map(|p| p.resolve(&root).map(|_| PathBuf::from(p.as_str())))
        .collect::<Result<_, _>>()?;
    let mut notes: Vec<String> = Vec::new();
    match cp.as_deref() {
        Some(_) => notes.push(NOTE_GIT.to_string()),
        None if target.base.is_some() => notes.push(
            "only tool edits are restored; command effects are not (git checkpoints are off)"
                .to_string(),
        ),
        None if std::fs::symlink_metadata(root.join(".git")).is_ok() => notes.push(
            "only tool edits are restored; command effects are not (git checkpoints are off)"
                .to_string(),
        ),
        None => notes.push(
            "only tool edits are restored; command effects are not (not a git repository)"
                .to_string(),
        ),
    }
    notes.extend(target.partial.clone());
    let (tbase, ebase) = match cp.as_deref() {
        Some(_) => (target.base.clone(), expected.base.clone()),
        None => (None, None),
    };
    let t_over = overlay_of(&root, target)?;
    let e_over = overlay_of(&root, expected)?;

    // The paths that may change.
    let mut cands: BTreeSet<PathBuf> = t_over.keys().chain(e_over.keys()).cloned().collect();
    if let Some(c) = cp.as_deref() {
        let changes = match (&tbase, &ebase) {
            (Some(t), Some(e)) if t != e => c.diff(e, t, deadline).await?,
            // The agent's tip is not known as a tree: every difference from the disk.
            (Some(t), None) => {
                let now = c.tree_id(true, deadline).await?;
                c.diff(&now, t, deadline).await?
            }
            _ => Vec::new(),
        };
        cands.extend(
            changes
                .into_iter()
                .filter(|ch| !ch.is_gitlink())
                .map(|ch| ch.path),
        );
    }
    let list: Vec<PathBuf> = cands.iter().cloned().collect();
    let tview = View {
        state: target,
        overlay: t_over,
        entries: base_entries(cp.as_deref(), tbase.as_deref(), &list).await?,
    };
    let eview = View {
        state: expected,
        overlay: e_over,
        entries: base_entries(cp.as_deref(), ebase.as_deref(), &list).await?,
    };

    let mut blobs = Blobs {
        cp: cp.as_deref(),
        readers: HashMap::new(),
        ids: HashMap::new(),
    };
    let mut planned = Vec::new();
    let mut conflicts = Vec::new();
    let mut blocked = Vec::new();
    let mut unlisted = false;
    for rel in cands {
        let Some((want, src)) = tview.value(&mut blobs, &rel).await? else {
            continue;
        };
        let exp = eview.value(&mut blobs, &rel).await?.map(|(v, _)| v);
        if exp.as_ref().is_some_and(|e| same(&want, e)) {
            continue;
        }
        let disk = read_disk(&root, &rel, MAX_DISK_READ).await?;
        let now = disk_val(blobs.cp, &disk).await?;
        if same(&want, &now) {
            continue;
        }
        if now == Val::Other {
            // A directory is never removed, and never written over.
            if want != Val::Absent {
                blocked.push(rel);
            }
            continue;
        }
        if exp.is_none_or(|e| !same(&e, &now)) {
            if !overwrite.contains(&rel) {
                unlisted = true;
            }
            conflicts.push(rel.clone());
        }
        planned.push(Planned { rel, src, disk });
    }
    drop(blobs);
    let text = |p: &PathBuf| p.to_string_lossy().into_owned();
    if !blocked.is_empty() {
        unlisted = true;
        let names: Vec<String> = blocked.iter().map(text).collect();
        notes.push(format!(
            "a directory is in the way of {}, which is never written over",
            names.join(", ")
        ));
    }
    let mut report = MaterializeReport {
        restored: planned
            .iter()
            .filter(|p| !matches!(p.src, Src::Absent))
            .map(|p| text(&p.rel))
            .collect(),
        deleted: planned
            .iter()
            .filter(|p| matches!(p.src, Src::Absent))
            .map(|p| text(&p.rel))
            .collect(),
        conflicts: conflicts.iter().chain(&blocked).map(text).collect(),
        dry_run,
        applied: false,
        backup_ref: None,
        note: None,
    };
    report.conflicts.sort();
    report.note = (!notes.is_empty()).then(|| notes.join("; "));
    if dry_run || unlisted {
        return Ok((report, None));
    }
    let rollback = match cp.as_deref_mut() {
        Some(c) if !planned.is_empty() => {
            let tree = c.tree_id(true, deadline).await?;
            let backup = c.commit_backup(&tree, deadline).await?;
            report.backup_ref = Some(backup.refname);
            Rollback::Git(backup.commit)
        }
        _ => Rollback::Files(
            planned
                .iter()
                .map(|p| (p.rel.clone(), p.disk.clone()))
                .collect(),
        ),
    };
    let written = write(sb, cp.as_deref(), target, tbase.as_deref(), &planned).await;
    if let Some(c) = cp.as_deref_mut() {
        c.invalidate();
    }
    if let Err(e) = written {
        let _ = roll_back(sb, cp.as_deref(), &rollback).await;
        return Err(e);
    }
    report.applied = true;
    Ok((report, Some(rollback)))
}

/// The entries of `paths` in `base`, when there is a base and a checkpointer to read it.
async fn base_entries(
    cp: Option<&Checkpointer>,
    base: Option<&str>,
    paths: &[PathBuf],
) -> Result<Option<HashMap<PathBuf, (String, String)>>, TtError> {
    Ok(match (cp, base) {
        (Some(c), Some(b)) => Some(
            c.entries(b, paths, Checkpointer::housekeeping_deadline())
                .await?,
        ),
        _ => None,
    })
}

/// Write the planned paths: deletions (links unlinked, never followed), the base's
/// contents through git, the overlay's blobs through the sandbox.
async fn write(
    sb: &dyn Sandbox,
    cp: Option<&Checkpointer>,
    target: &FileState,
    base: Option<&str>,
    planned: &[Planned],
) -> Result<(), TtError> {
    let root = sb.root();
    for p in planned.iter().filter(|p| matches!(p.src, Src::Absent)) {
        checkpoint::remove(sb, &root.join(&p.rel)).await?;
    }
    let from_base: Vec<PathBuf> = planned
        .iter()
        .filter(|p| matches!(p.src, Src::Base))
        .map(|p| p.rel.clone())
        .collect();
    if let (Some(c), Some(b), false) = (cp, base, from_base.is_empty()) {
        // A link in a path's place goes first: git would write through nothing else, but
        // the sandbox's own writes must not follow it either.
        for p in planned.iter().filter(|p| matches!(p.src, Src::Base)) {
            if matches!(p.disk, Disk::Link(_)) {
                checkpoint::remove(sb, &root.join(&p.rel)).await?;
            }
        }
        c.checkout(b, &from_base, Checkpointer::housekeeping_deadline())
            .await?;
    }
    let mut reader = None;
    for p in planned {
        let Src::Blob(b) = &p.src else {
            continue;
        };
        let full = root.join(&p.rel);
        if matches!(p.disk, Disk::Link(_)) {
            checkpoint::remove(sb, &full).await?;
        }
        if reader.is_none() {
            reader = Some(SessionReader::<SessionRecord>::open(&target.log)?);
        }
        let bytes = reader.as_ref().expect("opened").read_blob(b)?;
        sb.write_atomic(&full, &bytes, None)
            .await
            .map_err(|source| TtError::Disk {
                path: full.display().to_string(),
                source,
            })?;
    }
    Ok(())
}

/// Put back what [`materialize_inner`] changed: the backup snapshot (with git), else each
/// written file's content before (a file over the read limit or a link cannot be).
pub(crate) async fn roll_back(
    sb: &dyn Sandbox,
    cp: Option<&Checkpointer>,
    r: &Rollback,
) -> Result<(), TtError> {
    match (r, cp) {
        (Rollback::Git(commit), Some(cp)) => {
            cp.restore(commit, false).await?;
        }
        (Rollback::Git(_), None) => {}
        (Rollback::Files(files), _) => {
            for (rel, disk) in files {
                let full = sb.root().join(rel);
                match disk {
                    Disk::File { bytes, mode } => {
                        sb.write_atomic(&full, bytes, Some(*mode))
                            .await
                            .map_err(|source| TtError::Disk {
                                path: full.display().to_string(),
                                source,
                            })?;
                    }
                    Disk::Absent => checkpoint::remove(sb, &full).await?,
                    Disk::Large { .. } | Disk::Link(_) | Disk::Other => {}
                }
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------- in the engine

/// Client paths (`overwrite`) as log paths: relative to `root`, no `..`.
fn rel_paths(root: &Path, paths: &[String]) -> Result<Vec<RelPath>, TtError> {
    paths
        .iter()
        .map(|p| RelPath::from_abs(root, &root.join(p)).ok_or_else(|| TtError::BadPath(p.clone())))
        .collect()
}

/// Run `f` off the async workers, on a runtime of its own for its git steps (the log, its
/// blobs and the files are blocking I/O).
async fn blocking<T: Send + 'static>(
    f: impl FnOnce(&tokio::runtime::Runtime) -> Result<T, TtError> + Send + 'static,
) -> Result<T, TtError> {
    tokio::task::spawn_blocking(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| TtError::Other(e.to_string()))?;
        f(&rt)
    })
    .await
    .map_err(|e| TtError::Other(e.to_string()))?
}

/// The records a rewind's branch ends with, after its copy: a `restored` checkpoint of
/// exactly the materialized state (the target's base, or the expected tip's when the
/// target has none, with the overlay applied), or without any base an `Edit` per overlay
/// file asserting its content (pre- and post-image the same), so the branch's
/// [`expected_tip`] is the files it continues from.
async fn branch_tip(
    cp: Option<&Checkpointer>,
    reader: &SessionReader<SessionRecord>,
    target: &FileState,
    expected: &FileState,
    at: u32,
    cut: &Rebuilt,
) -> Result<Vec<SessionRecord>, TtError> {
    let root = reader.header().root.clone();
    let base = target.base.as_ref().or(expected.base.as_ref());
    let (Some(cp), Some(base)) = (cp, base) else {
        return Ok(target
            .overlay
            .iter()
            .map(|(path, b)| SessionRecord::Edit {
                path: path.clone(),
                pre_image: b.clone(),
                post_image_blob: b.clone(),
                step: at,
            })
            .collect());
    };
    let overlay = if target.base.is_some() {
        target.overlay.clone()
    } else {
        let mut merged: BTreeMap<RelPath, Option<BlobRef>> =
            expected.overlay.iter().cloned().collect();
        merged.extend(target.overlay.iter().cloned());
        merged.into_iter().collect()
    };
    let mut entries = Vec::with_capacity(overlay.len());
    for (path, b) in overlay {
        path.resolve(&root)?;
        let bytes = b.map(|b| reader.read_blob(&b)).transpose()?;
        entries.push((PathBuf::from(path.as_str()), bytes));
    }
    let made = cp
        .compose(base, &entries, Checkpointer::housekeeping_deadline())
        .await?;
    Ok(vec![SessionRecord::Checkpoint {
        n: cut.checkpoints.max_n + 1,
        commit: made.commit,
        tree: made.tree,
        head: made.head,
        dirty: made.dirty,
        turn: cut.turn_count,
        step: at,
        point: Some(CheckpointPoint::Restored),
    }])
}

/// What a rewind or switch made: the report, and when applied the log the session
/// continues on with its rebuilt context.
type Made = (
    MaterializeReport,
    Option<(SessionWriter<SessionRecord>, Rebuilt)>,
);

impl Session {
    /// `Command::Rewind`, as a job: the client can cancel it while it waits for the
    /// session-start checkpoint; once it starts it runs to its end. Answered with
    /// `Rewound` (and, when applied, `SessionChanged` after `on_branch` is told the
    /// branch's id and log) or an `Error`.
    pub(crate) async fn rewind_job(
        &mut self,
        at: u32,
        label: Option<String>,
        dry_run: bool,
        overwrite: Vec<String>,
        ctl: &TurnControl,
        on_branch: impl FnOnce(&str, &Path),
    ) {
        let unbound = self.events.clone();
        self.events = unbound.bound(ctl.cancel.clone());
        let done =
            if self.settle_start_checkpoint_or(&ctl.cancel).await && !ctl.cancel.is_cancelled() {
                self.rewind(at, label, dry_run, &overwrite, ctl).await
            } else {
                Err(TtError::Cancelled)
            };
        match done {
            Ok((report, moved)) => {
                let branch = moved.as_ref().map(|(id, _)| id.clone()).unwrap_or_default();
                self.emit(Event::Rewound { at, branch, report }).await;
                if let Some((id, path)) = moved {
                    on_branch(&id, &path);
                    self.emit(Event::SessionChanged { session_id: id }).await;
                }
            }
            Err(e) => self.time_travel_failed("rewind", e).await,
        }
        self.events = unbound;
    }

    /// `Command::Switch`, as a job like [`rewind_job`](Self::rewind_job). Answered with
    /// `Switched` (and `SessionChanged` when applied) or an `Error`.
    pub(crate) async fn switch_job(
        &mut self,
        session_id: String,
        dry_run: bool,
        overwrite: Vec<String>,
        ctl: &TurnControl,
        on_switch: impl FnOnce(&str, &Path),
    ) {
        let unbound = self.events.clone();
        self.events = unbound.bound(ctl.cancel.clone());
        let done =
            if self.settle_start_checkpoint_or(&ctl.cancel).await && !ctl.cancel.is_cancelled() {
                self.switch(&session_id, dry_run, &overwrite, ctl).await
            } else {
                Err(TtError::Cancelled)
            };
        match done {
            Ok((report, moved)) => {
                self.emit(Event::Switched {
                    branch: session_id,
                    report,
                })
                .await;
                if let Some((id, path)) = moved {
                    on_switch(&id, &path);
                    self.emit(Event::SessionChanged { session_id: id }).await;
                }
            }
            Err(e) => self.time_travel_failed("switch", e).await,
        }
        self.events = unbound;
    }

    async fn time_travel_failed(&self, what: &str, e: TtError) {
        self.emit(Event::Error {
            kind: ErrorKind::Tool,
            message: format!("{what}: {e}"),
            retryable: false,
            hint: None,
        })
        .await;
    }

    /// The working-tree lease, exclusively.
    fn exclusive(&self) -> Result<ExclusiveGuard, TtError> {
        match &self.lease {
            Some(lease) => Ok(lease.try_exclusive()?),
            None => Err(TtError::NoLease),
        }
    }

    /// The tip checkpoint before a restore writes (spec 7.12.2 step 1), made durable.
    async fn tip_checkpoint(&self) {
        self.checkpoint().await;
        self.git_checkpoint(Point::BeforeRestore, self.last_step(), None)
            .await;
        self.checkpoint().await;
    }

    async fn rewind(
        &mut self,
        at: u32,
        label: Option<String>,
        dry_run: bool,
        overwrite: &[String],
        ctl: &TurnControl,
    ) -> Result<(MaterializeReport, Option<(String, PathBuf)>), TtError> {
        let root = self.root().to_path_buf();
        let overwrite = rel_paths(&root, overwrite)?;
        let _lease = self.exclusive()?;
        if dry_run {
            self.checkpoint().await;
        } else {
            self.tip_checkpoint().await;
        }
        let path = self.log.path();
        let parent_id = self.session_id.clone();
        let data_home = self.data_home.clone();
        let init = HeaderInit {
            label,
            origin: Origin::Rewind,
            ..self.header_init.clone()
        };
        let calibration = self.calibration();
        let guard = match &self.checkpoints {
            Some(c) => Some(Arc::clone(c).lock_owned().await),
            None => None,
        };
        let sandbox = Arc::clone(&self.sandbox);
        let made: Made = blocking(move |rt| {
            rt.block_on(async move {
                let mut guard = guard;
                let mut cp = guard.as_deref_mut().map(|g| &mut g.cp);
                let reader = SessionReader::<SessionRecord>::open(&path)?;
                let cut = rebuild(&reader, Some(at))?;
                // The parent offset names the last record copied; checked before any write.
                let end = cut.end_offset.ok_or_else(|| {
                    TtError::Other(
                        "there is nothing to branch from yet: the session has no records \
                         before that point"
                            .into(),
                    )
                })?;
                let target = state_at(&reader, at)?;
                let expected = expected_tip(&reader)?;
                let (report, rollback) = materialize_inner(
                    sandbox.as_ref(),
                    cp.as_deref_mut(),
                    &target,
                    &expected,
                    &overwrite,
                    dry_run,
                )
                .await?;
                let Some(rollback) = rollback else {
                    return Ok((report, None));
                };
                let branched = async {
                    let tip =
                        branch_tip(cp.as_deref(), &reader, &target, &expected, at, &cut).await?;
                    let init = HeaderInit {
                        parent: Some((parent_id, end)),
                        model: reader.header().model.clone(),
                        ..init
                    };
                    let child = branch(
                        &data_home,
                        &root,
                        init,
                        &path,
                        end + 1,
                        cut.incomplete_turn,
                        tip,
                    )
                    .map_err(TtError::Other)?;
                    let rebuilt = SessionReader::<SessionRecord>::open(child.path())
                        .map_err(ResumeError::from)
                        .and_then(|r| rebuild_with(&r, None, Some(&calibration)));
                    match rebuilt {
                        Ok(rebuilt) => Ok((child, rebuilt)),
                        Err(e) => {
                            discard(child);
                            Err(TtError::from(e))
                        }
                    }
                }
                .await;
                match branched {
                    Ok(b) => Ok((report, Some(b))),
                    Err(e) => {
                        let _ = roll_back(sandbox.as_ref(), cp.as_deref(), &rollback).await;
                        Err(e)
                    }
                }
            })
        })
        .await?;
        let (report, branch) = made;
        let Some((child, rebuilt)) = branch else {
            return Ok((report, None));
        };
        let moved = self
            .continue_on(child, rebuilt, ctl.cancel.cancelled(), Continue::Branch)
            .await;
        Ok((report, Some(moved)))
    }

    async fn switch(
        &mut self,
        target_id: &str,
        dry_run: bool,
        overwrite: &[String],
        ctl: &TurnControl,
    ) -> Result<(MaterializeReport, Option<(String, PathBuf)>), TtError> {
        if target_id == self.session_id {
            return Err(TtError::SameSession(target_id.to_string()));
        }
        let root = self.root().to_path_buf();
        let overwrite = rel_paths(&root, overwrite)?;
        let _lease = self.exclusive()?;
        let found = {
            let (home, root, id) = (self.data_home.clone(), root.clone(), target_id.to_string());
            blocking(move |_| {
                tautly_session::list_here_in(&home, &root)?
                    .into_iter()
                    .find(|s| s.id == id)
                    .map(|s| s.path)
                    .ok_or(TtError::NotFound(id))
            })
            .await?
        };
        if dry_run {
            self.checkpoint().await;
        } else {
            self.tip_checkpoint().await;
        }
        let current = self.log.path();
        let calibration = self.calibration();
        let guard = match &self.checkpoints {
            Some(c) => Some(Arc::clone(c).lock_owned().await),
            None => None,
        };
        let sandbox = Arc::clone(&self.sandbox);
        let made: Made = blocking(move |rt| {
            rt.block_on(async move {
                let mut guard = guard;
                let mut cp = guard.as_deref_mut().map(|g| &mut g.cp);
                // Locked first: a session another engine has open is refused here.
                let mut writer = if dry_run {
                    None
                } else {
                    Some(SessionWriter::<SessionRecord>::reopen(&found)?)
                };
                let there = SessionReader::<SessionRecord>::open(&found)?;
                let here = SessionReader::<SessionRecord>::open(&current)?;
                let target = expected_tip(&there)?;
                let expected = expected_tip(&here)?;
                let (report, rollback) = materialize_inner(
                    sandbox.as_ref(),
                    cp.as_deref_mut(),
                    &target,
                    &expected,
                    &overwrite,
                    dry_run,
                )
                .await?;
                let (Some(mut writer), Some(rollback)) = (writer.take(), rollback) else {
                    return Ok((report, None));
                };
                let rebuilt = rebuild_with(&there, None, Some(&calibration))
                    .map_err(TtError::from)
                    .and_then(|r| {
                        // A session that stopped mid-turn gets its missing `TurnEnd`, as
                        // on resume.
                        if r.incomplete_turn {
                            writer.append(&SessionRecord::TurnEnd {
                                reason: FinishReason::Error,
                                evidence: None,
                            })?;
                            writer.checkpoint()?;
                        }
                        Ok(r)
                    });
                match rebuilt {
                    Ok(r) => Ok((report, Some((writer, r)))),
                    Err(e) => {
                        let _ = roll_back(sandbox.as_ref(), cp.as_deref(), &rollback).await;
                        Err(e)
                    }
                }
            })
        })
        .await?;
        let (report, moved) = made;
        let Some((writer, rebuilt)) = moved else {
            return Ok((report, None));
        };
        let moved = self
            .continue_on(writer, rebuilt, ctl.cancel.cancelled(), Continue::Resumed)
            .await;
        Ok((report, Some(moved)))
    }
}
