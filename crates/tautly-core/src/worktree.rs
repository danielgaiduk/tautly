//! Fork at a step into an isolated git worktree (spec 7.12.4): `tautly fork <session> --at
//! N` and `session/new {fork: {…, worktree: true}}`.
//!
//! - **The worktree** ([`create`]) is `git worktree add --no-checkout --detach <dir>
//!   <commit>`, where `<commit>` is `state_at(N)`'s checkpoint with its overlay (the
//!   tool-edit reverts) applied, composed as a commit of its own; then a checkout (`read-tree
//!   --reset -u HEAD`) by git rooted at the new worktree, so the filters its own
//!   configuration defines (`includeIf "gitdir:…/worktrees/…"`, `config.worktree`) are found
//!   and switched off too. The new worktree is clean at exactly the files of step N, and
//!   `git status` there shows only what happens after. Git runs through the checkpoints'
//!   hardened path: no hook (`post-checkout` included, whatever `core.hooksPath` says),
//!   filter, fsmonitor or transport runs, and no provider key is in its environment. A
//!   directory inside any worktree of the repository is refused, and what a failed add or
//!   checkout made is removed again. The original work tree,
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
//! - **Cleanup** ([`remove`]): Tautly's own `git status` in the worktree's context first
//!   (modified or untracked files: [`WtError::Dirty`]; ignored ones, which would go with it:
//!   [`WtError::Ignored`]; both unless forced), then `git worktree remove --force`, so git
//!   runs no check of its own there.

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
    #[error(
        "the worktree {} has ignored files that would be deleted with it ({}); --force \
         removes it all the same",
        dir.display(),
        files.join(", ")
    )]
    Ignored { dir: PathBuf, files: Vec<String> },
    #[error(
        "{} is inside a worktree of the repository; choose a directory outside it",
        .0.display()
    )]
    InsideRepository(PathBuf),
    #[error(
        "the default worktree directory {} is inside a worktree of the repository (the data \
         home is); choose a directory elsewhere with --dir (over RPC, `fork.dir`), or move \
         the data home with TAUTLY_HOME",
        .0.display()
    )]
    DefaultDirInsideRepository(PathBuf),
    #[error(
        "{} is a symbolic link; give the directory itself",
        .0.display()
    )]
    DirIsLink(PathBuf),
    #[error(
        "the worktree {} holds populated submodules ({}), whose repositories would go with \
         it; --force removes it all the same",
        dir.display(),
        paths.join(", ")
    )]
    Submodules { dir: PathBuf, paths: Vec<String> },
    #[error(
        "the worktree {}'s HEAD {commit} has commits no branch or tag reaches, which would \
         be lost; keep them with `git branch <name> {commit}` first, or --force removes it \
         all the same",
        dir.display()
    )]
    Unreachable { dir: PathBuf, commit: String },
    // Not `#[from]`/`#[source]`: the message already says the cause, and a chain printed
    // with `{:#}` would say it twice (review M4).
    #[error("git: {0}")]
    Git(CheckpointError),
    #[error("{0}")]
    Tt(TtError),
    #[error("cannot read or write the session log: {0}")]
    Session(tautly_session::Error),
    #[error("a path in the session log cannot be used in the worktree: {0}")]
    Path(ConfineError),
    #[error("{}: {err}", path.display())]
    Io { path: PathBuf, err: std::io::Error },
    #[error("{0}")]
    Other(String),
}

impl From<CheckpointError> for WtError {
    fn from(e: CheckpointError) -> Self {
        WtError::Git(e)
    }
}

impl From<TtError> for WtError {
    fn from(e: TtError) -> Self {
        WtError::Tt(e)
    }
}

impl From<tautly_session::Error> for WtError {
    fn from(e: tautly_session::Error) -> Self {
        WtError::Session(e)
    }
}

impl From<ConfineError> for WtError {
    fn from(e: ConfineError) -> Self {
        WtError::Path(e)
    }
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
pub(crate) fn checkpointer(
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
    let base = base_of(state)?;
    let entries = overlay_entries(state, dir).await?;
    // A link is neither followed nor, on a failure, removed (re-review N2).
    if std::fs::symlink_metadata(dir).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(WtError::DirIsLink(dir.to_path_buf()));
    }
    refuse_inside_a_worktree(repo_root, dir)?;
    let made = cp
        .compose(&base, &entries, Checkpointer::housekeeping_deadline())
        .await?;
    let made_dirs = make_dir(dir)?;
    // Added without a checkout, then checked out in the new worktree's own configuration
    // context (review C1); on any failure what was made goes again (review M5).
    let added = async {
        checkpoint::worktree_add(sb.as_ref(), repo_root, dir, &made.commit).await?;
        checkpoint::worktree_checkout(dir).await?;
        dir.canonicalize().map_err(|err| WtError::Io {
            path: dir.to_path_buf(),
            err,
        })
    }
    .await;
    match added {
        Ok(canonical) => Ok((canonical, made)),
        Err(e) => {
            // Best effort: the error says what failed either way.
            let _ = checkpoint::worktree_remove(sb.as_ref(), repo_root, dir).await;
            made_dirs.undo();
            Err(e)
        }
    }
}

