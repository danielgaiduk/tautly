//! Git checkpoints of the work tree (spec 7.11.11): the single checkpoint mechanism that
//! undo and time travel restore from.
//!
//! A checkpoint is a commit of the whole work tree (tracked and untracked files, ignored
//! ones left out) at `refs/tautly/checkpoints/<session>/<n>` in the user's repository,
//! made with plumbing through [`Sandbox::spawn`]:
//!
//! 1. `read-tree` of HEAD (the empty tree when HEAD is unborn) into a temporary index
//!    (`GIT_INDEX_FILE`) the checkpointer keeps for the session, so later snapshots only
//!    rehash files whose stat data changed;
//! 2. `ls-files --others --exclude-standard`, to skip the snapshot when untracked files
//!    exceed the limits;
//! 3. `add -A` (then `rm --cached` of the spill store `.tautly/tmp`, whatever the ignore
//!    rules say), `write-tree`, `commit-tree -p <previous checkpoint or HEAD>`,
//!    `update-ref`.
//!
//! The user's index, HEAD, branches, stash and ignored files are never read or written:
//! only objects and `refs/tautly/` change, except on an explicit restore, which writes the
//! work tree. Nothing the repository configures runs: every git call names the repository
//! and the root as its work tree (`--git-dir`, `--work-tree`: neither the environment nor
//! `core.worktree` can move it), runs with `core.fsmonitor=false`,
//! `core.hooksPath=/dev/null`, no submodule recursion, no transport, credential helper,
//! askpass, pager or editor, no system configuration, and every filter driver the
//! configuration defines switched off (so files tracked through Git LFS or another filter
//! are snapshotted as their bytes in the work tree); also no split index, `gc.auto=0`, no
//! signing, literal pathspecs. No object is ever fetched: lazy fetching is off
//! (`GIT_NO_LAZY_FETCH`), and every protocol is disallowed (`GIT_ALLOW_PROTOCOL` empty,
//! and each `protocol.<name>.allow` the configuration sets overridden with `never`); in a
//! partial clone, a git older than 2.44 (which ignores `GIT_NO_LAZY_FETCH`) is not run at
//! all and checkpoints are off for the session. A root whose `.git` is a symbolic link
//! gets no checkpoints either. Each runs in a process group of its own that is killed at
//! the deadline (`timeout_ms` for a snapshot), when the call is dropped (a cancel) or by
//! the session's process-group registry.
//!
//! Nothing but the work tree's own files goes into a checkpoint: the exclude rules it was
//! taken with (which undo reads back) are kept in the session data home
//! ([`with_excludes_store`](Checkpointer::with_excludes_store)), since a mirror push
//! publishes `refs/tautly/` (session start warns about such remotes).
//!
//! An unchanged tree reuses the previous checkpoint (same `n` and commit), and so does a
//! [`Checkpointer::snapshot`] whose write generation did not move and whose caller's
//! watcher saw no change, without running git at all.

use std::collections::HashSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tautly_sandbox::{Sandbox, SandboxError, SpawnSpec};
use tautly_tools::ProcessGroups;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

mod excludes_store;
mod git;
mod job;
mod worktree;

use self::excludes_store::ExcludesStore;
pub(crate) use self::git::Change;
use self::git::*;
pub(crate) use self::job::{GitCheckpoints, Point, StartCheckpoint};
#[cfg(test)]
pub(crate) use self::worktree::parse_status;
pub use self::worktree::{delete_session_refs, prune};
pub(crate) use self::worktree::{
    reached_by_a_branch_or_tag, worktree_add, worktree_checkout, worktree_move, worktree_remove,
    worktree_status, worktree_submodules,
};
use crate::config::CheckpointConfig;
use crate::record::{CheckpointPoint, SessionRecord};
use crate::session::{EventSink, Log, Session};

/// The directory, next to a project's session logs, where each checkpoint's exclude
/// rules are kept ([`Checkpointer::with_excludes_store`]).
pub const EXCLUDES_DIR: &str = "checkpoint-excludes";

/// Every checkpoint ref is below this.
pub const REF_PREFIX: &str = "refs/tautly/checkpoints";
/// Deadline of a restore, prune, ref deletion or adoption (user-initiated or background
/// work, not on a command's path).
const HOUSEKEEPING_TIMEOUT: Duration = Duration::from_secs(60);
/// Most bytes read from one git call's output.
const MAX_OUTPUT: usize = 64 * 1024 * 1024;
/// Most bytes of git's error output kept in a message.
const MAX_ERROR: usize = 300;
/// The empty tree's id (SHA-1 and SHA-256 repositories).
const EMPTY_TREES: [&str; 2] = [
    "4b825dc642cb6eb9a060e54bf8d69288fbee4904",
    "6ef19b41225c5369f1c104d45d8d85efa9b057b53b14b4b9b939dd74decc5321",
];
/// A gitlink (submodule) entry's mode: never restored.
const GITLINK: &str = "160000";
/// Tautly's scratch directory in the work tree (the spill store): left out of every
/// checkpoint.
const SPILL_STORE: &str = ".tautly/tmp";

/// A checkpoint taken (or reused).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointRef {
    /// Its number in the session (`refs/tautly/checkpoints/<session>/<n>`).
    pub n: u32,
    pub commit: String,
    /// The full ref name.
    pub refname: String,
    pub tree: String,
    /// HEAD's commit when it was taken (none: unborn).
    pub head: Option<String>,
    /// The tree differs from HEAD's.
    pub dirty: bool,
    /// Nothing changed since the previous checkpoint, which this is.
    pub reused: bool,
}

/// Why a snapshot was not taken (its `Display` is the reason a warning and the
/// `CheckpointSkipped` record show).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Skip {
    #[error("git is not available")]
    NoGit,
    #[error("the snapshot took longer than the time limit ([checkpoints] timeout_ms)")]
    Timeout,
    #[error(
        "too many untracked files ({files} files, {bytes} bytes; [checkpoints] \
         max_untracked_files / max_untracked_mb)"
    )]
    TooManyUntracked { files: u64, bytes: u64 },
    #[error("git failed: {0}")]
    Git(String),
    /// A partial clone (a promisor remote), and a git older than 2.44, which cannot be
    /// kept from fetching missing objects (and running what the remote's URL names).
    #[error(
        "the repository is a partial clone and git is older than 2.44, which could fetch \
         from its promisor remote (checkpoints are off for this session)"
    )]
    UnsafePartialClone,
    /// The root's `.git` is a symbolic link: checkpoints would write to whatever
    /// repository it points at.
    #[error("the work tree's .git is a symbolic link (checkpoints are off for this session)")]
    GitDirSymlink,
    /// Reading the repository's git configuration did not finish in
    /// `CONFIG_READ_TIMEOUT` for the second snapshot in a row: an include that never ends
    /// (a FIFO), which every later git call would wait on too.
    #[error(
        "reading the repository's git configuration took too long (an include that never \
         ends?); checkpoints are off for this session"
    )]
    ConfigTimeout,
    /// The root's `.git` names a git directory (a `.git` file, or a `commondir`) that
    /// git's own links do not confirm belongs to this work tree
    /// ([`tautly_session::git_dirs_at`]): checkpoints could write to another repository.
    #[error(
        "the work tree's .git names a repository that does not name it back \
         (checkpoints are off for this session)"
    )]
    GitDirUnverified,
}

