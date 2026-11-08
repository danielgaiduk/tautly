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
//! A path that would change is a conflict when its disk content is not the expected one
//! (or the expected content is not known), or when a human changed it (spec 7.12.1): in a
//! pair of consecutive checkpoints with no command between them (a turn's end and the
//! next turn's first checkpoint, a resume's start, ...), a path whose change no `Edit` or
//! `Undone` between them explains. Unless every conflict is listed in `overwrite`, nothing
//! is written. A path where a directory, a link or a file is in the way (at it or above it)
//! is *blocked*: never written, whatever `overwrite` says. Each write first checks that the
//! path resolves to itself. Before the first write the content it replaces is kept: a
//! snapshot (`undo-<n>`, with git checkpoints), else (in the engine) each file's bytes in
//! the session log's blob store, in a `RestoreBegun` record. A step that fails after files
//! were written puts them back and says so ([`TtError::Failed`]). Contents are compared as
//! git blob ids (with a checkpointer) or blake3 digests (without); disk files are read
//! without following links, large ones by a streaming hash. A tool's edit of a file no
//! checkpoint holds (an ignored one) is reverted from its pre-image too.
//!
//! # Rewind and switch
//!
//! `Command::Rewind` (spec 7.12.2): the exclusive working-tree lease, the step checked, a
//! `before_restore` checkpoint appended to the current session (so its tip stays
//! restorable), the target and expected states, the dry run's report; when applied:
//! backup, materialize, the branch log (a fork with `origin: rewind`, whose own last record
//! is a `restored` checkpoint of exactly the materialized state, or outside git an
//! asserted `Edit` per overlay file), its checkpoint refs, its rebuilt context. The current
//! log records the command, the tip checkpoint and the restore's `RestoreBegun` and
//! `RestoreEnded`. `Command::Switch` (spec 7.12.3) materializes another session's
//! [`expected_tip`] the same way, appends a `restored` checkpoint to it, and continues it.

mod materialize;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tautly_provider::ProviderEvent;
use tautly_sandbox::SandboxError;
use tautly_session::{BlobRef, ConfineError, HeaderInit, Origin, RelPath, SessionWriter};
use tautly_tools::PersistedEffect;

pub(crate) use self::materialize::confined;
pub use self::materialize::{MaterializeReport, materialize};
use self::materialize::{fail, materialize_inner};
use crate::checkpoint::{CheckpointError, Checkpointer, Point, Skip};
use crate::lease::{ExclusiveGuard, LeaseError};
use crate::protocol::{Command, ErrorKind, Event, FinishReason};
use crate::record::{CheckpointPoint, RecordLog, RestoreBy, RestoreOutcome, SessionRecord};
use crate::resume::{Continue, Rebuilt, ResumeError, branch, discard, rebuild, rebuild_with};
use crate::session::Session;
use crate::turn::{Response, TurnControl};
use crate::undo::{LiveLog, UndoError, UndoLog};

/// The note of a state with no checkpoint after its boundary.
const NOTE_NO_CHECKPOINT: &str = "only tool edits are restored; command effects are not \
     (no checkpoint follows that step: outside git, checkpoints off, or the session stopped \
     mid-turn)";

/// The note of a state across a record this version cannot read (cp5 FMT re-review N1).
const NOTE_UNREADABLE: &str = "a record written by a newer version of tautly, which this one \
     cannot read, lies between; it may have changed files";

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
    /// [`state_at`]: the offset of the step's last record (none: before every record).
    after: Option<u64>,
    /// [`state_at`]: each path a tool wrote after the chosen checkpoint, with its earliest
    /// content there: what an ignored file (no checkpoint holds it) goes back to.
    later: Vec<(RelPath, Option<BlobRef>)>,
    /// [`expected_tip`]: the last content a tool or undo left in each path, over the whole
    /// log (for a file no checkpoint holds).
    written: Vec<(RelPath, Option<BlobRef>)>,
    /// [`expected_tip`]: consecutive checkpoints with no command between, for the paths a
    /// human changed.
    pairs: Vec<Pair>,
}

/// Two consecutive checkpoints of a log with no command run between them.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Pair {
    /// The later one's offset.
    offset: u64,
    from: String,
    to: String,
    /// The later one is a `restored` checkpoint: a rewind or switch wrote every change.
    restore: bool,
    /// What the `Edit` and `Undone` records between left in each path (none: unknown).
    explained: Vec<(RelPath, Option<Option<BlobRef>>)>,
}

impl FileState {
    /// The log the state was read from.
    pub fn log(&self) -> &Path {
        &self.log
    }