/// The checkpoint commit `state` starts from, or why it has none.
fn base_of(state: &FileState) -> Result<String, WtError> {
    state.base.clone().ok_or_else(|| {
        let note = state.partial.as_deref().unwrap_or(NOTE_NO_BASE);
        WtError::Partial(note.to_string())
    })
}

/// The overlay of `state` as `(path, content)` entries for [`Checkpointer::compose`], each
/// path resolved under `dir` (and never anywhere else), the contents read from the log's
/// blob store.
async fn overlay_entries(
    state: &FileState,
    dir: &Path,
) -> Result<Vec<(PathBuf, Option<Vec<u8>>)>, WtError> {
    let (log, overlay, dir) = (
        state.log().to_path_buf(),
        state.overlay.clone(),
        dir.to_path_buf(),
    );
    blocking(move || {
        let reader = SessionReader::<SessionRecord>::open(&log)?;
        let mut entries = Vec::with_capacity(overlay.len());
        for (path, blob) in &overlay {
            path.resolve(&dir)?;
            let bytes = blob.as_ref().map(|b| reader.read_blob(b)).transpose()?;
            entries.push((PathBuf::from(path.as_str()), bytes));
        }
        Ok(entries)
    })
    .await
}

/// `state` (its base checkpoint with its overlay applied) as a commit of its own, by
/// `cp`, for a worktree at `dir` ([`create`]; a bisect moves its worktree to one).
pub(crate) async fn compose_state(
    cp: &Checkpointer,
    state: &FileState,
    dir: &Path,
) -> Result<CheckpointRef, WtError> {
    let base = base_of(state)?;
    let entries = overlay_entries(state, dir).await?;
    Ok(cp
        .compose(&base, &entries, Checkpointer::housekeeping_deadline())
        .await?)
}

/// The directories [`make_dir`] created (outermost first), and whether `dir` was there.
#[derive(Debug)]
struct MadeDirs {
    dir: PathBuf,
    existed: bool,
    parents: Vec<PathBuf>,
}

