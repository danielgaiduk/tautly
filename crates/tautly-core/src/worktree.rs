//! Fork at a step into an isolated git worktree (spec 7.12.4): `tautly fork <session> --at
//! N` and `session/new {fork: {…, worktree: true}}`.
//!
//! - **The worktree** ([`create`]) is `git worktree add --detach <dir> <commit>`, where
//!   `<commit>` is `state_at(N)`'s checkpoint with its overlay (the tool-edit reverts)
//!   applied, composed as a commit of its own: the new worktree is checked out clean at
//!   exactly the files of step N, and `git status` there shows only what happens after.
//!   Git runs through the checkpoints' hardened path (`checkpoint::worktree_add`): no hook
//!   (`post-checkout` included, whatever `core.hooksPath` says), filter, fsmonitor or
//!   transport runs, and no provider key is in its environment. The original work tree,
//!   its index and HEAD are never touched; only objects, `refs/tautly/` and the new
//!   worktree's administrative files are written. Outside git it is refused
//!   ([`WtError::NotGit`]); so is a state with no checkpoint to start from
//!   ([`WtError::Partial`], with the state's note). A partial state that has one (a
//!   skipped checkpoint between, say) is made, and its note returned.
//! - **The session** ([`fork_into`]) has `origin: what_if`, `parent: (session, offset of
//!   the last record copied)`, `root` = the worktree and the parent's project, so its log
//!   sits next to the parent's and shares its blobs. It holds the records up to step N,
//!   copied verbatim, then `Forked`, a `TurnEnd{cancelled}` when the copy ends inside a
//!   turn, a `restored` checkpoint of the worktree's commit, and a `ModelSet` when a model
//!   is asked for (applied before the first message: a resume replays it). Every path the
//!   copied records name must resolve under the new root (spec Review Focus 14): one with
//!   `..`, an absolute one or one with a NUL refuses the fork before anything is made.
//!   Root-relative paths make the records valid in the worktree; the engine that resumes
//!   the fork there builds that root's own code index. Logged approvals are never consent:
//!   no session rule is in a log, and the fork's engine decides every call by its own
//!   policy.
//! - **Cleanup** ([`remove`]): `git worktree remove`, which refuses a worktree with
//!   modified or untracked files unless forced ([`WtError::Dirty`]).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tautly_provider::ModelId;
use tautly_sandbox::Sandbox;
use tautly_session::{
    ConfineError, HeaderInit, Origin, RelPath, SessionReader, SessionWriter, new_session_id,
};
use tautly_tools::PersistedEffect;

use crate::checkpoint::{self, CheckpointError, CheckpointRef, Checkpointer};
use crate::config::CheckpointConfig;
use crate::record::{CheckpointPoint, SessionRecord};
use crate::resume::{Rebuilt, discard, rebuild};
use crate::timetravel::{FileState, TtError, copy_count, state_at, step_at_offset};

/// What a state without a checkpoint is refused with, when it has no note of its own.
const NOTE_NO_BASE: &str = "no checkpoint follows that step";