/// What a restore changes (with `dry_run`: would change), as absolute paths.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RestorePlan {
    /// Files written with the checkpoint's content (changed or deleted since).
    pub restored: Vec<PathBuf>,
    /// Files deleted (added since; untracked and not ignored).
    pub deleted: Vec<PathBuf>,
}

/// Why a checkpoint operation failed.
#[derive(Debug, thiserror::Error)]
pub enum CheckpointError {
    #[error("git is not available")]
    NoGit,
    #[error("git took too long")]
    Timeout,
    #[error("git failed: {0}")]
    Git(String),
    #[error("{path}: {source}")]
    Disk { path: String, source: SandboxError },
    #[error("{0}")]
    Invalid(String),
}

impl From<Skip> for CheckpointError {
    fn from(s: Skip) -> Self {
        match s {
            Skip::NoGit => CheckpointError::NoGit,
            Skip::Timeout | Skip::ConfigTimeout => CheckpointError::Timeout,
            Skip::Git(why) => CheckpointError::Git(why),
            other @ (Skip::TooManyUntracked { .. }
            | Skip::UnsafePartialClone
            | Skip::GitDirSymlink
            | Skip::GitDirUnverified) => CheckpointError::Invalid(other.to_string()),
        }
    }
}

impl Skip {
    /// It holds for the rest of the session: said once, and no checkpoint is tried again.
    pub fn is_permanent(&self) -> bool {
        matches!(
            self,
            Skip::UnsafePartialClone
                | Skip::GitDirSymlink
                | Skip::GitDirUnverified
                | Skip::ConfigTimeout
        )
    }

    /// Git failed because an object it needs is missing (or a fetch of one was refused):
    /// a damaged repository, or a partial clone that lacks it. It will not heal by itself.
    pub fn is_missing_object(&self) -> bool {
        let Skip::Git(why) = self else {
            return false;
        };
        let why = why.to_ascii_lowercase();
        [
            "unable to read",
            "missing blob",
            "missing tree",
            "missing object",
            "bad object",
            "not a valid object",
            "lazy fetching disabled",
            "could not fetch",
            "from promisor remote",
        ]
        .iter()
        .any(|m| why.contains(m))
            || (why.contains("transport '") && why.contains("not allowed"))
    }

    /// The reason a `CheckpointSkipped` record gives (`cancelled` is the other one).
    pub fn code(&self) -> &'static str {
        match self {
            Skip::NoGit => "no_git",
            Skip::Timeout => "timeout",
            Skip::ConfigTimeout => "config_timeout",
            Skip::TooManyUntracked { .. } => "too_many_untracked",
            Skip::Git(_) => "git_failed",
            Skip::UnsafePartialClone => "unsafe_partial_clone",
            Skip::GitDirSymlink => "git_dir_symlink",
            Skip::GitDirUnverified => "git_dir_unverified",
        }
    }
}

/// The warning shown for a skipped checkpoint.
pub fn skip_warning(skip: &Skip) -> String {
    format!("checkpoint skipped: {skip}")
}

/// The warning for remotes a push to would publish checkpoint refs to.
fn publishing_warning(remotes: &[String]) -> String {
    let names: Vec<String> = remotes.iter().map(|r| format!("{r:?}")).collect();
    format!(
        "a push to remote {} would publish the checkpoint refs under {REF_PREFIX}/ \
         (mirror, or a push refspec that covers them), with the untracked files they hold",
        names.join(", ")
    )
}

/// The ref of checkpoint `name` (a number, or `undo-<n>`) of `session`.
pub fn ref_name(session: &str, name: &str) -> String {
    format!("{REF_PREFIX}/{session}/{name}")
}

/// How one git call failed.
#[derive(Debug)]
enum Fail {
    NoGit,
    Timeout,
    /// The configuration read ([`Git::read_config`]) did not finish in
    /// [`CONFIG_READ_TIMEOUT`].
    ConfigTimeout,
    /// It ran and exited non-zero (`code`; none: killed by a signal).
    Exit {
        code: Option<i32>,
        stderr: String,
    },
    Io(String),
    /// A partial clone, and a git that may fetch missing objects (see
    /// [`Skip::UnsafePartialClone`]): nothing was run.
    UnsafePartialClone,
}

impl From<Fail> for Skip {
    fn from(f: Fail) -> Self {
        match f {
            Fail::NoGit => Skip::NoGit,
            Fail::UnsafePartialClone => Skip::UnsafePartialClone,
            Fail::Timeout => Skip::Timeout,
            Fail::ConfigTimeout => Skip::ConfigTimeout,
            Fail::Exit { stderr, code } => Skip::Git(if stderr.is_empty() {
                format!("exit status {code:?}")
            } else {
                stderr
            }),
            Fail::Io(why) => Skip::Git(why),
        }
    }
}

impl From<Fail> for CheckpointError {
    fn from(f: Fail) -> Self {
        Skip::from(f).into()
    }
}
/// The work tree as a tree object, HEAD then, and the exclude rules then (the user's
/// excludes file, then `info/exclude`; none: unknown).
struct TreeNow {
    tree: String,
    head: Option<(String, String)>,
    excludes: Option<Vec<u8>>,
}

impl TreeNow {
    fn dirty(&self) -> bool {
        match &self.head {
            Some((_, head_tree)) => *head_tree != self.tree,
            None => !EMPTY_TREES.contains(&self.tree.as_str()),
        }
    }
}

/// The latest checkpoint, the write generation it was taken at (none: unknown), and the
/// exclude rules it keeps (none: unknown).
struct Last {
    r: CheckpointRef,
    write_gen: Option<u64>,
    excludes: Option<Option<Vec<u8>>>,
}

/// A session's git checkpoints (see the module docs).
pub struct Checkpointer {
    sb: Arc<dyn Sandbox>,
    root: PathBuf,
    /// The repository the root's `.git` names.
    git_dir: PathBuf,
    session: String,
    cfg: CheckpointConfig,
    program: PathBuf,
    groups: Option<ProcessGroups>,
    /// Holds the temporary index (`index`) and pathspec files; removed with it.
    tmp: tempfile::TempDir,
    index: PathBuf,
    /// The number the next checkpoint gets.
    next: u32,
    last: Option<Last>,
    version: GitVersion,
    /// Where each checkpoint's exclude rules are kept (none: not kept, and undo uses the
    /// rules as they are then).
    excludes: Option<ExcludesStore>,
    /// Snapshots in a row whose configuration read ran out of [`CONFIG_READ_TIMEOUT`].
    config_timeouts: u32,
    /// Variables no git call inherits: the provider key variables.
    env_strip: Vec<String>,
}