impl MadeDirs {
    /// Remove what was made: `dir` (emptied again when it was there) and the parents that
    /// are empty again.
    fn undo(&self) {
        if self.existed {
            if let Ok(entries) = std::fs::read_dir(&self.dir) {
                for e in entries.flatten() {
                    let p = e.path();
                    let _ = match e.file_type() {
                        Ok(t) if t.is_dir() => std::fs::remove_dir_all(&p),
                        _ => std::fs::remove_file(&p),
                    };
                }
            }
        } else {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
        for p in self.parents.iter().rev() {
            let _ = std::fs::remove_dir(p);
        }
    }
}

/// The worktree's parent directories (private, as the data home's are); `dir` itself
/// must not exist, or be empty.
fn make_dir(dir: &Path) -> Result<MadeDirs, WtError> {
    let io = |err| WtError::Io {
        path: dir.to_path_buf(),
        err,
    };
    match std::fs::read_dir(dir) {
        Ok(mut entries) => {
            return match entries.next() {
                Some(_) => Err(WtError::Exists(dir.to_path_buf())),
                None => Ok(MadeDirs {
                    dir: dir.to_path_buf(),
                    existed: true,
                    parents: Vec::new(),
                }),
            };
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(io(e)),
    }
    let mut parents: Vec<PathBuf> = dir
        .ancestors()
        .skip(1)
        .take_while(|p| std::fs::symlink_metadata(p).is_err())
        .map(Path::to_path_buf)
        .collect();
    parents.reverse();
    if let Some(parent) = dir.parent() {
        tautly_session::paths::create_private_dir_all(parent).map_err(io)?;
    }
    Ok(MadeDirs {
        dir: dir.to_path_buf(),
        existed: false,
        parents,
    })
}

/// `path` with its nearest existing ancestor resolved (links followed), the rest as given.
pub(crate) fn resolved(path: &Path) -> PathBuf {
    let mut rest = Vec::new();
    let mut at = path;
    loop {
        if let Ok(real) = at.canonicalize() {
            return rest.iter().rev().fold(real, |p, c| p.join(c));
        }
        match (at.parent(), at.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_os_string());
                at = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
}

/// Refuse a worktree directory inside any worktree of the repository at `repo_root` (the
/// main one, a linked one): it would show up in that tree, and its checkpoints would meet
/// a repository inside it (review M3). The worktrees are read from git's files (no git is
/// run).
fn refuse_inside_a_worktree(repo_root: &Path, dir: &Path) -> Result<(), WtError> {
    let dir_real = resolved(dir);
    let mut trees = vec![resolved(repo_root)];
    if let Some(dirs) = tautly_session::git_dirs_at(&repo_root.join(".git")) {
        // The main worktree, when the repository is its `.git` (not a bare one).
        if dirs.common_dir.file_name().is_some_and(|n| n == ".git")
            && let Some(main) = dirs.common_dir.parent()
        {
            trees.push(resolved(main));
        }
        if let Ok(entries) = std::fs::read_dir(dirs.common_dir.join("worktrees")) {
            for e in entries.flatten() {
                // `gitdir` names the worktree's `.git` file.
                if let Ok(text) = std::fs::read_to_string(e.path().join("gitdir"))
                    && let Some(tree) = Path::new(text.trim()).parent()
                {
                    trees.push(resolved(tree));
                }
            }
        }
    }
    if trees.iter().any(|t| dir_real.starts_with(t)) {
        return Err(WtError::InsideRepository(dir.to_path_buf()));
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
        parent_step: Some(plan.at),
        label: fork.label,
        origin: fork.origin,
        // The copied records start from the parent's first model.
        model: header.model.clone(),
        // The parent's, as a fork or rewind inherits them (cp5 review C M9).
        config_hash: header.config_hash.clone(),
        tags: header.tags.clone(),
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
        // Both are written for the copy, at its last record's time.
        child.append_at_last(&SessionRecord::Forked)?;
        if plan.cut.incomplete_turn {
            child.append_at_last(&SessionRecord::TurnEnd {
                reason: crate::FinishReason::Cancelled,
                evidence: None,
                synthetic: true,
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
    let defaulted = req.dir.is_none();
    let (worktree, made) = make(Arc::clone(&sb), &repo_root, &state, &dir, &id)
        .await
        .map_err(|e| match e {
            WtError::InsideRepository(d) if defaulted => WtError::DefaultDirInsideRepository(d),
            e => e,
        })?;
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
            let _ = checkpoint::worktree_remove(sb.as_ref(), &repo_root, &worktree).await;
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

/// Remove the worktree at `dir` of the repository at `repo_root` (`git worktree remove
/// --force`, hardened as checkpoints are). Unless `force`, it is first checked with
/// Tautly's own `git status` in the worktree's configuration context (review C1, M2): a
/// worktree with modified or untracked files is refused ([`WtError::Dirty`]), and so is
/// one with ignored files, which would go with it ([`WtError::Ignored`], naming them;
/// review M8).
///
/// Also unless `force` (re-review N1, N3): one holding populated submodules is refused
/// ([`WtError::Submodules`]), as git refuses it, and so is one whose HEAD is neither
/// `start` (the commit the fork began at) nor reached by a branch or tag
/// ([`WtError::Unreachable`], naming it): a commit made there would be lost.
pub async fn remove(
    sb: &dyn Sandbox,
    repo_root: &Path,
    dir: &Path,
    start: Option<&str>,
    force: bool,
) -> Result<(), WtError> {
    if !force {
        let submodules = checkpoint::worktree_submodules(dir).await?;
        let (changed, ignored) = checkpoint::worktree_status(dir).await?;
        if !submodules.is_empty() {
            return Err(WtError::Submodules {
                dir: dir.to_path_buf(),
                paths: submodules,
            });
        }
        if !changed.is_empty() {
            return Err(WtError::Dirty(dir.to_path_buf()));
        }
        if !ignored.is_empty() {
            return Err(WtError::Ignored {
                dir: dir.to_path_buf(),
                files: ignored,
            });
        }
        // Read from git's files; the reachability by git in the main repository's context.
        if let Some(head) = tautly_session::git_head(dir)
            && start != Some(head.as_str())
            && !checkpoint::reached_by_a_branch_or_tag(sb, repo_root, &head).await?
        {
            return Err(WtError::Unreachable {
                dir: dir.to_path_buf(),
                commit: head,
            });
        }
    }
    checkpoint::worktree_remove(sb, repo_root, dir).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_a_failed_add_made_goes_again() {
        // Review M5: the directory and the parents made for it.
        let base = tempfile::tempdir().unwrap();
        let dir = base.path().join("a/b/wt");
        let made = make_dir(&dir).unwrap();
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("f"), "x").unwrap();
        made.undo();
        assert!(!base.path().join("a").exists());
        // A directory that was there (empty) stays, emptied.
        let there = base.path().join("there");
        std::fs::create_dir(&there).unwrap();
        let made = make_dir(&there).unwrap();
        std::fs::write(there.join("f"), "x").unwrap();
        made.undo();
        assert!(there.exists());
        assert_eq!(std::fs::read_dir(&there).unwrap().count(), 0);
    }

    #[test]
    fn status_lists_changes_and_ignored_files_but_not_the_scratch_store() {
        let out = b" M a.txt\0?? new.txt\0R  to.txt\0from.txt\0!! notes.log\0!! .tautly/tmp/\0!! .tautly/tmp/spill/x\0";
        let (changed, ignored) = checkpoint::parse_status(out);
        assert_eq!(changed, ["a.txt", "new.txt", "to.txt"]);
        assert_eq!(ignored, ["notes.log"]);
    }
}