#[derive(Debug, thiserror::Error)]
pub enum WtError {
    #[error("a worktree fork needs a git work tree: {0}")]
    NotGit(String),
    #[error("the files at that step are not known exactly, so no worktree is made: {0}")]
    Partial(String),
    #[error("{} exists and is not empty", .0.display())]
    Exists(PathBuf),
    #[error(
        "the worktree {} has changes (modified or untracked files); --force removes it \
         all the same",
        .0.display()
    )]
    Dirty(PathBuf),
    #[error("git: {0}")]
    Git(#[from] CheckpointError),
    #[error("{0}")]
    Tt(#[from] TtError),
    #[error("cannot read or write the session log: {0}")]
    Session(#[from] tautly_session::Error),
    #[error("a path in the session log cannot be used in the worktree: {0}")]
    Path(#[from] ConfineError),
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{0}")]
    Other(String),
}

/// `git -c core.hooksPath=/dev/null worktree add --detach <dir> <base>` (with the rest of
/// the checkpoints' hardening) on the repository at `repo_root`, at `state`: its base
/// checkpoint with its overlay applied, as a commit of its own, so the worktree is clean
/// at exactly that state (see the module docs). `dir` must not exist or be empty. Returns
/// the worktree's canonical path.
pub async fn create(
    sb: Arc<dyn Sandbox>,
    repo_root: &Path,
    state: &FileState,
    dir: &Path,
) -> Result<PathBuf, WtError> {
    Ok(make(sb, repo_root, state, dir, "worktree").await?.0)
}

/// A checkpointer on `root` for the fork `session`, or why there can be none.
fn checkpointer(
    sb: &Arc<dyn Sandbox>,
    root: &Path,
    session: &str,
) -> Result<Checkpointer, WtError> {
    if checkpoint::dot_git_is_link(root) {
        return Err(WtError::NotGit(format!(
            "the .git of {} is a symbolic link",
            root.display()
        )));
    }
    if checkpoint::dot_git_unverified(root) {
        return Err(WtError::NotGit(format!(
            "the .git of {} names a repository that does not name it back",
            root.display()
        )));
    }
    Checkpointer::new(
        Arc::clone(sb),
        root.to_path_buf(),
        session.to_string(),
        CheckpointConfig::default(),
    )
    .ok_or_else(|| {
        WtError::NotGit(format!(
            "{} is not a git work tree, or git is not available",
            root.display()
        ))
    })
}

/// [`create`], also returning the commit the worktree was checked out at. `session` names
/// the composed commit.
async fn make(
    sb: Arc<dyn Sandbox>,
    repo_root: &Path,
    state: &FileState,
    dir: &Path,
    session: &str,
) -> Result<(PathBuf, CheckpointRef), WtError> {
    let cp = checkpointer(&sb, repo_root, session)?;
    let Some(base) = state.base.clone() else {
        let note = state.partial.as_deref().unwrap_or(NOTE_NO_BASE);
        return Err(WtError::Partial(note.to_string()));
    };
    // The overlay's paths, each resolved under the new root (and never anywhere else).
    let (log, overlay, dir_owned) = (
        state.log().to_path_buf(),
        state.overlay.clone(),
        dir.to_path_buf(),
    );
    let entries = blocking(move || {
        let reader = SessionReader::<SessionRecord>::open(&log)?;
        let mut entries = Vec::with_capacity(overlay.len());
        for (path, blob) in &overlay {
            path.resolve(&dir_owned)?;
            let bytes = blob.as_ref().map(|b| reader.read_blob(b)).transpose()?;
            entries.push((PathBuf::from(path.as_str()), bytes));
        }
        Ok(entries)
    })
    .await?;
    let made = cp
        .compose(&base, &entries, Checkpointer::housekeeping_deadline())
        .await?;
    make_dir(dir)?;
    checkpoint::worktree_add(sb.as_ref(), repo_root, dir, &made.commit).await?;
    let canonical = dir.canonicalize().map_err(|source| WtError::Io {
        path: dir.to_path_buf(),
        source,
    })?;
    Ok((canonical, made))
}

/// The worktree's parent directories (private, as the data home's are); `dir` itself
/// must not exist, or be empty.
fn make_dir(dir: &Path) -> Result<(), WtError> {
    use std::os::unix::fs::DirBuilderExt;
    let io = |source| WtError::Io {
        path: dir.to_path_buf(),
        source,
    };
    match std::fs::read_dir(dir) {
        Ok(mut entries) => {
            return match entries.next() {
                Some(_) => Err(WtError::Exists(dir.to_path_buf())),
                None => Ok(()),
            };
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(io(e)),
    }
    if let Some(parent) = dir.parent() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
            .map_err(io)?;
    }
    Ok(())
}

/// Run `f` on the blocking pool (the log and its blobs).
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, WtError> + Send + 'static,
) -> Result<T, WtError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| WtError::Other(e.to_string()))?
}

/// A new session at `new_root` (a worktree of the parent's project) holding the records
/// of `reader`'s log up to step `at`, copied verbatim, with `label`, `origin` and
/// `model` (applied before the first message); returns its id. See the module docs.
/// Blocking I/O.
pub fn fork_into(
    reader: &SessionReader<SessionRecord>,
    at: u32,
    new_root: &Path,
    label: Option<String>,
    origin: Origin,
    model: Option<ModelId>,
) -> Result<String, WtError> {
    let plan = plan(reader, at)?;
    let fork = Fork {
        label,
        origin,
        model,
        id: None,
        tail: Vec::new(),
    };
    let child = fork_log(reader, &plan, new_root, fork)?;
    Ok(child.session_id().to_string())
}

/// What a fork at a step copies: how many records, and the rebuilt state of the cut.
struct Plan {
    at: u32,
    count: u64,
    cut: Rebuilt,
}

/// The fork's header and what follows its copy.
struct Fork {
    label: Option<String>,
    origin: Origin,
    model: Option<ModelId>,
    /// The session id to use (none: a new one).
    id: Option<String>,
    /// Records after `Forked` (and the cut turn's end).
    tail: Vec<SessionRecord>,
}

