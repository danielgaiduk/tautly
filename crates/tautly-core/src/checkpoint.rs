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
//! 3. `add -A`, `write-tree`, `commit-tree -p <previous checkpoint or HEAD>`, `update-ref`.
//!
//! The user's index, HEAD, branches, stash and ignored files are never read or written:
//! only objects and `refs/tautly/` change, except on an explicit restore, which writes the
//! work tree. Every git call runs with `core.fsmonitor=false` and `core.hooksPath=/dev/null`
//! (no hook or fsmonitor program runs), `gc.auto=0`, no signing, literal pathspecs, the
//! repository found at the root only (`GIT_CEILING_DIRECTORIES`), in a process group of
//! its own that is killed at the deadline (`timeout_ms` for a snapshot), when the call is
//! dropped (a cancel) or by the session's process-group registry.
//!
//! An unchanged tree reuses the previous checkpoint (same `n` and commit), and so does a
//! [`Checkpointer::snapshot`] whose write generation did not move and whose caller's
//! watcher saw no change, without running git at all.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tautly_sandbox::{Sandbox, SandboxError, SpawnSpec};
use tautly_tools::ProcessGroups;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::config::CheckpointConfig;
use crate::record::SessionRecord;
use crate::session::Session;

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
}

/// What a restore changes (with `dry_run`: would change), as absolute paths.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RestorePlan {
    /// Files written with the checkpoint's content (changed or deleted since).
    pub restored: Vec<PathBuf>,
    /// Files deleted (added since; untracked and not ignored).
    pub deleted: Vec<PathBuf>,
}

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
            Skip::Timeout => CheckpointError::Timeout,
            Skip::Git(why) => CheckpointError::Git(why),
            other => CheckpointError::Git(other.to_string()),
        }
    }
}

/// The warning shown for a skipped checkpoint.
pub fn skip_warning(skip: &Skip) -> String {
    format!("checkpoint skipped: {skip}")
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
    /// It ran and exited non-zero (`code`; none: killed by a signal).
    Exit {
        code: Option<i32>,
        stderr: String,
    },
    Io(String),
}