    fn new(log: &Path) -> FileState {
        FileState {
            base: None,
            overlay: Vec::new(),
            partial: None,
            log: log.to_path_buf(),
            after: None,
            later: Vec::new(),
            written: Vec::new(),
            pairs: Vec::new(),
        }
    }
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
    /// A step failed after files were written: whether they were put back, and the
    /// snapshot that holds them as they were.
    #[error("{source}; {}{}", rolled(.rolled_back), backup(.backup_ref))]
    Failed {
        source: Box<TtError>,
        backup_ref: Option<String>,
        rolled_back: Result<(), String>,
    },
    #[error("{0}")]
    Other(String),
}

fn rolled(r: &Result<(), String>) -> String {
    match r {
        Ok(()) => "the files written were put back as they were".to_string(),
        Err(e) => format!("the files written could NOT all be put back ({e})"),
    }
}

fn backup(b: &Option<String>) -> String {
    match b {
        Some(b) => format!("; the files from before are in the snapshot {b}"),
        None => String::new(),
    }
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

/// Steps as the context turn counter numbers them (spec 7.12.1): a user message is the
/// step after the last one, a provider step is its `Request`'s. A step's records are its
/// `Request`, `Provider`, `ToolCall`, `ToolResult` and `Edit` records, and a user
/// message's `TurnStart`. A request uses up its number only once its response puts
/// something into the history ([`Response::answered`], the rule rebuild and the live loop
/// follow): one with no response (cancelled, failed, cut off by a crash) shares its number
/// with the step after it. The one numbering of tree, `state_at`, `step_at_offset`,
/// replay, compare, bisect, rewind and fork.
#[derive(Default)]
pub(crate) struct StepCounter {
    /// The last step that used up its number.
    done: u32,
    open: Option<(u32, u32)>,
    /// The open step's response so far.
    resp: Response,
    /// A user turn is running (between its `TurnStart` and `TurnEnd`).
    running: bool,
}

impl StepCounter {
    /// The step (and its user turn) `rec` is part of, if any.
    pub fn see(&mut self, rec: &SessionRecord) -> Option<(u32, u32)> {
        match rec {
            SessionRecord::TurnStart { turn, .. } => {
                self.close();
                self.running = true;
                self.done = self.done.saturating_add(1);
                Some((self.done, *turn))
            }
            SessionRecord::Request { step, turn, .. } => {
                self.close();
                self.open = Some((*step, *turn));
                self.open
            }
            // What rebuild ends a step at (`close_step`): the records written before the
            // next step's request. A response event after one belongs to no step.
            SessionRecord::TurnEnd { .. }
            | SessionRecord::Forked
            | SessionRecord::ModelSet { .. }
            | SessionRecord::GcApplied { .. }
            | SessionRecord::Compaction { .. }
            | SessionRecord::PinsFolded
            | SessionRecord::MemoryIndexApplied { .. }
            | SessionRecord::PrefixApplied { .. }
            | SessionRecord::StartPins { .. }
            | SessionRecord::Uncompacted { .. }
            | SessionRecord::Undone { .. }
            | SessionRecord::WritesLost { late: true, .. } => {
                self.running &= !matches!(rec, SessionRecord::TurnEnd { .. });
                self.close();
                None
            }
            // A pin sent between turns is part of what the next step is built from.
            SessionRecord::Command(Command::Pin { .. } | Command::Unpin { .. })
                if !self.running =>
            {
                self.close();
                None
            }
            SessionRecord::Provider(ev) => {
                if self.open.is_some()
                    && let Some(ev) = slim_event(ev)
                {
                    self.resp.push(ev);
                }
                self.open
            }
            // A `ToolCall` answers nothing by itself: its call is the response's
            // `Provider` event, as rebuild reads it.
            SessionRecord::ToolCall { .. }
            | SessionRecord::ToolResult { .. }
            | SessionRecord::Edit { .. } => self.open,
            _ => None,
        }
    }

    /// The last step so far: the open one once its response is answered.
    pub fn last(&self) -> u32 {
        match self.open {
            Some((step, _)) if self.resp.answered() => step,
            _ => self.done,
        }
    }

    fn close(&mut self) {
        self.done = self.last();
        self.open = None;
        self.resp = Response::default();
    }
}

/// `ev` as far as step numbering needs it: content events emptied (their kind and count
/// decide whether a response is answered), `Done` kept (a cut-off reasoning item is
/// dropped), usage and the rest left out.
fn slim_event(ev: &ProviderEvent) -> Option<ProviderEvent> {
    Some(match ev {
        ProviderEvent::TextDelta(_) => ProviderEvent::TextDelta(String::new()),
        ProviderEvent::ReasoningDelta(_) => ProviderEvent::ReasoningDelta(String::new()),
        ProviderEvent::ReasoningSignature(_) => ProviderEvent::ReasoningSignature(String::new()),
        ProviderEvent::RedactedReasoning(_) => ProviderEvent::RedactedReasoning(String::new()),
        ProviderEvent::ToolCall { id, name, .. } => ProviderEvent::ToolCall {
            id: id.clone(),
            name: name.clone(),
            input_json: String::new(),
        },
        ProviderEvent::Done { stop } => ProviderEvent::Done { stop: stop.clone() },
        _ => return None,
    })
}

/// A record as [`Boundaries`] keeps it: what `state_at` and the bisect pre-pass read, with
/// the bulk (provider text, other tools' arguments, prompts) left out, so a walk does not
/// hold the log (cp5 review C M7). None: not needed.
fn slim(rec: SessionRecord) -> Option<SessionRecord> {
    Some(match rec {
        // Every one is kept: a step's last record decides its boundary.
        SessionRecord::Provider(ev) => {
            SessionRecord::Provider(slim_event(&ev).unwrap_or(ProviderEvent::Unknown))
        }
        SessionRecord::ToolCall {
            id,
            name,
            args,
            args_blob,
        } => {
            let (args, args_blob) = match name.as_str() {
                "bash" => (args, args_blob),
                _ => (serde_json::Value::Null, None),
            };
            SessionRecord::ToolCall {
                id,
                name,
                args,
                args_blob,
            }
        }
        SessionRecord::TurnStart { turn, .. } => SessionRecord::TurnStart {
            turn,
            text: String::new(),
        },
        rec @ (SessionRecord::Request { .. }
        | SessionRecord::ToolResult { .. }
        | SessionRecord::Edit { .. }
        | SessionRecord::Undone { .. }
        | SessionRecord::WritesLost { .. }
        | SessionRecord::TurnEnd { .. }
        | SessionRecord::Checkpoint { .. }
        | SessionRecord::CheckpointSkipped { .. }
        // It may have changed files: the states across it are partial.
        | SessionRecord::Unreadable) => rec,
        _ => return None,
    })
}

/// A log's records with their offsets and the step each belongs to.
struct Walked {
    recs: Vec<(u64, SessionRecord)>,
    owner: Vec<Option<(u32, u32)>>,
    last_step: u32,
}

fn walk(reader: &RecordLog) -> Result<Walked, TtError> {
    let mut steps = StepCounter::default();
    let mut recs = Vec::new();
    let mut owner = Vec::new();
    for rec in reader.records() {
        let (at, rec) = rec?;
        let o = steps.see(&rec);
        if let Some(rec) = slim(rec) {
            owner.push(o);
            recs.push((at, rec));
        }
    }
    Ok(Walked {
        recs,
        owner,
        last_step: steps.last(),
    })
}

/// The step whose last record is at `offset` or before: for a fork's `Header.parent`
/// offset, which names the last record its log copied, the step it branched at. (Spec
/// 7.12.3 words it as "the step whose last record is at `offset − 1`", counting the
/// offset past the copy; the header stores the last record's own offset.) 0 before the
/// first step. A record that cannot be read ends the count there.
pub fn step_at_offset(reader: &RecordLog, offset: u64) -> u32 {
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
    steps.last()
}

/// Values in log order, each path's earliest kept.
fn earliest(values: Vec<(RelPath, Option<BlobRef>)>) -> Vec<(RelPath, Option<BlobRef>)> {
    let mut map = BTreeMap::new();
    for (path, value) in values.into_iter().rev() {
        map.insert(path, value);
    }
    map.into_iter().collect()
}

/// What `rec` (an `Edit` or `Undone`) wrote before: `(path, content before)` each.
pub(crate) fn befores(rec: &SessionRecord) -> Vec<(RelPath, Option<BlobRef>)> {
    match rec {
        SessionRecord::Edit {
            path, pre_image, ..
        } => vec![(path.clone(), pre_image.clone())],
        SessionRecord::Undone { files, .. } => files
            .iter()
            .map(|f| (f.path.clone(), f.from_blob.clone()))
            .collect(),
        _ => Vec::new(),
    }
}

/// The files at the end of step `at` (spec 7.12.1; `at = 0`: before the first step): the
/// first `Checkpoint` after the step's last record, with the `Edit`s and `Undone`s between
/// reverted; see the module docs for when it is partial. A step past the log's last is
/// [`TtError::NoSuchStep`]. Blocking I/O (the log).
pub fn state_at(reader: &RecordLog, at: u32) -> Result<FileState, TtError> {
    Boundaries::new(reader)?.state(at, true)
}

/// A log walked once, for the files at many of its step boundaries ([`state_at`] per
/// boundary would read the whole log each time; a bisect plans over every boundary).
pub(crate) struct Boundaries {
    w: Walked,
    log: PathBuf,
    /// Each step's last record (an index into the records).
    last_of: std::collections::HashMap<u32, usize>,
    /// Each user turn's first `turn_end` checkpoint (an index into the records).
    first_turn_end: std::collections::HashMap<u32, usize>,
}

impl Boundaries {
    /// Read the log once. Blocking I/O.
    pub(crate) fn new(reader: &RecordLog) -> Result<Boundaries, TtError> {
        let w = walk(reader)?;
        let mut last_of = std::collections::HashMap::new();
        let mut first_turn_end = std::collections::HashMap::new();
        for (i, (owner, (_, rec))) in w.owner.iter().zip(&w.recs).enumerate() {
            if let Some((s, _)) = owner {
                last_of.insert(*s, i);
            }
            if let SessionRecord::Checkpoint {
                point: Some(CheckpointPoint::TurnEnd),
                turn,
                ..
            } = rec
            {
                first_turn_end.entry(*turn).or_insert(i);
            }
        }
        Ok(Boundaries {
            w,
            log: reader.path().to_path_buf(),
            last_of,
            first_turn_end,
        })
    }

    /// The log's last step.
    pub(crate) fn last_step(&self) -> u32 {
        self.w.last_step
    }

    /// The log's records, with their offsets.
    pub(crate) fn records(&self) -> &[(u64, SessionRecord)] {
        &self.w.recs
    }

    /// [`state_at`] of step `at`. Without `later`, the state leaves out what tools wrote
    /// after its checkpoint, which only an in-place materialize needs (for ignored files)
    /// and which reads the rest of the log.
    pub(crate) fn state(&self, at: u32, later: bool) -> Result<FileState, TtError> {
        state_in(self, at, later)
    }
}

fn state_in(b: &Boundaries, at: u32, later: bool) -> Result<FileState, TtError> {
    let w = &b.w;
    let no_step = || TtError::NoSuchStep {
        at,
        last: w.last_step,
    };
    if at > w.last_step {
        return Err(no_step());
    }
    let mut state = FileState::new(&b.log);
    let (start, turn) = if at == 0 {
        (0, None)
    } else {
        let last = *b.last_of.get(&at).ok_or_else(no_step)?;
        state.after = Some(w.recs[last].0);
        (last + 1, w.owner[last].map(|(_, t)| t))
    };
    let turn_end_seen = turn.is_some_and(|t| b.first_turn_end.get(&t).is_some_and(|&i| i < start));
    let mut reverts = Vec::new();
    let (mut skipped, mut lost, mut unreadable) = (false, false, false);
    let mut chosen = None;
    for (i, (_, rec)) in w.recs.iter().enumerate().skip(start) {
        match rec {
            SessionRecord::CheckpointSkipped { .. } => skipped = true,
            SessionRecord::WritesLost { .. } => lost = true,
            SessionRecord::Unreadable => unreadable = true,
            SessionRecord::Checkpoint { .. } => {
                chosen = Some(i);
                break;
            }
            rec => reverts.extend(befores(rec)),
        }
    }
    state.overlay = earliest(reverts);
    let Some(i) = chosen else {
        let mut note = NOTE_NO_CHECKPOINT.to_string();
        if unreadable {
            note.push_str("; ");
            note.push_str(NOTE_UNREADABLE);
        }
        state.partial = Some(note);
        return Ok(state);
    };
    let SessionRecord::Checkpoint { commit, point, .. } = &w.recs[i].1 else {
        unreachable!("chosen as a checkpoint");
    };
    let mut notes = Vec::new();
    if skipped {
        notes.push(format!(
            "a checkpoint after step {at} was skipped, so command effects may not be \
             restored exactly"
        ));
    }
    if lost {
        notes.push(format!(
            "writes of unknown outcome happened after step {at}"
        ));
    }
    if unreadable {
        notes.push(NOTE_UNREADABLE.to_string());
    }
    match point {
        Some(CheckpointPoint::ResumeStart | CheckpointPoint::BeforeRestore) => {
            notes.push(format!(
                "the checkpoint after step {at} was taken when the session resumed or before \
                 a restore, and may hold changes made outside the agent"
            ));
        }
        None | Some(CheckpointPoint::Unknown) if !turn_end_seen => {
            notes.push(format!(
                "the checkpoint after step {at} was written by another version of tautly and \
                 is read as partial"
            ));
        }
        _ => {}
    }
    state.base = Some(commit.clone());
    state.partial = (!notes.is_empty()).then(|| notes.join("; "));
    if later {
        state.later = earliest(
            w.recs[i + 1..]
                .iter()
                .flat_map(|(_, r)| befores(r))
                .collect(),
        );
    }
    Ok(state)
}

/// What the `Edit`s after a `ToolResult` say of each path's new content.
#[derive(Default)]
pub(crate) struct Writes {
    /// What the result the edits follow did to each path (deleted or edited).
    kinds: std::collections::HashMap<RelPath, bool>,
}

impl Writes {
    /// What `rec` left in its paths (`None`: not known), or nothing when it writes none.
    pub(crate) fn see(&mut self, rec: &SessionRecord) -> Vec<(RelPath, Option<Option<BlobRef>>)> {
        match rec {
            SessionRecord::ToolResult { effects, .. } => {
                self.kinds.clear();
                for e in effects {
                    match e {
                        PersistedEffect::Edited { path, .. } => {
                            self.kinds.insert(path.clone(), false);
                        }
                        PersistedEffect::Deleted { path, .. } => {
                            self.kinds.insert(path.clone(), true);
                        }
                        _ => {}
                    }
                }
                Vec::new()
            }
            SessionRecord::Edit {
                path,
                pre_image,
                post_image_blob,
                asserted,
                ..
            } => {
                let value = match (post_image_blob, self.kinds.get(path)) {
                    (Some(post), _) => Some(Some(post.clone())),
                    (None, Some(true)) => Some(None),
                    (None, Some(false)) => None,
                    // An edit no result announced: a rewind's assertion.
                    (None, None) => (*asserted || pre_image.is_none()).then_some(None),
                };
                vec![(path.clone(), value)]
            }
            SessionRecord::Undone { files, .. } => {
                self.kinds.clear();
                files
                    .iter()
                    .map(|f| (f.path.clone(), Some(f.to.clone())))
                    .collect()
            }
            _ => {
                self.kinds.clear();
                Vec::new()
            }
        }
    }
}

/// A record that may have changed files no record names: a command ran.
pub(crate) fn ran_command(rec: &SessionRecord) -> bool {
    match rec {
        SessionRecord::ToolResult { effects, .. } => effects.iter().any(|e| {
            matches!(
                e,
                PersistedEffect::Exec { may_write: true } | PersistedEffect::Unknown
            )
        }),
        SessionRecord::WritesLost { .. } => true,
        _ => false,
    }
}

/// The files as the agent left them (spec 7.12.1): the latest checkpoint (other than a
/// `before_restore` one) with the post-images of later tool edits and the results of later
/// undos applied. A tool edit whose result was not captured leaves its path out (it is then
/// compared with the checkpoint's) and makes the state partial. Also gathers what
/// [`materialize`] needs to tell human changes apart. Blocking I/O (the log).
pub fn expected_tip(reader: &RecordLog) -> Result<FileState, TtError> {
    let mut state = FileState::new(reader.path());
    // Two passes over the log rather than holding it (cp5 review C M7): the base first.
    let mut base: Option<(u64, String)> = None;
    for rec in reader.records() {
        if let (offset, SessionRecord::Checkpoint { commit, point, .. }) = rec?
            && point != Some(CheckpointPoint::BeforeRestore)
        {
            base = Some((offset, commit));
        }
    }
    let base_at = base.as_ref().map(|(offset, _)| *offset);
    let mut overlay: BTreeMap<RelPath, Option<BlobRef>> = BTreeMap::new();
    let mut unknown = std::collections::BTreeSet::new();
    // A record this version cannot read follows the base (N1).
    let mut unreadable = false;
    let mut written: BTreeMap<RelPath, Option<BlobRef>> = BTreeMap::new();
    let mut writes = Writes::default();
    // The pairs: the last checkpoint, what was written since, whether a command ran.
    let mut last: Option<String> = None;
    let mut between: BTreeMap<RelPath, Option<Option<BlobRef>>> = BTreeMap::new();
    let mut command = false;
    // A turn ended (cancelled, or closed after a crash) with no checkpoint of its end
    // since the pair began: nothing then marks when the human's changes began, so the
    // pair counts whatever commands ran in it (Task 49 N1).
    let mut cut = false;
    let mut end_checkpointed = false;
    for rec in reader.records() {
        let (offset, rec) = rec?;
        let (offset, rec) = (&offset, &rec);
        match rec {
            SessionRecord::Checkpoint {
                point: Some(CheckpointPoint::TurnEnd),
                ..
            } => end_checkpointed = true,
            SessionRecord::TurnEnd { .. } => {
                cut |= !end_checkpointed;
                end_checkpointed = false;
            }
            SessionRecord::Request { .. }
            | SessionRecord::Provider(_)
            | SessionRecord::ToolCall { .. }
            | SessionRecord::ToolResult { .. }
            | SessionRecord::CheckpointSkipped { .. } => end_checkpointed = false,
            _ => {}
        }
        if let SessionRecord::Checkpoint { commit, point, .. } = rec {
            if let Some(from) = &last
                && (!command || cut)
                && from != commit
            {
                state.pairs.push(Pair {
                    offset: *offset,
                    from: from.clone(),
                    to: commit.clone(),
                    restore: *point == Some(CheckpointPoint::Restored),
                    explained: std::mem::take(&mut between).into_iter().collect(),
                });
            }
            last = Some(commit.clone());
            between.clear();
            command = false;
            cut = false;
            continue;
        }
        unreadable |=
            matches!(rec, SessionRecord::Unreadable) && base_at.is_none_or(|b| *offset > b);
        command |= ran_command(rec);
        for (path, value) in writes.see(rec) {
            between.insert(path.clone(), value.clone());
            match &value {
                Some(v) => {
                    written.insert(path.clone(), v.clone());
                }
                None => {
                    written.remove(&path);
                }
            }
            if base_at.is_none_or(|b| *offset > b) {
                match value {
                    Some(v) => {
                        overlay.insert(path.clone(), v);
                        unknown.remove(&path);
                    }
                    None => {
                        overlay.remove(&path);
                        unknown.insert(path);
                    }
                }
            }
        }
    }
    state.base = base.map(|(_, commit)| commit);
    state.overlay = overlay.into_iter().collect();
    state.written = written.into_iter().collect();
    let mut notes = Vec::new();
    if !unknown.is_empty() {
        let names: Vec<&str> = unknown.iter().map(RelPath::as_str).collect();
        notes.push(format!(
            "the content a tool left in {} was not captured",
            names.join(", ")
        ));
    }
    if unreadable {
        notes.push(NOTE_UNREADABLE.to_string());
    }
    state.partial = (!notes.is_empty()).then(|| notes.join("; "));
    Ok(state)
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

/// How many records a rewind to step `at` copies: through the cut's last record, or for
/// `at = 0` with nothing kept before the first step, the records before that step's first
/// one (commands and set-up, which change nothing). None: nothing to copy.
pub(crate) fn copy_count(reader: &RecordLog, at: u32, cut: &Rebuilt) -> Option<u64> {
    if let Some(end) = cut.end_offset {
        return Some(end + 1);
    }
    if at != 0 {
        return None;
    }
    let mut steps = StepCounter::default();
    let mut count = 0;
    for rec in reader.records() {
        let Ok((offset, rec)) = rec else {
            break;
        };
        if steps.see(&rec).is_some() {
            return (offset > 0).then_some(offset);
        }
        count = offset + 1;
    }
    (count > 0).then_some(count)
}

/// A `restored` checkpoint of exactly `state` (its base with its overlay applied), with
/// number `n`, of user turn `turn` and step `step`. None without a base.
async fn restored_checkpoint(
    cp: &Checkpointer,
    reader: &RecordLog,
    base: Option<&str>,
    overlay: &[(RelPath, Option<BlobRef>)],
    (n, turn, step): (u32, u32, u32),
) -> Result<Option<SessionRecord>, TtError> {
    let Some(base) = base else {
        return Ok(None);
    };
    let root = reader.header().root.clone();
    let mut entries = Vec::with_capacity(overlay.len());
    for (path, b) in overlay {
        path.resolve(&root)?;
        let bytes = b.as_ref().map(|b| reader.read_blob(b)).transpose()?;
        entries.push((PathBuf::from(path.as_str()), bytes));
    }
    let made = cp
        .compose(base, &entries, Checkpointer::housekeeping_deadline())
        .await?;
    Ok(Some(SessionRecord::Checkpoint {
        n,
        commit: made.commit,
        tree: made.tree,
        head: made.head,
        dirty: made.dirty,
        turn,
        step,
        point: Some(CheckpointPoint::Restored),
    }))
}

/// The records a rewind's branch ends with, after its copy, so its [`expected_tip`] is
/// the files it continues from: a `restored` checkpoint (the target's base, or the
/// expected tip's when the target has none, with the overlay applied), or without any base
/// an asserted `Edit` per overlay file (pre- and post-image the same).
async fn branch_tip(
    cp: Option<&Checkpointer>,
    reader: &RecordLog,
    target: &FileState,
    expected: &FileState,
    at: u32,
    cut: &Rebuilt,
) -> Result<Vec<SessionRecord>, TtError> {
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
                asserted: true,
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
    let numbers = (cut.checkpoints.max_n + 1, cut.turn_count, at);
    let made = restored_checkpoint(cp, reader, Some(base), &overlay, numbers).await?;
    Ok(made.into_iter().collect())
}

/// The log ends after a `TurnStart` with no `TurnEnd` (a crash mid-turn).
fn ends_inside_a_turn(reader: &RecordLog) -> Result<bool, TtError> {
    let mut open = false;
    for rec in reader.records() {
        match rec?.1 {
            SessionRecord::TurnStart { .. } => open = true,
            SessionRecord::TurnEnd { .. } => open = false,
            _ => {}
        }
    }
    Ok(open)
}

/// The highest checkpoint number, the user turn and the step a log ends on.
fn log_tip(reader: &RecordLog) -> Result<(u32, u32, u32), TtError> {
    let (mut n, mut turn) = (0, 0);
    let mut steps = StepCounter::default();
    for rec in reader.records() {
        let (_, rec) = rec?;
        match &rec {
            SessionRecord::Checkpoint { n: m, .. } => n = n.max(*m),
            SessionRecord::TurnStart { turn: t, .. } => turn = turn.max(*t),
            _ => {}
        }
        steps.see(&rec);
    }
    Ok((n, turn, steps.last()))
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
                self.emit(Event::Rewound {
                    id: None,
                    at,
                    branch,
                    report,
                })
                .await;
                if let Some((id, path)) = moved {
                    on_branch(&id, &path);
                    self.emit(Event::SessionChanged {
                        id: None,
                        session_id: id,
                    })
                    .await;
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
                    id: None,
                    branch: session_id,
                    report,
                })
                .await;
                if let Some((id, path)) = moved {
                    on_switch(&id, &path);
                    self.emit(Event::SessionChanged {
                        id: None,
                        session_id: id,
                    })
                    .await;
                }
            }
            Err(e) => self.time_travel_failed("switch", e).await,
        }
        self.events = unbound;
    }

    pub(crate) async fn time_travel_failed(&self, what: &str, e: TtError) {
        let hint = matches!(e, TtError::Failed { .. }).then(|| {
            "check the files named; `git status` and the snapshot ref show what changed".into()
        });
        self.emit(Event::Error {
            id: None,
            kind: ErrorKind::Tool,
            message: crate::printable(&format!("{what}: {e}")),
            retryable: false,
            hint,
        })
        .await;
    }

    /// The working-tree lease, exclusively.
    pub(crate) fn exclusive(&self) -> Result<ExclusiveGuard, TtError> {
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
        let path = self.log.path();
        // The step is checked before anything is logged.
        self.checkpoint().await;
        let checked = path.clone();
        blocking(move |_| {
            let reader = RecordLog::open(&checked)?;
            let last = step_at_offset(&reader, u64::MAX);
            if at > last {
                return Err(TtError::NoSuchStep { at, last });
            }
            Ok(())
        })
        .await?;
        if !dry_run {
            self.tip_checkpoint().await;
        }
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
        let mut live = LiveLog(self.log.clone());
        let made: Made = blocking(move |rt| {
            rt.block_on(async move {
                let mut guard = guard;
                let mut cp = guard.as_deref_mut().map(|g| &mut g.cp);
                let reader = RecordLog::open(&path)?;
                let cut = rebuild(&reader, Some(at))?;
                // Checked before any write.
                let count = copy_count(&reader, at, &cut).ok_or_else(|| {
                    TtError::Other(
                        "there is nothing to branch from yet: the session has no records \
                         before that point"
                            .into(),
                    )
                })?;
                let target = state_at(&reader, at)?;
                let expected = expected_tip(&reader)?;
                let applied = materialize_inner(
                    sandbox.as_ref(),
                    cp.as_deref_mut(),
                    &target,
                    &expected,
                    &overwrite,
                    dry_run,
                    Some((&mut live as &mut dyn UndoLog, RestoreBy::Rewind)),
                )
                .await?;
                let materialize::Applied {
                    mut report,
                    rollback,
                } = applied;
                let Some(rollback) = rollback else {
                    return Ok((report, None));
                };
                let branched = async {
                    let tip =
                        branch_tip(cp.as_deref(), &reader, &target, &expected, at, &cut).await?;
                    let init = HeaderInit {
                        parent: Some((parent_id, count - 1)),
                        parent_step: Some(at),
                        model: reader.header().model.clone(),
                        ..init
                    };
                    let child = branch(
                        &data_home,
                        &root,
                        init,
                        &path,
                        count,
                        cut.incomplete_turn,
                        tip,
                    )
                    .map_err(TtError::Other)?;
                    let rebuilt = RecordLog::open(child.path())
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
                    Ok(b) => {
                        ended(&mut live, &mut report);
                        Ok((report, Some(b)))
                    }
                    Err(e) => {
                        let backup = report.backup_ref.clone();
                        Err(fail(
                            sandbox.as_ref(),
                            cp.as_deref(),
                            Some(&mut live as &mut dyn UndoLog),
                            &rollback,
                            backup,
                            e,
                        )
                        .await)
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
            .continue_on(
                child,
                rebuilt,
                ctl.cancel.cancelled(),
                Continue::Branch,
                "rewind",
            )
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
        // The target is found and (to apply) locked before anything is logged: a session
        // another engine has open is refused here.
        let (found, opened) = {
            let (home, root, id) = (self.data_home.clone(), root.clone(), target_id.to_string());
            blocking(move |_| {
                let found = tautly_session::list_here_in(&home, &root)?
                    .into_iter()
                    .find(|s| s.id == id)
                    .map(|s| s.path)
                    .ok_or(TtError::NotFound(id))?;
                let writer = if dry_run {
                    None
                } else {
                    Some(SessionWriter::<SessionRecord>::reopen(&found)?)
                };
                Ok((found, writer))
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
        let mut live = LiveLog(self.log.clone());
        let made: Made = blocking(move |rt| {
            rt.block_on(async move {
                let mut guard = guard;
                let mut cp = guard.as_deref_mut().map(|g| &mut g.cp);
                let mut writer = opened;
                let there = RecordLog::open(&found)?;
                // A session that stopped mid-turn gets its missing `TurnEnd`, as on resume,
                // before the switch writes anything there: stamped at its last record, so
                // the time no session ran is not the turn's (cp5 FMT re-review N3).
                if let Some(w) = writer.as_mut()
                    && ends_inside_a_turn(&there)?
                {
                    w.append_at_last(&SessionRecord::TurnEnd {
                        reason: FinishReason::Error,
                        evidence: None,
                        synthetic: true,
                    })?;
                    w.checkpoint()?;
                }
                let here = RecordLog::open(&current)?;
                let target = expected_tip(&there)?;
                let expected = expected_tip(&here)?;
                // Both sessions record the restore, so a cut-off one is warned about
                // whichever is resumed.
                let mut both = Both {
                    here: &mut live,
                    there: writer.as_mut(),
                };
                let applied = materialize_inner(
                    sandbox.as_ref(),
                    cp.as_deref_mut(),
                    &target,
                    &expected,
                    &overwrite,
                    dry_run,
                    Some((&mut both as &mut dyn UndoLog, RestoreBy::Switch)),
                )
                .await?;
                let materialize::Applied {
                    mut report,
                    rollback,
                } = applied;
                let (Some(mut writer), Some(rollback)) = (writer.take(), rollback) else {
                    return Ok((report, None));
                };
                let continued = async {
                    // The tip it continues from, as a checkpoint of its own: what was
                    // written here is not a human's change.
                    if let Some(cp) = cp.as_deref() {
                        let (n, turn, step) = log_tip(&there)?;
                        let made = restored_checkpoint(
                            cp,
                            &there,
                            target.base.as_deref(),
                            &target.overlay,
                            (n + 1, turn, step),
                        )
                        .await?;
                        if let Some(rec) = made {
                            writer.append(&rec)?;
                            writer.checkpoint()?;
                        }
                    }
                    let there = RecordLog::open(&found)?;
                    let r = rebuild_with(&there, None, Some(&calibration))?;
                    Ok::<_, TtError>(r)
                }
                .await;
                let mut both = Both {
                    here: &mut live,
                    there: Some(&mut writer),
                };
                match continued {
                    Ok(r) => {
                        ended(&mut both, &mut report);
                        Ok((report, Some((writer, r))))
                    }
                    Err(e) => {
                        let backup = report.backup_ref.clone();
                        Err(fail(
                            sandbox.as_ref(),
                            cp.as_deref(),
                            Some(&mut both as &mut dyn UndoLog),
                            &rollback,
                            backup,
                            e,
                        )
                        .await)
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
            .continue_on(
                writer,
                rebuilt,
                ctl.cancel.cancelled(),
                Continue::Resumed,
                "switch",
            )
            .await;
        Ok((report, Some(moved)))
    }
}

/// The session a switch leaves and the one it switches to, logged to together (blobs go
/// to the first: both logs share the project's blob store).
struct Both<'a> {
    here: &'a mut LiveLog,
    there: Option<&'a mut SessionWriter<SessionRecord>>,
}

impl UndoLog for Both<'_> {
    fn blob(&mut self, bytes: &[u8]) -> Result<BlobRef, UndoError> {
        self.here.blob(bytes)
    }

    fn record(&mut self, rec: &SessionRecord) -> Result<(), UndoError> {
        self.here.record(rec)?;
        match self.there.as_deref_mut() {
            Some(w) => UndoLog::record(w, rec),
            None => Ok(()),
        }
    }
}

/// Log that a restore that wrote files ended well; a log that cannot say so is noted.
fn ended(live: &mut dyn UndoLog, report: &mut MaterializeReport) {
    if report.restored.is_empty() && report.deleted.is_empty() {
        return;
    }
    let rec = SessionRecord::RestoreEnded {
        outcome: RestoreOutcome::Applied,
    };
    if live.record(&rec).is_err() {
        let note = "the session log could not record the restore's end";
        report.note = Some(match report.note.take() {
            Some(n) => format!("{n}; {note}"),
            None => note.to_string(),
        });
    }
}