impl Checkpointer {
    /// A checkpointer for the work tree at `root` (the sandbox root, a git top level):
    /// none when `root` is not a git work tree (it has no usable `.git`; a bare repository
    /// has none), when no `git` is on `PATH`, or when no temporary directory can be made.
    /// Runs no git.
    pub fn new(
        sb: Arc<dyn Sandbox>,
        root: PathBuf,
        session: String,
        cfg: CheckpointConfig,
    ) -> Option<Checkpointer> {
        let git_dir = git_dir(&root)?;
        let program = git_on_path(&root)?;
        let tmp = tempfile::Builder::new()
            .prefix("tautly-checkpoint-")
            .tempdir()
            .ok()?;
        let index = tmp.path().join("index");
        Some(Checkpointer {
            sb,
            root,
            git_dir,
            session,
            cfg,
            program,
            groups: None,
            tmp,
            index,
            next: 1,
            last: None,
            version: GitVersion::new(),
            excludes: None,
            config_timeouts: 0,
            env_strip: default_env_strip(),
        })
    }

    /// Keep each checkpoint's exclude rules (the user's excludes file and the
    /// repository's `info/exclude` as they are when it is taken) in `dir`, a private
    /// directory of the session data home (never the repository), for undo to read back.
    pub fn with_excludes_store(mut self, dir: PathBuf) -> Self {
        self.excludes = Some(ExcludesStore::new(dir));
        self
    }

    /// The directory of the store keeping each checkpoint's exclude rules, if any.
    pub fn excludes_dir(&self) -> Option<&Path> {
        self.excludes.as_ref().map(ExcludesStore::dir)
    }

    /// Also keep `vars` (the session's provider key variables: the defaults plus every
    /// configured `api_key_env`) from every git call.
    pub fn with_env_remove(mut self, vars: Vec<String>) -> Self {
        for v in vars {
            if !self.env_strip.contains(&v) {
                self.env_strip.push(v);
            }
        }
        self
    }

    /// Run this git program instead of the one on `PATH`.
    pub fn with_git(mut self, program: impl Into<PathBuf>) -> Self {
        self.program = program.into();
        self
    }

    /// Register git's process groups here while they run (the session's registry, which
    /// kills what is left at shutdown or on a signal escalation).
    pub fn with_process_groups(mut self, groups: ProcessGroups) -> Self {
        self.groups = Some(groups);
        self
    }

    /// The session whose refs new checkpoints get.
    pub fn session(&self) -> &str {
        &self.session
    }

    /// The checkpoint settings this checkpointer runs with.
    pub fn config(&self) -> &CheckpointConfig {
        &self.cfg
    }

    /// The latest checkpoint taken or known.
    pub fn last(&self) -> Option<&CheckpointRef> {
        self.last.as_ref().map(|l| &l.r)
    }

    /// Continue a session whose log already has checkpoints: numbers go on after
    /// `max_n`, and `last` (`n`, commit, tree) is the latest one (it is not reused
    /// without running git, since what changed since is unknown).
    pub fn continue_after(&mut self, max_n: u32, last: Option<(u32, String, String)>) {
        self.next = self.next.max(max_n.saturating_add(1));
        if let Some((n, commit, tree)) = last {
            self.last = Some(Last {
                r: CheckpointRef {
                    n,
                    refname: ref_name(&self.session, &n.to_string()),
                    commit,
                    tree,
                    head: None,
                    dirty: false,
                    reused: false,
                },
                write_gen: None,
                excludes: None,
            });
        }
    }

    /// Continue as fork `session`, whose log copied checkpoints up to `max_n` and whose
    /// latest one is `last` (`n`, commit, tree; the branch has a ref of its own for it, or
    /// none: nothing is reused). The parent's later checkpoints are never reused.
    pub(crate) fn branch(
        &mut self,
        session: String,
        max_n: u32,
        last: Option<(u32, String, String)>,
    ) {
        self.session = session;
        self.next = max_n.saturating_add(1);
        self.last = None;
        self.continue_after(max_n, last);
    }

    /// Something other than a snapshot changed files (a restore): the next snapshot runs
    /// git whatever the write generation says.
    pub fn invalidate(&mut self) {
        if let Some(l) = &mut self.last {
            l.write_gen = None;
        }
    }