impl From<Fail> for Skip {
    fn from(f: Fail) -> Self {
        match f {
            Fail::NoGit => Skip::NoGit,
            Fail::Timeout => Skip::Timeout,
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

/// One git invocation context: which git, where, with which index.
struct Git<'a> {
    sb: &'a dyn Sandbox,
    root: &'a Path,
    program: &'a Path,
    index: Option<&'a Path>,
    groups: Option<&'a ProcessGroups>,
}

impl Git<'_> {
    /// Run `git <args>` (with `input` on stdin) until `deadline`; its stdout on success.
    async fn run(
        &self,
        args: &[&str],
        input: Option<Vec<u8>>,
        deadline: Instant,
    ) -> Result<Vec<u8>, Fail> {
        let mut argv = vec![self.program.to_string_lossy().into_owned()];
        for setting in [
            "core.fsmonitor=false",
            "core.hooksPath=/dev/null",
            "gc.auto=0",
            "maintenance.auto=false",
            "commit.gpgSign=false",
            "advice.addEmbeddedRepo=false",
            "core.quotePath=false",
        ] {
            argv.push("-c".into());
            argv.push(setting.into());
        }
        argv.extend(args.iter().map(|a| a.to_string()));
        let mut env = vec![
            ("GIT_TERMINAL_PROMPT".into(), "0".into()),
            ("GIT_OPTIONAL_LOCKS".into(), "0".into()),
            ("GIT_LITERAL_PATHSPECS".into(), "1".into()),
            ("GIT_AUTHOR_NAME".into(), "tautly".into()),
            ("GIT_AUTHOR_EMAIL".into(), "tautly@localhost".into()),
            ("GIT_COMMITTER_NAME".into(), "tautly".into()),
            ("GIT_COMMITTER_EMAIL".into(), "tautly@localhost".into()),
        ];
        if let Some(parent) = self.root.parent() {
            env.push((
                "GIT_CEILING_DIRECTORIES".into(),
                parent.to_string_lossy().into_owned(),
            ));
        }
        let mut env_remove: Vec<String> = [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_COMMON_DIR",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_NAMESPACE",
            "GIT_PREFIX",
            "GIT_GLOB_PATHSPECS",
            "GIT_NOGLOB_PATHSPECS",
            "GIT_ICASE_PATHSPECS",
        ]
        .map(String::from)
        .to_vec();
        match self.index {
            Some(index) => env.push((
                "GIT_INDEX_FILE".into(),
                index.to_string_lossy().into_owned(),
            )),
            None => env_remove.push("GIT_INDEX_FILE".into()),
        }
        let spec = SpawnSpec {
            argv,
            cwd: self.root.to_path_buf(),
            env,
            env_remove,
            new_process_group: true,
        };
        let spawned = match &input {
            Some(_) => self.sb.spawn_stdio(spec).await,
            None => self.sb.spawn(spec).await,
        };
        let mut child = match spawned {
            Ok(c) => c,
            Err(SandboxError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(Fail::NoGit);
            }
            Err(e) => return Err(Fail::Io(e.to_string())),
        };
        // Declared after `child`, so dropped (killing the group) before it.
        let group = child.id().map(|pgid| Group::new(self.groups, pgid));
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let work = async {
            let feed = async move {
                if let (Some(mut w), Some(bytes)) = (stdin, input) {
                    let _ = w.write_all(&bytes).await;
                    let _ = w.shutdown().await;
                }
            };
            let (_, out, err, status) =
                tokio::join!(feed, read_all(stdout), read_all(stderr), child.wait());
            (out, err, status)
        };
        let ended = tokio::time::timeout_at(deadline, work).await;
        let Ok((out, err, status)) = ended else {
            // The leader is not reaped yet (or a member holds the pipes open): the group
            // is still ours to kill.
            if let Some(g) = group {
                g.kill();
            }
            let _ = child.wait().await;
            return Err(Fail::Timeout);
        };
        if let Some(g) = group {
            g.done();
        }
        let status = status.map_err(|e| Fail::Io(e.to_string()))?;
        let out = out.map_err(Fail::Io)?;
        if status.success() {
            Ok(out)
        } else {
            let err = err.unwrap_or_default();
            Err(Fail::Exit {
                code: status.code(),
                stderr: cut(String::from_utf8_lossy(&err).trim()),
            })
        }
    }

    /// stdout of `git <args>` as trimmed text.
    async fn text(&self, args: &[&str], deadline: Instant) -> Result<String, Fail> {
        let out = self.run(args, None, deadline).await?;
        Ok(String::from_utf8_lossy(&out).trim().to_string())
    }

    /// `(HEAD commit, its tree)`; none when HEAD is unborn.
    async fn head(&self, deadline: Instant) -> Result<Option<(String, String)>, Fail> {
        match self
            .text(&["rev-parse", "HEAD^{commit}", "HEAD^{tree}"], deadline)
            .await
        {
            Ok(text) => {
                let mut lines = text.lines();
                match (lines.next(), lines.next()) {
                    (Some(c), Some(t)) => Ok(Some((c.to_string(), t.to_string()))),
                    _ => Err(Fail::Io(format!("unexpected rev-parse output: {text}"))),
                }
            }
            Err(Fail::Exit { .. }) => {
                // Unborn (exit 1, nothing printed), or a real failure (reported).
                match self
                    .run(&["rev-parse", "-q", "--verify", "HEAD"], None, deadline)
                    .await
                {
                    Err(Fail::Exit { code: Some(1), .. }) => Ok(None),
                    Ok(_) => Err(Fail::Io("HEAD is not a commit".into())),
                    Err(e) => Err(e),
                }
            }
            Err(e) => Err(e),
        }
    }

    /// `diff-tree` from `a` to `b`, recursively, without rename detection.
    async fn diff(&self, a: &str, b: &str, deadline: Instant) -> Result<Vec<Change>, Fail> {
        let out = self
            .run(
                &["diff-tree", "-r", "-z", "--no-renames", "--raw", a, b],
                None,
                deadline,
            )
            .await?;
        parse_raw_diff(&out)
    }

    /// Run `update-ref --stdin -z` with these commands (NUL-terminated fields).
    async fn update_refs(&self, commands: Vec<u8>, deadline: Instant) -> Result<(), Fail> {
        if commands.is_empty() {
            return Ok(());
        }
        self.run(&["update-ref", "--stdin", "-z"], Some(commands), deadline)
            .await
            .map(drop)
    }

    /// Every ref below `prefix`, with its commit's committer date (unix seconds).
    async fn refs(&self, prefix: &str, deadline: Instant) -> Result<Vec<(String, u64)>, Fail> {
        let out = self
            .run(
                &[
                    "for-each-ref",
                    "--format=%(refname)%00%(committerdate:unix)",
                    prefix,
                ],
                None,
                deadline,
            )
            .await?;
        Ok(String::from_utf8_lossy(&out)
            .lines()
            .filter_map(|l| {
                let (name, date) = l.split_once('\0')?;
                Some((name.to_string(), date.trim().parse().unwrap_or(0)))
            })
            .collect())
    }
}

/// One entry of a tree diff: the path, and each side's `(mode, object id)` (none: absent).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Change {
    pub path: PathBuf,
    pub old: Option<(String, String)>,
    pub new: Option<(String, String)>,
}