/// Check that step `at` exists and can be branched from, and that every path the records
/// to copy name resolves under a root (Review Focus 14).
fn plan(reader: &SessionReader<SessionRecord>, at: u32) -> Result<Plan, WtError> {
    let cut = rebuild(reader, Some(at)).map_err(TtError::from)?;
    let count = copy_count(reader, at, &cut).ok_or_else(|| {
        WtError::Other(
            "there is nothing to branch from yet: the session has no records before that \
             point"
                .into(),
        )
    })?;
    // Lexical: no root is needed to refuse `..`, absolute paths and NULs.
    let probe = Path::new("/");
    for rec in reader
        .records()
        .take(usize::try_from(count).unwrap_or(usize::MAX))
    {
        for path in record_paths(&rec?.1) {
            path.resolve(probe)?;
        }
    }
    Ok(Plan { at, count, cut })
}

/// The root-relative paths a record names.
fn record_paths(rec: &SessionRecord) -> Vec<RelPath> {
    match rec {
        SessionRecord::Edit { path, .. } => vec![path.clone()],
        SessionRecord::WritesLost { paths, .. } => paths.clone(),
        SessionRecord::Undone { files, .. } => files.iter().map(|f| f.path.clone()).collect(),
        SessionRecord::GcApplied {
            forgotten_paths, ..
        } => forgotten_paths.clone(),
        SessionRecord::RestoreBegun { files, paths, .. } => files
            .iter()
            .map(|f| f.path.clone())
            .chain(paths.iter().cloned())
            .collect(),
        SessionRecord::ToolResult { effects, .. } => effects
            .iter()
            .filter_map(|e| match e {
                PersistedEffect::Read { path, .. }
                | PersistedEffect::Edited { path, .. }
                | PersistedEffect::Deleted { path, .. } => Some(path.clone()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// Write the fork's log next to the parent's (see the module docs). On a failure the
/// half-written log is removed.
fn fork_log(
    reader: &SessionReader<SessionRecord>,
    plan: &Plan,
    new_root: &Path,
    fork: Fork,
) -> Result<SessionWriter<SessionRecord>, WtError> {
    let header = reader.header();
    let init = HeaderInit {
        parent: Some((header.session_id.clone(), plan.count - 1)),
        label: fork.label,
        origin: fork.origin,
        // The copied records start from the parent's first model.
        model: header.model.clone(),
        ..HeaderInit::default()
    };
    let mut child = SessionWriter::<SessionRecord>::create_beside(
        reader.path(),
        new_root,
        fork.id.as_deref(),
        init,
    )?;
    // Not when the copy already ends on it.
    let model = fork.model.filter(|m| plan.cut.model.as_ref() != Some(m));
    let written = (|| {
        child.copy_from(reader.path(), plan.count)?;
        child.append(&SessionRecord::Forked)?;
        if plan.cut.incomplete_turn {
            child.append(&SessionRecord::TurnEnd {
                reason: crate::FinishReason::Cancelled,
                evidence: None,
            })?;
        }
        for rec in &fork.tail {
            child.append(rec)?;
        }
        if let Some(model) = model {
            child.append(&SessionRecord::ModelSet { model })?;
        }
        child.checkpoint()
    })();
    match written {
        Ok(()) => Ok(child),
        Err(e) => {
            discard(child);
            Err(e.into())
        }
    }
}

/// What [`fork_to_worktree`] is asked for.
#[derive(Debug, Clone, Default)]
pub struct ForkRequest {
    /// The step (none: the session's last one).
    pub at: Option<u32>,
    /// Where the worktree goes (none: [`default_dir`]).
    pub dir: Option<PathBuf>,
    pub label: Option<String>,
    /// The model the fork continues on (none: the parent's at that step).
    pub model: Option<ModelId>,
}

/// What [`fork_to_worktree`] made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeFork {
    pub session_id: String,
    /// The worktree (canonical).
    pub worktree: PathBuf,
    /// The fork's session log.
    pub log: PathBuf,
    /// The step it was forked at.
    pub at: u32,
    /// Why its files may not be exactly the step's, or what else could not be done.
    pub note: Option<String>,
}

/// `<data home>/worktrees/<project-id>/<session-id>`: where a fork's worktree goes by
/// default.
pub fn default_dir(data_home: &Path, project: &str, session_id: &str) -> PathBuf {
    data_home.join("worktrees").join(project).join(session_id)
}

/// Fork the session whose log is `parent_log` at step `req.at` into a new worktree of the
/// repository the sandbox root is in ([`create`]), with a session of its own
/// ([`fork_into`]) whose checkpoints get refs of their own. Nothing is left behind when it
/// fails: the step, the record paths and the repository are checked first, and a log that
/// cannot be written removes the worktree again.
pub async fn fork_to_worktree(
    sb: Arc<dyn Sandbox>,
    data_home: &Path,
    parent_log: &Path,
    req: ForkRequest,
) -> Result<WorktreeFork, WtError> {
    let repo_root = sb.root().to_path_buf();
    let log = parent_log.to_path_buf();
    let wanted = req.at;
    let (plan, state, project) = blocking(move || {
        let reader = SessionReader::<SessionRecord>::open(&log)?;
        let at = wanted.unwrap_or_else(|| step_at_offset(&reader, u64::MAX));
        let plan = plan(&reader, at)?;
        let state = state_at(&reader, at)?;
        Ok((plan, state, reader.header().project.clone()))
    })
    .await?;
    let id = new_session_id();
    let dir = req
        .dir
        .clone()
        .unwrap_or_else(|| default_dir(data_home, &project, &id));
    let (worktree, made) = make(Arc::clone(&sb), &repo_root, &state, &dir, &id).await?;
    let restored = SessionRecord::Checkpoint {
        n: plan.cut.checkpoints.max_n + 1,
        commit: made.commit.clone(),
        tree: made.tree.clone(),
        // The worktree's HEAD is that commit.
        head: Some(made.commit.clone()),
        dirty: false,
        turn: plan.cut.turn_count,
        step: plan.at,
        point: Some(CheckpointPoint::Restored),
    };
    let at = plan.at;
    let written = {
        let (log, worktree, id) = (parent_log.to_path_buf(), worktree.clone(), id.clone());
        blocking(move || {
            let reader = SessionReader::<SessionRecord>::open(&log)?;
            let fork = Fork {
                label: req.label,
                origin: Origin::WhatIf,
                model: req.model,
                id: Some(id),
                tail: vec![restored],
            };
            let child = fork_log(&reader, &plan, &worktree, fork)?;
            Ok(child.path().to_path_buf())
        })
        .await
    };
    let child_log = match written {
        Ok(path) => path,
        Err(e) => {
            // Best effort: the error says what failed either way.
            let _ = checkpoint::worktree_remove(sb.as_ref(), &repo_root, &worktree, true).await;
            return Err(e);
        }
    };
    let mut notes: Vec<String> = state.partial.into_iter().collect();
    if let Err(e) = adopt(&sb, &repo_root, &id, &child_log).await {
        notes.push(format!(
            "the fork's checkpoint refs could not be made ({e}); deleting the parent \
             session can make its checkpoints unavailable to the fork"
        ));
    }
    Ok(WorktreeFork {
        session_id: id,
        worktree,
        log: child_log,
        at,
        note: (!notes.is_empty()).then(|| notes.join("; ")),
    })
}

/// Refs of the fork `id`'s own for every checkpoint its log names (the copied ones and its
/// `restored` one), so deleting or pruning the parent never breaks it.
async fn adopt(
    sb: &Arc<dyn Sandbox>,
    repo_root: &Path,
    id: &str,
    log: &Path,
) -> Result<(), WtError> {
    let log = log.to_path_buf();
    let all = blocking(move || {
        let reader = SessionReader::<SessionRecord>::open(&log)?;
        let mut all = Vec::new();
        for rec in reader.records() {
            if let SessionRecord::Checkpoint { n, commit, .. } = rec?.1 {
                all.push((n, commit));
            }
        }
        Ok(all)
    })
    .await?;
    let cp = checkpointer(sb, repo_root, id)?;
    Ok(cp.adopt(id, &all).await?)
}

/// Remove the worktree at `dir` of the repository at `repo_root` (`git worktree remove`,
/// hardened as checkpoints are). A worktree with modified or untracked files is refused
/// ([`WtError::Dirty`]) unless `force`.
pub async fn remove(
    sb: &dyn Sandbox,
    repo_root: &Path,
    dir: &Path,
    force: bool,
) -> Result<(), WtError> {
    match checkpoint::worktree_remove(sb, repo_root, dir, force).await {
        Ok(()) => Ok(()),
        Err(CheckpointError::Git(why)) if !force && why.contains("modified or untracked") => {
            Err(WtError::Dirty(dir.to_path_buf()))
        }
        Err(e) => Err(e.into()),
    }
}