    fn git(&self) -> Git<'_> {
        Git {
            env_strip: &self.env_strip,
            ..Git::new(
                self.sb.as_ref(),
                &self.root,
                &self.git_dir,
                &self.program,
                Some(&self.index),
                self.groups.as_ref(),
                &self.version,
            )
        }
    }

    fn deadline(&self) -> Instant {
        Instant::now() + Duration::from_millis(self.cfg.timeout_ms)
    }

    /// The commit HEAD names, by the hardened git; none when HEAD is unborn or git
    /// failed (`tautly eval` records it after a task's setup, for its verify script).
    pub async fn head_commit(&self) -> Option<String> {
        let git = Git {
            index: None,
            ..self.git()
        };
        match git.head(self.deadline()).await {
            Ok(Some((commit, _tree))) => Some(commit),
            _ => None,
        }
    }

    /// A checkpoint of the work tree as it is now, `write_gen` being the session's write
    /// generation: the previous one when nothing changed since (the same generation and
    /// `watcher_changed` false, without running git; or the same tree). Bounded by
    /// `timeout_ms` and the untracked-file limits.
    pub async fn snapshot(
        &mut self,
        write_gen: u64,
        watcher_changed: bool,
    ) -> Result<CheckpointRef, Skip> {
        if let Some(l) = &self.last
            && l.write_gen == Some(write_gen)
            && !watcher_changed
        {
            return Ok(CheckpointRef {
                reused: true,
                ..l.r.clone()
            });
        }
        let made = self.take(false).await?;
        if let Some(l) = &mut self.last {
            l.write_gen = Some(write_gen);
        }
        Ok(made)
    }

    /// A checkpoint of the work tree now, whatever the write generation says (before a
    /// rewind or switch, and at session start).
    pub async fn snapshot_now(&mut self) -> Result<CheckpointRef, Skip> {
        let made = self.take(false).await?;
        self.invalidate();
        Ok(made)
    }

    /// A snapshot of the work tree at `refs/tautly/checkpoints/<session>/undo-<n>`, taken
    /// before an undo or rewind writes, so what it overwrites can be had back with git.
    /// Always a new ref; it does not become the latest checkpoint.
    pub async fn backup(&mut self) -> Result<CheckpointRef, Skip> {
        let deadline = self.deadline();
        let now = self.tree_now(true, deadline).await?;
        self.commit_backup(&now.tree, deadline).await
    }

    /// [`backup`](Self::backup) of a tree already written.
    pub(crate) async fn commit_backup(
        &mut self,
        tree: &str,
        deadline: Instant,
    ) -> Result<CheckpointRef, Skip> {
        let head = self.git().head(deadline).await?;
        let now = TreeNow {
            tree: tree.to_string(),
            head,
            excludes: None,
        };
        self.commit(&now, true, deadline).await
    }

    /// The work tree's tree object now, written to the object store (with `limits`, the
    /// untracked-file limits apply), within `deadline`.
    pub(crate) async fn tree_id(&self, limits: bool, deadline: Instant) -> Result<String, Skip> {
        Ok(self.tree_now(limits, deadline).await?.tree)
    }

    /// A deadline for work that is not on a command's path (an undo, a restore).
    pub(crate) fn housekeeping_deadline() -> Instant {
        Instant::now() + HOUSEKEEPING_TIMEOUT
    }

    /// [`Checkpointer::take_once`], with a configuration read that runs out of its cap
    /// counted: the first time is an ordinary [`Skip::Timeout`] (a slow network or synced
    /// filesystem may recover), the second in a row [`Skip::ConfigTimeout`], which turns
    /// checkpoints off for the session.
    async fn take(&mut self, backup: bool) -> Result<CheckpointRef, Skip> {
        match self.take_once(backup).await {
            Err(Skip::ConfigTimeout) => {
                self.config_timeouts += 1;
                Err(if self.config_timeouts >= 2 {
                    Skip::ConfigTimeout
                } else {
                    Skip::Timeout
                })
            }
            other => {
                self.config_timeouts = 0;
                other
            }
        }
    }

    async fn take_once(&mut self, backup: bool) -> Result<CheckpointRef, Skip> {
        let deadline = self.deadline();
        let now = self.tree_now(true, deadline).await?;
        // The same tree, kept with the same exclude rules (which undo reads back).
        if !backup
            && let Some(l) = &self.last
            && l.r.tree == now.tree
            && l.excludes.as_ref() == Some(&now.excludes)
        {
            let r = CheckpointRef {
                head: now.head.as_ref().map(|(c, _)| c.clone()),
                dirty: now.dirty(),
                reused: true,
                ..l.r.clone()
            };
            return Ok(r);
        }
        self.commit(&now, backup, deadline).await
    }

    /// Commit `now` on top of the previous checkpoint (or HEAD) and give it the next
    /// number's ref (`undo-<n>` for a backup), which must not exist yet.
    async fn commit(
        &mut self,
        now: &TreeNow,
        backup: bool,
        deadline: Instant,
    ) -> Result<CheckpointRef, Skip> {
        let git = self.git();
        let parent = self
            .last
            .as_ref()
            .map(|l| l.r.commit.clone())
            .or_else(|| now.head.as_ref().map(|(c, _)| c.clone()));
        let message = format!("tautly checkpoint ({})\n", self.session);
        let mut args = vec!["commit-tree", "--no-gpg-sign", now.tree.as_str()];
        if let Some(p) = &parent {
            args.extend(["-p", p.as_str()]);
        }
        let out = git.run(&args, Some(message.into_bytes()), deadline).await?;
        let commit = String::from_utf8_lossy(&out).trim().to_string();
        // The exclude rules go with the checkpoint, outside the repository: undo decides
        // what was ignored at the checkpoint with them (see `ignored_under`).
        let excludes = match (&now.excludes, &self.excludes) {
            (Some(rules), Some(store)) => store.put(&commit, rules).is_ok().then(|| rules.clone()),
            _ => None,
        };
        // A number already taken (a backup of an earlier run, an old log) moves on.
        let mut n = self.next;
        let refname = loop {
            let name = if backup {
                format!("undo-{n}")
            } else {
                n.to_string()
            };
            let refname = ref_name(&self.session, &name);
            let create = format!("create {refname}\0{commit}\0").into_bytes();
            match git.update_refs(create, deadline).await {
                Ok(()) => break refname,
                Err(Fail::Exit { .. }) if n < self.next.saturating_add(64) => n += 1,
                Err(e) => return Err(e.into()),
            }
        };
        self.next = n + 1;
        let made = CheckpointRef {
            n,
            commit,
            refname,
            tree: now.tree.clone(),
            head: now.head.as_ref().map(|(c, _)| c.clone()),
            dirty: now.dirty(),
            reused: false,
        };
        if !backup {
            self.last = Some(Last {
                r: made.clone(),
                write_gen: None,
                excludes: Some(excludes),
            });
        }
        Ok(made)
    }

    /// Write the work tree into the temporary index and then the object store.
    async fn tree_now(&self, limits: bool, deadline: Instant) -> Result<TreeNow, Skip> {
        let git = self.git();
        // A snapshot abandoned half way (a cancel, the deadline) can leave git's lock on
        // the temporary index; nothing else uses it.
        let _ = std::fs::remove_file(self.tmp.path().join("index.lock"));
        let head = git.head(deadline).await?;
        let excludes = self.excludes_now(&git, deadline).await?;
        match &head {
            // `--reset` keeps the stat data of entries that match (without the up-to-date
            // check of `-m`), so `add -A` only rehashes what changed since the last
            // snapshot.
            Some((commit, _)) if self.index.exists() => {
                git.run(&["read-tree", "--reset", commit], None, deadline)
                    .await?
            }
            Some((commit, _)) => git.run(&["read-tree", commit], None, deadline).await?,
            None => git.run(&["read-tree", "--empty"], None, deadline).await?,
        };
        if limits {
            self.check_untracked(&git, deadline).await?;
        }
        git.run(&["add", "-A"], None, deadline).await?;
        // The spill store (full command output) never goes in, whatever the repository's
        // ignore rules say: a committed `.tautly/tmp/.gitignore` can un-ignore it, which
        // no exclude rule of ours could outrank.
        let spill = [
            "rm",
            "--cached",
            "-r",
            "-f",
            "-q",
            "--ignore-unmatch",
            "--",
            SPILL_STORE,
        ];
        git.run(&spill, None, deadline).await?;
        let tree = git.text(&["write-tree"], deadline).await?;
        Ok(TreeNow {
            tree,
            head,
            excludes,
        })
    }

    /// The exclude rules outside the work tree now: the user's excludes file (see
    /// [`Git::user_excludes_file`]), then the repository's `info/exclude` (later rules
    /// win, as in git). The user's file may be a symbolic link (only the user's own
    /// configuration names it, so it is followed); `info/exclude` is repository content
    /// and is never read through one. None when no store keeps them, when one cannot be
    /// read (or `info` is a symbolic link), or when they are longer than [`MAX_EXCLUDES`].
    async fn excludes_now(
        &self,
        git: &Git<'_>,
        deadline: Instant,
    ) -> Result<Option<Vec<u8>>, Skip> {
        if self.excludes.is_none() {
            return Ok(None);
        }
        let user = match git.user_excludes_file(deadline).await {
            Ok(path) => path,
            Err(Fail::Exit { .. } | Fail::Io(_)) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let info = common_dir(&self.git_dir).join("info");
        if std::fs::symlink_metadata(&info).is_ok_and(|m| m.file_type().is_symlink()) {
            return Ok(None);
        }
        let mut rules = Vec::new();
        let sources = [(user, true), (Some(info.join("exclude")), false)];
        for (path, follow) in sources {
            let Some(path) = path else {
                continue;
            };
            match read_regular(&self.root.join(path), follow, MAX_EXCLUDES) {
                Ok(Some(bytes)) => {
                    rules.extend(bytes);
                    rules.push(b'\n');
                }
                Ok(None) => {}
                Err(()) => return Ok(None),
            }
        }
        Ok((rules.len() as u64 <= MAX_EXCLUDES).then_some(rules))
    }

    /// Skip when untracked (not ignored) files exceed the configured limits.
    async fn check_untracked(&self, git: &Git<'_>, deadline: Instant) -> Result<(), Skip> {
        let out = git
            .run(
                &["ls-files", "--others", "--exclude-standard", "-z"],
                None,
                deadline,
            )
            .await?;
        let paths: Vec<PathBuf> = out
            .split(|&b| b == 0)
            .filter(|p| !p.is_empty())
            .map(|p| self.root.join(bytes_to_os(p)))
            .collect();
        let files = paths.len() as u64;
        let max_bytes = self.cfg.max_untracked_mb.saturating_mul(1024 * 1024);
        if files > self.cfg.max_untracked_files {
            return Err(Skip::TooManyUntracked { files, bytes: 0 });
        }
        let sizes = tokio::task::spawn_blocking(move || {
            paths
                .iter()
                .filter_map(|p| std::fs::symlink_metadata(p).ok())
                .map(|m| m.len())
                .sum::<u64>()
        });
        let bytes = tokio::time::timeout_at(deadline, sizes)
            .await
            .map_err(|_| Skip::Timeout)?
            .unwrap_or(0);
        if bytes > max_bytes {
            return Err(Skip::TooManyUntracked { files, bytes });
        }
        Ok(())
    }

    /// The remotes a push to would publish this repository's checkpoint refs to (and the
    /// untracked files they hold): `remote.<name>.mirror` set, or a push refspec that can
    /// name a ref below `refs/tautly/` (`pushes_checkpoints`). Empty when the
    /// configuration cannot be read.
    pub async fn publishing_remotes(&self) -> Vec<String> {
        let git = Git {
            index: None,
            ..self.git()
        };
        let out = match git
            .run(
                &[
                    "config",
                    "-z",
                    "--get-regexp",
                    r"^remote\..+\.(mirror|push)$",
                ],
                None,
                Self::housekeeping_deadline(),
            )
            .await
        {
            Ok(out) => out,
            Err(_) => return Vec::new(),
        };
        let mut remotes = std::collections::BTreeSet::new();
        for entry in out.split(|&b| b == 0).filter(|e| !e.is_empty()) {
            let text = String::from_utf8_lossy(entry);
            // A key without a value has no LF.
            let (key, value) = match text.split_once('\n') {
                Some((k, v)) => (k, Some(v)),
                None => (text.as_ref(), None),
            };
            let Some((name, var)) = key
                .strip_prefix("remote.")
                .and_then(|rest| rest.rsplit_once('.'))
            else {
                continue;
            };
            let publishes = match var {
                // A boolean: no value is true, an empty one false.
                "mirror" => value.is_none_or(|v| {
                    !matches!(
                        v.trim().to_ascii_lowercase().as_str(),
                        "" | "false" | "no" | "off" | "0"
                    )
                }),
                _ => value.is_some_and(pushes_checkpoints),
            };
            if publishes {
                remotes.insert(name.to_string());
            }
        }
        remotes.into_iter().collect()
    }

    /// Whether the repository has every one of these commits.
    pub(crate) async fn have_commits(&self, commits: &[&str], deadline: Instant) -> bool {
        let mut input = Vec::new();
        for c in commits {
            input.extend_from_slice(c.as_bytes());
            input.push(b'\n');
        }
        let git = Git {
            index: None,
            ..self.git()
        };
        match git
            .run(&["cat-file", "--batch-check"], Some(input), deadline)
            .await
        {
            Ok(out) => {
                let out = String::from_utf8_lossy(&out);
                let lines: Vec<&str> = out.lines().collect();
                lines.len() == commits.len() && lines.iter().all(|l| l.contains(" commit "))
            }
            Err(_) => false,
        }
    }

    /// The changes from tree-ish `a` to tree-ish `b`.
    pub(crate) async fn diff(
        &self,
        a: &str,
        b: &str,
        deadline: Instant,
    ) -> Result<Vec<Change>, CheckpointError> {
        Ok(self.git().diff(a, b, deadline).await?)
    }

    /// Write `paths` (root-relative) with their content and mode in `commit`, through the
    /// temporary index (`git checkout <commit> -- <paths>`).
    pub(crate) async fn checkout(
        &self,
        commit: &str,
        paths: &[PathBuf],
        deadline: Instant,
    ) -> Result<(), CheckpointError> {
        use std::os::unix::ffi::OsStrExt;
        if paths.is_empty() {
            return Ok(());
        }
        let mut spec = Vec::new();
        for p in paths {
            spec.extend_from_slice(p.as_os_str().as_bytes());
            spec.push(0);
        }
        let file = self.tmp.path().join("pathspec");
        std::fs::write(&file, spec).map_err(|e| CheckpointError::Git(e.to_string()))?;
        let from = format!("--pathspec-from-file={}", file.to_string_lossy());
        let result = self
            .git()
            .run(
                &["checkout", commit, &from, "--pathspec-file-nul"],
                None,
                deadline,
            )
            .await;
        let _ = std::fs::remove_file(&file);
        result?;
        Ok(())
    }

    /// Restore the work tree to checkpoint `commit`: files changed or deleted since come
    /// back with its content, and files added since (untracked and not ignored) are
    /// deleted through the sandbox. Ignored files and submodules are never touched. With
    /// `dry_run` nothing is written; either way the plan is returned. Take a
    /// [`backup`](Self::backup) first.
    ///
    /// Crate-internal: it has no conflict check (a file a human changed or created since
    /// is overwritten or deleted). Rewind and switch (spec 7.12), which check conflicts
    /// themselves, use it to put a backup back when they fail half way.
    pub(crate) async fn restore(
        &self,
        commit: &str,
        dry_run: bool,
    ) -> Result<RestorePlan, CheckpointError> {
        let deadline = Self::housekeeping_deadline();
        let now = self.tree_now(true, deadline).await?;
        let changes = self.git().diff(&now.tree, commit, deadline).await?;
        let mut plan = RestorePlan::default();
        let mut write = Vec::new();
        for c in changes.into_iter().filter(|c| !c.is_gitlink()) {
            match c.new {
                Some(_) => {
                    plan.restored.push(self.root.join(&c.path));
                    write.push(c.path);
                }
                None => plan.deleted.push(self.root.join(&c.path)),
            }
        }
        if dry_run {
            return Ok(plan);
        }
        for p in &plan.deleted {
            remove(self.sb.as_ref(), p).await?;
        }
        self.checkout(commit, &write, deadline).await?;
        Ok(plan)
    }

    /// Which of `paths` (root-relative) the ignore rules of checkpoint `tree` ignore: its
    /// `.gitignore` files, with the repository's `info/exclude` and the user's excludes
    /// file as they were when it was taken (kept in the excludes store). The rules are
    /// written to a scratch directory that stands in for the work tree (`check-ignore
    /// --no-index`), so the work tree is not touched: the kept exclude rules go first in
    /// its top `.gitignore` (so they take precedence over the exclude files as they are
    /// now, which can only add to what is ignored, and the tree's own top rules win over
    /// them, as in git). A checkpoint that kept none (too long, or not a checkpoint) is
    /// checked with the exclude files as they are now.
    pub(crate) async fn ignored_under(
        &self,
        tree: &str,
        paths: &[PathBuf],
        deadline: Instant,
    ) -> Result<std::collections::HashSet<PathBuf>, CheckpointError> {
        use std::os::unix::ffi::OsStrExt;
        let mut ignored = std::collections::HashSet::new();
        let paths: Vec<&PathBuf> = paths.iter().filter(|p| plain_relative(p)).collect();
        if paths.is_empty() {
            return Ok(ignored);
        }
        let disk = |e: std::io::Error| CheckpointError::Git(format!("ignore rules: {e}"));
        let scratch = tempfile::Builder::new()
            .prefix("rules-")
            .tempdir_in(self.tmp.path())
            .map_err(disk)?;
        let rules = scratch.path().canonicalize().map_err(disk)?;
        let git = Git {
            index: None,
            ..self.git()
        };
        let kept = self.excludes.as_ref().and_then(|store| store.get(tree));
        if let Some(rules_then) = &kept {
            std::fs::write(rules.join(".gitignore"), rules_then).map_err(disk)?;
        }
        let listing = git
            .run(&["ls-tree", "-r", "-z", tree], None, deadline)
            .await?;
        for entry in listing.split(|&b| b == 0).filter(|e| !e.is_empty()) {
            let Some(tab) = entry.iter().position(|&b| b == b'\t') else {
                continue;
            };
            let meta = String::from_utf8_lossy(&entry[..tab]);
            let path = PathBuf::from(bytes_to_os(&entry[tab + 1..]));
            let [mode, "blob", id] = meta.split(' ').collect::<Vec<_>>()[..] else {
                continue;
            };
            let is_rules = path.file_name().is_some_and(|n| n == ".gitignore");
            if !is_rules || !matches!(mode, "100644" | "100755") || !plain_relative(&path) {
                continue;
            }
            let mut text = git.run(&["cat-file", "blob", id], None, deadline).await?;
            if let (Some(rules_then), true) = (&kept, path == Path::new(".gitignore")) {
                let mut both = rules_then.clone();
                both.push(b'\n');
                both.append(&mut text);
                text = both;
            }
            let dest = rules.join(&path);
            if let Some(dir) = dest.parent() {
                std::fs::create_dir_all(dir).map_err(disk)?;
            }
            std::fs::write(&dest, text).map_err(disk)?;
        }
        // Directory-only patterns (`target/`) match through the leading directories.
        let mut input = Vec::new();
        for p in &paths {
            if let Some(dir) = p.parent() {
                std::fs::create_dir_all(rules.join(dir)).map_err(disk)?;
            }
            input.extend_from_slice(rules.join(p).as_os_str().as_bytes());
            input.push(0);
        }
        let check = Git {
            work_tree: &rules,
            ..git
        };
        let out = match check
            .run(
                &["check-ignore", "--no-index", "-z", "--stdin"],
                Some(input),
                deadline,
            )
            .await
        {
            Ok(out) => out,
            // None is ignored.
            Err(Fail::Exit { code: Some(1), .. }) => Vec::new(),
            Err(e) => return Err(e.into()),
        };
        for p in out.split(|&b| b == 0).filter(|p| !p.is_empty()) {
            if let Ok(rel) = Path::new(&bytes_to_os(p)).strip_prefix(&rules) {
                ignored.insert(rel.to_path_buf());
            }
        }
        Ok(ignored)
    }

    /// The blob id git gives `bytes` (no filter runs; nothing is written).
    pub(crate) async fn blob_id(
        &self,
        bytes: Vec<u8>,
        deadline: Instant,
    ) -> Result<String, CheckpointError> {
        let git = Git {
            index: None,
            ..self.git()
        };
        let out = git
            .run(
                &["hash-object", "--no-filters", "--stdin"],
                Some(bytes),
                deadline,
            )
            .await?;
        Ok(String::from_utf8_lossy(&out).trim().to_string())
    }

    /// The blob id of the regular file at `path` (absolute, inside the root) as it is, read
    /// by git without filters: for a file too large to hold in memory.
    pub(crate) async fn blob_id_of_file(
        &self,
        path: &Path,
        deadline: Instant,
    ) -> Result<String, CheckpointError> {
        let git = Git {
            index: None,
            ..self.git()
        };
        let path = path.to_string_lossy();
        let out = git
            .run(
                &["hash-object", "--no-filters", "--", &path],
                None,
                deadline,
            )
            .await?;
        Ok(String::from_utf8_lossy(&out).trim().to_string())
    }

    /// The tree entries `(mode, object id)` of `paths` (root-relative) in tree-ish
    /// `tree`; a path it does not have (or has as a directory) is left out.
    pub(crate) async fn entries(
        &self,
        tree: &str,
        paths: &[PathBuf],
        deadline: Instant,
    ) -> Result<std::collections::HashMap<PathBuf, (String, String)>, CheckpointError> {
        let mut found = std::collections::HashMap::new();
        let paths: Vec<&PathBuf> = paths.iter().filter(|p| plain_relative(p)).collect();
        if paths.is_empty() {
            return Ok(found);
        }
        let git = Git {
            index: None,
            ..self.git()
        };
        // Literal pathspecs (`GIT_LITERAL_PATHSPECS`), in bounded batches.
        for chunk in paths.chunks(512) {
            let names: Vec<String> = chunk
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect();
            let mut args = vec!["ls-tree", "-z", "--full-tree", tree, "--"];
            args.extend(names.iter().map(String::as_str));
            let out = git.run(&args, None, deadline).await?;
            for entry in out.split(|&b| b == 0).filter(|e| !e.is_empty()) {
                let Some(tab) = entry.iter().position(|&b| b == b'\t') else {
                    continue;
                };
                let meta = String::from_utf8_lossy(&entry[..tab]);
                let [mode, kind, id] = meta.split(' ').collect::<Vec<_>>()[..] else {
                    continue;
                };
                if kind != "blob" {
                    continue;
                }
                let path = PathBuf::from(bytes_to_os(&entry[tab + 1..]));
                found.insert(path, (mode.to_string(), id.to_string()));
            }
        }
        Ok(found)
    }

    /// A commit (with no ref) of tree-ish `base` with `overlay` applied: each path
    /// (root-relative) set to its content (none: removed). A path keeps its mode in `base`,
    /// else takes the mode of the file now at it in the work tree (regular files only),
    /// else `100644`. Built in an index of its own, so the session's snapshot index keeps
    /// its stat data. Returns the commit and its tree, HEAD and whether the tree differs
    /// from HEAD's.
    pub(crate) async fn compose(
        &self,
        base: &str,
        overlay: &[(PathBuf, Option<Vec<u8>>)],
        deadline: Instant,
    ) -> Result<CheckpointRef, CheckpointError> {
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::PermissionsExt;
        let index = self.tmp.path().join("compose-index");
        let _ = std::fs::remove_file(&index);
        let git = Git {
            index: Some(&index),
            ..self.git()
        };
        let plain = Git {
            index: None,
            ..self.git()
        };
        let made = async {
            git.run(&["read-tree", base], None, deadline).await?;
            let present: Vec<PathBuf> = overlay
                .iter()
                .filter(|(_, c)| c.is_some())
                .map(|(p, _)| p.clone())
                .collect();
            let modes = self.entries(base, &present, deadline).await?;
            let mut info = Vec::new();
            for (path, content) in overlay {
                if !plain_relative(path) {
                    return Err(CheckpointError::Invalid(format!(
                        "{} cannot be put in a tree",
                        path.display()
                    )));
                }
                let line = match content {
                    Some(bytes) => {
                        let id = plain
                            .run(
                                &["hash-object", "-w", "--no-filters", "--stdin"],
                                Some(bytes.clone()),
                                deadline,
                            )
                            .await?;
                        let id = String::from_utf8_lossy(&id).trim().to_string();
                        let mode = match modes.get(path) {
                            Some((mode, _)) => mode.clone(),
                            None => match std::fs::symlink_metadata(self.root.join(path)) {
                                Ok(m) if m.is_file() && m.permissions().mode() & 0o100 != 0 => {
                                    "100755".to_string()
                                }
                                _ => "100644".to_string(),
                            },
                        };
                        format!("{mode} {id}\t")
                    }
                    None => format!("0 {}\t", "0".repeat(base.len())),
                };
                info.extend_from_slice(line.as_bytes());
                info.extend_from_slice(path.as_os_str().as_bytes());
                info.push(0);
            }
            if !info.is_empty() {
                git.run(
                    &["update-index", "-z", "--index-info"],
                    Some(info),
                    deadline,
                )
                .await?;
            }
            let tree = git.text(&["write-tree"], deadline).await?;
            let message = format!("tautly checkpoint ({}, restored)\n", self.session);
            let out = git
                .run(
                    &["commit-tree", "--no-gpg-sign", &tree, "-p", base],
                    Some(message.into_bytes()),
                    deadline,
                )
                .await?;
            let commit = String::from_utf8_lossy(&out).trim().to_string();
            let head = git.head(deadline).await?;
            let now = TreeNow {
                tree,
                head,
                excludes: None,
            };
            Ok(CheckpointRef {
                n: 0,
                commit,
                refname: String::new(),
                head: now.head.as_ref().map(|(c, _)| c.clone()),
                dirty: now.dirty(),
                tree: now.tree,
                reused: false,
            })
        }
        .await;
        let _ = std::fs::remove_file(&index);
        made
    }

    /// Give `child_session` refs of its own (`refs/tautly/checkpoints/<child>/<n>`) for
    /// these checkpoints (`(n, commit)`) its log copied from this session's, so deleting
    /// or pruning the parent never breaks the branch.
    pub async fn adopt(
        &self,
        child_session: &str,
        commits: &[(u32, String)],
    ) -> Result<(), CheckpointError> {
        check_session(child_session)?;
        // A reused checkpoint is logged again with its number: one ref each.
        let unique: std::collections::BTreeMap<u32, &String> =
            commits.iter().map(|(n, c)| (*n, c)).collect();
        let mut commands = Vec::new();
        for (n, commit) in unique {
            let refname = ref_name(child_session, &n.to_string());
            commands.extend(format!("update {refname}\0{commit}\0\0").into_bytes());
        }
        let git = Git {
            index: None,
            ..self.git()
        };
        Ok(git
            .update_refs(commands, Self::housekeeping_deadline())
            .await?)
    }
}