impl Change {
    /// A submodule (gitlink) on either side: never restored.
    pub fn is_gitlink(&self) -> bool {
        [&self.old, &self.new]
            .into_iter()
            .flatten()
            .any(|(mode, _)| mode == GITLINK)
    }
}

/// Parse `diff-tree --raw -z` output: `:<old mode> <new mode> <old id> <new id> <status>`
/// NUL `<path>` NUL, per entry.
fn parse_raw_diff(out: &[u8]) -> Result<Vec<Change>, Fail> {
    let mut fields = out.split(|&b| b == 0).filter(|f| !f.is_empty());
    let mut changes = Vec::new();
    while let Some(meta) = fields.next() {
        let meta = String::from_utf8_lossy(meta);
        let Some(path) = fields.next() else {
            return Err(Fail::Io("truncated diff-tree output".into()));
        };
        let parts: Vec<&str> = meta.trim_start_matches(':').split(' ').collect();
        let [old_mode, new_mode, old_id, new_id, _status] = parts[..] else {
            return Err(Fail::Io(format!("unexpected diff-tree entry: {meta}")));
        };
        let side =
            |mode: &str, id: &str| (mode != "000000").then(|| (mode.to_string(), id.to_string()));
        changes.push(Change {
            path: PathBuf::from(bytes_to_os(path)),
            old: side(old_mode, old_id),
            new: side(new_mode, new_id),
        });
    }
    Ok(changes)
}

fn bytes_to_os(b: &[u8]) -> OsString {
    use std::os::unix::ffi::OsStringExt;
    OsString::from_vec(b.to_vec())
}

/// `s`, cut to [`MAX_ERROR`] bytes on a character boundary.
fn cut(s: &str) -> String {
    if s.len() <= MAX_ERROR {
        return s.to_string();
    }
    let mut end = MAX_ERROR;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

async fn read_all(r: Option<impl AsyncRead + Unpin>) -> Result<Vec<u8>, String> {
    let Some(r) = r else {
        return Ok(Vec::new());
    };
    let mut buf = Vec::new();
    r.take(MAX_OUTPUT as u64 + 1)
        .read_to_end(&mut buf)
        .await
        .map_err(|e| e.to_string())?;
    if buf.len() > MAX_OUTPUT {
        return Err("git printed more than 64 MiB".into());
    }
    Ok(buf)
}

/// A git call's process group, registered with the session's registry while it runs.
/// Dropped before [`Group::done`], it kills the group (the call was abandoned).
struct Group {
    groups: Option<ProcessGroups>,
    pgid: u32,
    armed: bool,
}

impl Group {
    fn new(groups: Option<&ProcessGroups>, pgid: u32) -> Group {
        if let Some(g) = groups {
            g.register(pgid);
        }
        Group {
            groups: groups.cloned(),
            pgid,
            armed: true,
        }
    }

    /// The call ended on its own (the leader is reaped).
    fn done(mut self) {
        self.armed = false;
        if let Some(g) = &self.groups {
            g.release(self.pgid);
        }
    }

    fn kill(self) {
        // Drop does it.
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        if self.armed {
            if let Ok(pgid) = libc::pid_t::try_from(self.pgid) {
                // SAFETY: killpg has no memory-safety preconditions; failure (the group is
                // gone) is harmless.
                unsafe {
                    libc::killpg(pgid, libc::SIGKILL);
                }
            }
            if let Some(g) = &self.groups {
                g.release(self.pgid);
            }
        }
    }
}

/// The work tree as a tree object, and HEAD then.
struct TreeNow {
    tree: String,
    head: Option<(String, String)>,
}

impl TreeNow {
    fn dirty(&self) -> bool {
        match &self.head {
            Some((_, head_tree)) => *head_tree != self.tree,
            None => !EMPTY_TREES.contains(&self.tree.as_str()),
        }
    }
}

/// The latest checkpoint, and the write generation it was taken at (none: unknown).
struct Last {
    r: CheckpointRef,
    write_gen: Option<u64>,
}

/// A session's git checkpoints (see the module docs).
pub struct Checkpointer {
    sb: Arc<dyn Sandbox>,
    root: PathBuf,
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
}

impl Checkpointer {
    /// A checkpointer for the work tree at `root` (the sandbox root, a git top level):
    /// none when `root` is not a git work tree (it has no `.git`; a bare repository has
    /// none), when no `git` is on `PATH`, or when no temporary directory can be made.
    /// Runs no git.
    pub fn new(
        sb: Arc<dyn Sandbox>,
        root: PathBuf,
        session: String,
        cfg: CheckpointConfig,
    ) -> Option<Checkpointer> {
        std::fs::symlink_metadata(root.join(".git")).ok()?;
        let program = find_on_path("git")?;
        let tmp = tempfile::Builder::new()
            .prefix("tautly-checkpoint-")
            .tempdir()
            .ok()?;
        let index = tmp.path().join("index");
        Some(Checkpointer {
            sb,
            root,
            session,
            cfg,
            program,
            groups: None,
            tmp,
            index,
            next: 1,
            last: None,
        })
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

    /// New checkpoints go to `session`'s refs from now on (a fork's branch).
    pub fn set_session(&mut self, session: String) {
        self.session = session;
    }

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
            });
        }
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
            sb: self.sb.as_ref(),
            root: &self.root,
            program: &self.program,
            index: Some(&self.index),
            groups: self.groups.as_ref(),
        }
    }

    fn deadline(&self) -> Instant {
        Instant::now() + Duration::from_millis(self.cfg.timeout_ms)
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

    async fn take(&mut self, backup: bool) -> Result<CheckpointRef, Skip> {
        let deadline = self.deadline();
        let now = self.tree_now(true, deadline).await?;
        if !backup
            && let Some(l) = &self.last
            && l.r.tree == now.tree
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
        let message = format!("tautly checkpoint ({})", self.session);
        let mut args = vec!["commit-tree", "--no-gpg-sign", now.tree.as_str()];
        if let Some(p) = &parent {
            args.extend(["-p", p.as_str()]);
        }
        args.extend(["-m", message.as_str()]);
        let commit = git.text(&args, deadline).await?;
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
        let tree = git.text(&["write-tree"], deadline).await?;
        Ok(TreeNow { tree, head })
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
    pub async fn restore(
        &self,
        commit: &str,
        dry_run: bool,
    ) -> Result<RestorePlan, CheckpointError> {
        let deadline = Self::housekeeping_deadline();
        let now = self.tree_now(false, deadline).await?;
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
fn check_session(session: &str) -> Result<(), CheckpointError> {
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

/// The first executable `name` on `PATH`.
fn find_on_path(name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|p| {
            std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
}

/// git from `PATH`, for the free functions.
fn plain_git<'a>(sb: &'a dyn Sandbox, root: &'a Path, program: &'a Path) -> Git<'a> {
    Git {
        sb,
        root,
        program,
        index: None,
        groups: None,
    }
}

/// Delete every checkpoint ref (of any session of the repository at `root`) whose commit
/// is older than `keep_days`; returns how many. Run at engine start; `git gc` is never run,
/// so the objects stay until git's own housekeeping.
pub async fn prune(sb: &dyn Sandbox, root: &Path, keep_days: u32) -> Result<u32, CheckpointError> {
    let program = find_on_path("git").ok_or(CheckpointError::NoGit)?;
    let git = plain_git(sb, root, &program);
    let deadline = Checkpointer::housekeeping_deadline();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let cutoff = now.saturating_sub(u64::from(keep_days) * 24 * 3600);
    let old: Vec<String> = git
        .refs(&format!("{REF_PREFIX}/"), deadline)
        .await?
        .into_iter()
        .filter(|(_, date)| *date < cutoff)
        .map(|(name, _)| name)
        .collect();
    delete_refs(&git, &old, deadline).await
}

/// Delete session `session`'s checkpoint refs in the repository at `root` (`tautly
/// sessions delete`); returns how many. The objects (and the session blobs) are shared
/// and kept.
pub async fn delete_session_refs(
    sb: &dyn Sandbox,
    root: &Path,
    session: &str,
) -> Result<u32, CheckpointError> {
    check_session(session)?;
    let program = find_on_path("git").ok_or(CheckpointError::NoGit)?;
    let git = plain_git(sb, root, &program);
    let deadline = Checkpointer::housekeeping_deadline();
    let names: Vec<String> = git
        .refs(&format!("{REF_PREFIX}/{session}/"), deadline)
        .await?
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    delete_refs(&git, &names, deadline).await
}

async fn delete_refs(
    git: &Git<'_>,
    names: &[String],
    deadline: Instant,
) -> Result<u32, CheckpointError> {
    let mut commands = Vec::new();
    for name in names {
        commands.extend(format!("delete {name}\0\0").into_bytes());
    }
    git.update_refs(commands, deadline).await?;
    Ok(u32::try_from(names.len()).unwrap_or(u32::MAX))
}

// ------------------------------------------------------------------ in the session

/// A session's checkpointer, shared by the turn (before commands, at turn end) and the
/// actor (session start, undo, pruning); one snapshot at a time.
pub(crate) struct GitCheckpoints {
    pub cp: Checkpointer,
    /// The watcher's event count at the last snapshot (none: unknown).
    fs_events: Option<u64>,
    /// The last skip warned about in this user turn: the same reason is not repeated
    /// before every command.
    warned: Option<(u32, String)>,
}

impl GitCheckpoints {
    pub fn shared(cp: Checkpointer) -> Arc<tokio::sync::Mutex<GitCheckpoints>> {
        Arc::new(tokio::sync::Mutex::new(GitCheckpoints {
            cp,
            fs_events: None,
            warned: None,
        }))
    }
}

/// Where in the session a checkpoint is taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Point {
    /// Session start (and resume): always a full snapshot.
    Start,
    /// Before a command (`bash`, a stdio MCP call): always looks at the files, since a
    /// change the watcher has not delivered yet must not be attributed to the command.
    BeforeExec,
    /// At a user turn's end: reused without running git when the write generation did
    /// not move and the watcher saw no change.
    TurnEnd,
}

impl Session {
    /// Take a checkpoint at `point` of provider step `step` and log it (`Checkpoint`, or
    /// `CheckpointSkipped` with a warning). Nothing without a checkpointer. A cancel
    /// abandons the snapshot (its processes are killed), logged as skipped.
    pub(crate) async fn git_checkpoint(
        &self,
        point: Point,
        step: u32,
        cancel: Option<&CancellationToken>,
    ) {
        let Some(shared) = self.checkpoints.clone() else {
            return;
        };
        let turn = self.user_turn;
        let write_gen = self.ledger.generation();
        let watcher = self
            .workspace
            .index()
            .filter(|i| i.watching())
            .map(|i| i.fs_events());
        let mut st = shared.lock().await;
        let watcher_changed = watcher.is_none() || watcher != st.fs_events;
        st.fs_events = watcher;
        let snap = async {
            match point {
                Point::Start => st.cp.snapshot_now().await,
                Point::BeforeExec => st.cp.snapshot(write_gen, true).await,
                Point::TurnEnd => st.cp.snapshot(write_gen, watcher_changed).await,
            }
        };
        let taken = match cancel {
            Some(c) => tokio::select! {
                r = snap => Some(r),
                () = c.cancelled() => None,
            },
            None => Some(snap.await),
        };
        let rec = match taken {
            Some(Ok(r)) => SessionRecord::Checkpoint {
                n: r.n,
                commit: r.commit,
                tree: r.tree,
                head: r.head,
                dirty: r.dirty,
                turn,
                step,
            },
            Some(Err(skip)) => {
                let reason = skip.to_string();
                let fresh = st.warned.as_ref() != Some(&(turn, reason.clone()));
                st.warned = Some((turn, reason.clone()));
                if fresh {
                    self.warn(skip_warning(&skip)).await;
                }
                SessionRecord::CheckpointSkipped { reason, turn, step }
            }
            None => {
                st.cp.invalidate();
                SessionRecord::CheckpointSkipped {
                    reason: "cancelled".into(),
                    turn,
                    step,
                }
            }
        };
        drop(st);
        self.log(rec).await;
    }

    /// The step a checkpoint outside a command belongs to: the last one started (0: none).
    pub(crate) fn last_step(&self) -> u32 {
        self.context.next_turn().saturating_sub(1)
    }
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
    fn session_ids_are_checked_before_they_reach_a_ref() {
        assert!(check_session("01K9ABCDEF").is_ok());
        for bad in ["", "a/b", "..", "a*", "x\n", "refs/heads"] {
            assert!(check_session(bad).is_err(), "{bad:?}");
        }
    }
}