/// Delete `path` through the sandbox (gone already is fine).
pub(crate) async fn remove(sb: &dyn Sandbox, path: &Path) -> Result<(), CheckpointError> {
    match sb.remove_file(path).await {
        Ok(()) => Ok(()),
        Err(SandboxError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(CheckpointError::Disk {
            path: path.display().to_string(),
            source,
        }),
    }
}

/// A session id usable in a ref name and as a ref prefix: no `/`, `.`, wildcard or
/// control characters.
pub(crate) fn check_session(session: &str) -> Result<(), CheckpointError> {
    let ok = !session.is_empty()
        && session.len() <= 64
        && session
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if ok {
        Ok(())
    } else {
        Err(CheckpointError::Invalid(format!(
            "not a session id: {session:?}"
        )))
    }
}

/// A path of plain names only (no root, `.` or `..`).
fn plain_relative(p: &Path) -> bool {
    p.components()
        .all(|c| matches!(c, std::path::Component::Normal(_)))
}

/// The first executable `git` on `PATH` that the work tree at `root` cannot have put
/// there ([`tautly_sandbox::which_outside`], the rule every program tautly starts by a
/// bare name follows): absolute entries outside the root only. Returned as found, links
/// not resolved, so a shim that dispatches on its name still sees `git`.
///
/// When `root` is a linked worktree (a fork's, a bisect's), entries inside the main work
/// tree are skipped as well, by the same rule (CP5 review I2).
pub(crate) fn git_on_path(root: &Path) -> Option<PathBuf> {
    tautly_sandbox::which_outside("git", &std::env::var_os("PATH")?, root)
}

/// Whether the root's `.git` is a symbolic link. Checkpoints are then not taken: they
/// would write objects and refs to whatever repository it points at.
pub(crate) fn dot_git_is_link(root: &Path) -> bool {
    std::fs::symlink_metadata(root.join(".git")).is_ok_and(|m| m.file_type().is_symlink())
}

/// The repository of the work tree at `root`: its `.git` directory, or the directory a
/// `.git` file (`gitdir: <path>`, a linked worktree or submodule) names, only when git's
/// own links confirm it belongs to this work tree ([`tautly_session::git_dirs_at`]; a
/// `.git` directory with a `commondir` never does). None otherwise: a checkout that
/// arrived with its `.git` must not get checkpoints written to (or pruned from) another
/// repository; [`dot_git_unverified`] tells that case apart. A `.git` symbolic link is
/// never followed (see [`dot_git_is_link`]). Every git call names it (`--git-dir`) with
/// the root as the work tree, so neither the environment nor `core.worktree` can point git
/// elsewhere.
///
/// Nor when the common directory's `refs`, `refs/tautly`, `refs/tautly/checkpoints`, a
/// session directory below it, `packed-refs` or `objects` is a symbolic link: checkpoints
/// would write (and prune, or delete with a session) refs and objects in whatever
/// repository it leads to; git rewrites `packed-refs` through a link when it deletes a
/// ref. (`objects/info/alternates` is left alone: reading another repository's objects is
/// how `clone --shared` works, and nothing is written there.)
pub(crate) fn git_dir(root: &Path) -> Option<PathBuf> {
    let dirs = tautly_session::git_dirs_at(&root.join(".git"))?;
    let is_link = |p: &Path| std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink());
    let sessions = dirs.common_dir.join(REF_PREFIX);
    let linked = ["refs", "refs/tautly", REF_PREFIX, "packed-refs", "objects"]
        .iter()
        .any(|p| is_link(&dirs.common_dir.join(p)))
        || std::fs::read_dir(&sessions).is_ok_and(|entries| {
            entries
                .filter_map(Result::ok)
                .any(|e| e.file_type().is_ok_and(|t| t.is_symlink()))
        });
    (!linked).then_some(dirs.git_dir)
}

/// Whether the root has a `.git` (not a symbolic link) that [`git_dir`] does not accept.
pub(crate) fn dot_git_unverified(root: &Path) -> bool {
    std::fs::symlink_metadata(root.join(".git")).is_ok_and(|m| !m.file_type().is_symlink())
        && git_dir(root).is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_diff_entries_parse() {
        let out = b":100644 100755 aaaa bbbb M\0dir/a b.txt\0:000000 100644 0000 cccc A\0new\0:160000 000000 dddd 0000 D\0sub\0";
        let changes = parse_raw_diff(out).unwrap();
        assert_eq!(changes.len(), 3);
        assert_eq!(changes[0].path, PathBuf::from("dir/a b.txt"));
        assert_eq!(changes[0].old, Some(("100644".into(), "aaaa".into())));
        assert_eq!(changes[1].old, None);
        assert!(changes[2].is_gitlink() && changes[2].new.is_none());
    }

    #[test]
    fn git_dir_needs_a_dot_git_git_names_back() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(git_dir(dir.path()), None);
        assert!(!dot_git_unverified(dir.path()));
        // Pointers nothing names back (see `tautly_session::git_dirs_at`).
        for pointer in [
            "gitdir: ../main/.git/worktrees/w\n",
            "gitdir: /abs/repo.git\n",
            "not a pointer\n",
        ] {
            std::fs::write(dir.path().join(".git"), pointer).unwrap();
            assert_eq!(git_dir(dir.path()), None, "{pointer}");
            assert!(dot_git_unverified(dir.path()), "{pointer}");
        }
        std::fs::remove_file(dir.path().join(".git")).unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        let real = dir.path().canonicalize().unwrap().join(".git");
        assert_eq!(git_dir(dir.path()), Some(real));
        std::fs::write(dir.path().join(".git/commondir"), "/elsewhere/.git\n").unwrap();
        assert_eq!(git_dir(dir.path()), None);
    }

    fn git(root: &Path, args: &[&str]) {
        let ok = std::process::Command::new("git")
            .current_dir(root)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args(["-c", "user.name=T", "-c", "user.email=t@example.com"])
            .args(args)
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?}");
    }

    #[tokio::test]
    async fn restore_brings_back_a_checkpoint_without_running_filters() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(root.join("a.txt"), "a\n").unwrap();
        git(&root, &["init", "-q"]);
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-q", "-m", "i"]);
        let marker = root.parent().unwrap().join(format!(
            "{}-marker",
            root.file_name().unwrap().to_string_lossy()
        ));
        let touch = format!("touch '{}'; cat", marker.display());
        git(&root, &["config", "filter.f.clean", &touch]);
        git(&root, &["config", "filter.f.smudge", &touch]);
        std::fs::write(root.join(".gitattributes"), "* filter=f\n").unwrap();
        let sb: Arc<dyn Sandbox> =
            Arc::new(tautly_sandbox::LocalSandbox::new(root.clone()).unwrap());
        let mut cp =
            Checkpointer::new(sb, root.clone(), "s".into(), CheckpointConfig::default()).unwrap();
        let first = cp.snapshot_now().await.unwrap();
        std::fs::write(root.join("a.txt"), "changed\n").unwrap();
        std::fs::write(root.join("b.txt"), "new\n").unwrap();
        let dry = cp.restore(&first.commit, true).await.unwrap();
        assert_eq!(dry.restored, [root.join("a.txt")]);
        assert_eq!(dry.deleted, [root.join("b.txt")]);
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "changed\n"
        );
        let plan = cp.restore(&first.commit, false).await.unwrap();
        assert_eq!(plan, dry);
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "a\n");
        assert!(!root.join("b.txt").exists());
        assert!(!marker.exists(), "a filter ran");
    }

    #[test]
    fn config_read_switches_off_filters_and_protocols() {
        let out = b"filter.lfs.clean\ngit-lfs clean\0filter.a.b.smudge\nx\0\
protocol.ext.allow\nalways\0protocol.my.proto.allow\nuser\0\
remote.origin.promisor\nfalse\0hook.x.command\ntouch m\0hook.a.b.event\npost-checkout\0\
hook.jobs\n4\0";
        let config = parse_config(out).unwrap();
        let settings: Vec<&str> = config
            .overrides
            .iter()
            .filter(|a| *a != "-c")
            .map(String::as_str)
            .collect();
        assert!(settings.contains(&"filter.lfs.process="), "{settings:?}");
        assert!(settings.contains(&"filter.a.b.smudge="), "{settings:?}");
        assert!(
            settings.contains(&"protocol.ext.allow=never"),
            "{settings:?}"
        );
        assert!(
            settings.contains(&"protocol.my.proto.allow=never"),
            "{settings:?}"
        );
        for s in [
            "hook.x.enabled=false",
            "hook.x.event=",
            "hook.a.b.enabled=false",
            "hook.a.b.event=",
        ] {
            assert!(settings.contains(&s), "{settings:?}");
        }
        assert!(!settings.iter().any(|s| s.starts_with("hook.jobs")));
        assert!(!config.partial_clone);
        // A promisor remote (a key without a value is true) or the extension.
        for out in [
            &b"remote.origin.promisor\0"[..],
            b"remote.o.promisor\nyes\0",
            b"extensions.partialclone\norigin\0",
            b"remote.origin.partialclonefilter\nblob:none\0",
        ] {
            assert!(parse_config(out).unwrap().partial_clone);
        }
        assert!(parse_config(b"protocol.a=b.allow\nalways\0").is_err());
        assert!(parse_config(b"hook.a=b.command\nx\0").is_err());
    }

    #[test]
    fn git_versions_parse() {
        assert_eq!(
            parse_version("git version 2.54.0 (Apple Git-157)\n"),
            Some((2, 54))
        );
        assert_eq!(parse_version("git version 2.43.0.windows.1"), Some((2, 43)));
        assert_eq!(parse_version("git version 3.0"), Some((3, 0)));
        assert_eq!(parse_version("something else"), None);
        // A release candidate comes before its release.
        assert!(parse_version("git version 2.44.0.rc0").unwrap() < (2, 44));
        assert!(parse_version("git version 2.44.0-rc2").unwrap() < (2, 44));
        assert!(parse_version("git version 3.0.0.rc1").unwrap() < (3, 0));
        assert_eq!(parse_version("git version 2.44.1.windows.1"), Some((2, 44)));
    }

    #[test]
    fn push_refspecs_that_can_publish_checkpoints() {
        for spec in [
            "+refs/*:refs/*",
            "refs/*",
            "refs/tautly/*:refs/tautly/*",
            "+refs/tautly/checkpoints/s/1:refs/x",
            "refs/t*:refs/t*",
            "tautly/checkpoints/s/1",
            "*:*",
        ] {
            assert!(pushes_checkpoints(spec), "{spec:?}");
        }
        for spec in [
            "refs/heads/main:refs/heads/main",
            "+refs/heads/*:refs/heads/*",
            ":",
            "^refs/tautly/*",
            "refs/tags/*",
            "HEAD",
            "",
        ] {
            assert!(!pushes_checkpoints(spec), "{spec:?}");
        }
    }

    #[test]
    fn session_ids_are_checked_before_they_reach_a_ref() {
        assert!(check_session("01K9ABCDEF").is_ok());
        for bad in ["", "a/b", "..", "a*", "x\n", "refs/heads"] {
            assert!(check_session(bad).is_err(), "{bad:?}");
        }
    }
}
