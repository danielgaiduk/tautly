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

use self::excludes_store::ExcludesStore;
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
            other @ (Skip::TooManyUntracked { .. }
            | Skip::UnsafePartialClone
            | Skip::GitDirSymlink) => CheckpointError::Invalid(other.to_string()),
        }
    }
}

impl Skip {
    /// It holds for the rest of the session: said once, and no checkpoint is tried again.
    pub fn is_permanent(&self) -> bool {
        matches!(self, Skip::UnsafePartialClone | Skip::GitDirSymlink)
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
            Skip::TooManyUntracked { .. } => "too_many_untracked",
            Skip::Git(_) => "git_failed",
            Skip::UnsafePartialClone => "unsafe_partial_clone",
            Skip::GitDirSymlink => "git_dir_symlink",
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

/// Settings every git call runs with: no hook, fsmonitor, submodule recursion, network
/// transport, credential helper or askpass program runs, nothing is written outside the
/// objects and refs (no split index, no automatic gc or maintenance), nothing is signed.
const HARDENING: [&str; 13] = [
    "core.fsmonitor=false",
    "core.hooksPath=/dev/null",
    "core.splitIndex=false",
    "submodule.recurse=false",
    "protocol.allow=never",
    "credential.helper=",
    "core.askPass=",
    "gc.auto=0",
    "maintenance.auto=false",
    "commit.gpgSign=false",
    "advice.addEmbeddedRepo=false",
    "core.quotePath=false",
    "core.pager=cat",
];
/// Inherited variables that would change which repository, configuration or programs a
/// git call uses.
const ENV_REMOVED: [&str; 18] = [
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
    "GIT_CONFIG",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
    "GIT_EXTERNAL_DIFF",
    "GIT_SSH",
    "GIT_SSH_COMMAND",
    "SSH_ASKPASS",
    "GIT_ATTR_SOURCE",
];

/// One git invocation context: which git, which repository and work tree, which index.
struct Git<'a> {
    sb: &'a dyn Sandbox,
    /// The sandbox root: the working directory of every call.
    root: &'a Path,
    /// The repository (`--git-dir`), resolved from the root's `.git`.
    git_dir: &'a Path,
    /// The work tree (`--work-tree`): the root, so `core.worktree` cannot move it.
    work_tree: &'a Path,
    program: &'a Path,
    index: Option<&'a Path>,
    groups: Option<&'a ProcessGroups>,
    /// What the configuration sets that matters here, read once per `Git` (one
    /// operation).
    config: tokio::sync::OnceCell<RepoConfig>,
    /// The git program's version (`(major, minor)`; none: unknown), asked once per
    /// checkpointer (session).
    version: &'a GitVersion,
}

/// The cell a checkpointer keeps its git's version in.
type GitVersion = tokio::sync::OnceCell<Option<(u32, u32)>>;

/// The first git release that honours `GIT_NO_LAZY_FETCH`.
const LAZY_FETCH_OFF_SINCE: (u32, u32) = (2, 44);

/// Most bytes of exclude rules (the user's excludes file and `info/exclude`) kept for a
/// checkpoint; above it, undo falls back to the rules as they are then.
const MAX_EXCLUDES: u64 = 256 * 1024;

/// What one read of the configuration (every scope, includes followed) gives.
#[derive(Debug, Default)]
struct RepoConfig {
    /// `-c` settings that switch off every filter driver and every protocol the
    /// configuration allows.
    overrides: Vec<String>,
    /// `extensions.partialClone`, a promisor remote or a remote's partial-clone filter is
    /// set: missing objects would be fetched (lazily) from a remote.
    partial_clone: bool,
}

impl<'a> Git<'a> {
    fn new(
        sb: &'a dyn Sandbox,
        root: &'a Path,
        git_dir: &'a Path,
        program: &'a Path,
        index: Option<&'a Path>,
        groups: Option<&'a ProcessGroups>,
        version: &'a GitVersion,
    ) -> Git<'a> {
        Git {
            sb,
            root,
            git_dir,
            work_tree: root,
            program,
            index,
            groups,
            config: tokio::sync::OnceCell::new(),
            version,
        }
    }

    /// Run `git <args>` (with `input` on stdin) until `deadline`; its stdout on success.
    /// Every call runs with every configured filter driver and protocol switched off
    /// (see [`Git::config`]): not only `add` and `checkout` convert content, any command
    /// that refreshes the index (`write-tree`) may run a clean filter to compare a racily
    /// clean file, and any command that reads an object a partial clone lacks may fetch
    /// it. In a partial clone, a git older than 2.44 (which cannot be told not to fetch)
    /// runs nothing at all.
    async fn run(
        &self,
        args: &[&str],
        input: Option<Vec<u8>>,
        deadline: Instant,
    ) -> Result<Vec<u8>, Fail> {
        let config = self.config(deadline).await?;
        if config.partial_clone {
            let version = self
                .version
                .get_or_try_init(|| self.ask_version(deadline))
                .await?;
            if !version.is_some_and(|v| v >= LAZY_FETCH_OFF_SINCE) {
                return Err(Fail::UnsafePartialClone);
            }
        }
        self.exec(args, &config.overrides, input, deadline).await
    }

    /// The git program's `(major, minor)` version (none: it did not say).
    async fn ask_version(&self, deadline: Instant) -> Result<Option<(u32, u32)>, Fail> {
        match self.exec(&["version"], &[], None, deadline).await {
            Ok(out) => Ok(parse_version(&String::from_utf8_lossy(&out))),
            Err(Fail::Exit { .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// The configuration that matters here, read once (any scope, includes followed;
    /// reading it runs nothing): see [`RepoConfig`].
    ///
    /// - Every filter driver it defines is switched off: a repository that came with a
    ///   `.git/config` and `.gitattributes` must not get its programs run by a checkpoint.
    ///   With the filters off, files tracked through a filter (Git LFS, git-crypt) are
    ///   snapshotted and restored as their bytes in the work tree.
    /// - Every `protocol.<name>.allow` it sets becomes `never` (`protocol.allow=never`
    ///   alone does not override them), so a lazy fetch from a promisor remote can reach
    ///   no transport (an `ext::` URL runs a command).
    ///
    /// Names git cannot take on the command line fail the call (no snapshot, rather than
    /// a program run).
    async fn config(&self, deadline: Instant) -> Result<&RepoConfig, Fail> {
        self.config
            .get_or_try_init(|| self.read_config(deadline))
            .await
    }

    async fn read_config(&self, deadline: Instant) -> Result<RepoConfig, Fail> {
        let wanted = r"^(filter\..+|protocol\..+\.allow|extensions\.partialclone|remote\..+\.(promisor|partialclonefilter))$";
        let out = match self
            .exec(
                &["config", "-z", "--get-regexp", wanted],
                &[],
                None,
                deadline,
            )
            .await
        {
            Ok(out) => out,
            // Nothing matched.
            Err(Fail::Exit { code: Some(1), .. }) => return Ok(RepoConfig::default()),
            Err(e) => return Err(e),
        };
        parse_config(&out)
    }

    /// The user's excludes file: `core.excludesFile` of the global configuration (its
    /// includes followed), else `$XDG_CONFIG_HOME/git/ignore` (`~/.config/git/ignore`).
    /// Never a value the repository's own configuration sets: that would let a repository
    /// name any file of the user's to be read. The system configuration is not read by
    /// any checkpoint git call (`GIT_CONFIG_NOSYSTEM`). Err: it could not be told.
    async fn user_excludes_file(&self, deadline: Instant) -> Result<Option<PathBuf>, Fail> {
        let args = [
            "config",
            "--global",
            "--includes",
            "-z",
            "--get",
            "core.excludesfile",
        ];
        let configured = match self.run(&args, None, deadline).await {
            Ok(out) => {
                let value = String::from_utf8_lossy(&out);
                let value = value.trim_end_matches('\0');
                (!value.is_empty()).then(|| expand_home(value))
            }
            // Not set.
            Err(Fail::Exit { code: Some(1), .. }) => None,
            Err(e) => return Err(e),
        };
        Ok(
            configured.or_else(|| match std::env::var_os("XDG_CONFIG_HOME") {
                Some(xdg) if !xdg.is_empty() => Some(PathBuf::from(xdg).join("git/ignore")),
                _ => std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config/git/ignore")),
            }),
        )
    }

    /// Run `git [hardening] [extra] <args>` (with `input` on stdin) until `deadline`.
    async fn exec(
        &self,
        args: &[&str],
        extra: &[String],
        input: Option<Vec<u8>>,
        deadline: Instant,
    ) -> Result<Vec<u8>, Fail> {
        let mut argv = vec![
            self.program.to_string_lossy().into_owned(),
            "--no-pager".into(),
            format!("--git-dir={}", self.git_dir.to_string_lossy()),
            format!("--work-tree={}", self.work_tree.to_string_lossy()),
        ];
        for setting in HARDENING {
            argv.push("-c".into());
            argv.push(setting.into());
        }
        argv.extend(extra.iter().cloned());
        argv.extend(args.iter().map(|a| a.to_string()));
        let mut env: Vec<(String, String)> = [
            ("GIT_CONFIG_NOSYSTEM", "1"),
            ("GIT_TERMINAL_PROMPT", "0"),
            ("GIT_ASKPASS", ""),
            ("GIT_PAGER", "cat"),
            ("PAGER", "cat"),
            ("GIT_EDITOR", "true"),
            ("GIT_SEQUENCE_EDITOR", "true"),
            ("GIT_NO_LAZY_FETCH", "1"),
            // Set (to no protocol), it overrides every `protocol.*` setting: no transport.
            ("GIT_ALLOW_PROTOCOL", ""),
            ("GIT_OPTIONAL_LOCKS", "0"),
            ("GIT_LITERAL_PATHSPECS", "1"),
            ("GIT_AUTHOR_NAME", "tautly"),
            ("GIT_AUTHOR_EMAIL", "tautly@localhost"),
            ("GIT_COMMITTER_NAME", "tautly"),
            ("GIT_COMMITTER_EMAIL", "tautly@localhost"),
        ]
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .to_vec();
        if let Some(parent) = self.root.parent() {
            env.push((
                "GIT_CEILING_DIRECTORIES".into(),
                parent.to_string_lossy().into_owned(),
            ));
        }
        let mut env_remove: Vec<String> = ENV_REMOVED.map(String::from).to_vec();
        // `check-ignore` takes no pathspec magic, literal included (it is given absolute
        // paths, which have none).
        if args.first() == Some(&"check-ignore") {
            env.retain(|(k, _)| k != "GIT_LITERAL_PATHSPECS");
            env_remove.push("GIT_LITERAL_PATHSPECS".into());
        }
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
                &[
                    "diff-tree",
                    "-r",
                    "-z",
                    "--no-renames",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--raw",
                    a,
                    b,
                ],
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

    /// Every ref below `prefix`.
    async fn refs(&self, prefix: &str, deadline: Instant) -> Result<Vec<RefInfo>, Fail> {
        let out = self
            .run(
                &[
                    "for-each-ref",
                    "--format=%(refname)%00%(committerdate:unix)%00%(objectname)",
                    prefix,
                ],
                None,
                deadline,
            )
            .await?;
        Ok(String::from_utf8_lossy(&out)
            .lines()
            .filter_map(|l| {
                let mut fields = l.split('\0');
                let (name, date, commit) = (fields.next()?, fields.next()?, fields.next()?);
                Some(RefInfo {
                    name: name.to_string(),
                    date: date.trim().parse().unwrap_or(0),
                    commit: commit.trim().to_string(),
                })
            })
            .collect())
    }
}

/// One ref of [`Git::refs`]: its name, its commit, and that commit's committer date (unix
/// seconds).
struct RefInfo {
    name: String,
    date: u64,
    commit: String,
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

/// Parse `config -z --get-regexp` output (`<key>` LF `<value>` NUL per entry, no LF for a
/// key without a value) of the keys [`Git::read_config`] asks for.
fn parse_config(out: &[u8]) -> Result<RepoConfig, Fail> {
    let mut filters = std::collections::BTreeSet::new();
    let mut protocols = std::collections::BTreeSet::new();
    let mut config = RepoConfig::default();
    for entry in out.split(|&b| b == 0).filter(|e| !e.is_empty()) {
        let (key, value) = match entry.iter().position(|&b| b == b'\n') {
            Some(lf) => (&entry[..lf], Some(&entry[lf + 1..])),
            None => (entry, None),
        };
        let key = std::str::from_utf8(key)
            .map_err(|_| Fail::Io("a configuration key is not valid UTF-8".into()))?;
        let value = value.map(String::from_utf8_lossy);
        let unusable =
            |what: &str, name: &str| Fail::Io(format!("{what} {name:?} cannot be switched off"));
        if let Some(rest) = key.strip_prefix("filter.") {
            let Some((name, _var)) = rest.rsplit_once('.') else {
                continue;
            };
            if name.contains('=') {
                return Err(unusable("filter driver", name));
            }
            filters.insert(name.to_string());
        } else if let Some(name) = key
            .strip_prefix("protocol.")
            .and_then(|rest| rest.strip_suffix(".allow"))
        {
            if name.contains('=') {
                return Err(unusable("protocol", name));
            }
            protocols.insert(name.to_string());
        } else if key == "extensions.partialclone" {
            config.partial_clone |= value.is_none_or(|v| !v.is_empty());
        } else if key.ends_with(".partialclonefilter") {
            // `remote.<name>.partialclonefilter` alone makes `<name>` a promisor remote.
            config.partial_clone = true;
        } else if key.starts_with("remote.") {
            // `remote.<name>.promisor`: anything but a false boolean counts.
            let off = value.is_some_and(|v| {
                matches!(
                    v.trim().to_ascii_lowercase().as_str(),
                    "" | "false" | "no" | "off" | "0"
                )
            });
            config.partial_clone |= !off;
        }
    }
    for name in filters {
        for setting in ["clean=", "smudge=", "process=", "required=false"] {
            config.overrides.push("-c".to_string());
            config.overrides.push(format!("filter.{name}.{setting}"));
        }
    }
    for name in protocols {
        config.overrides.push("-c".to_string());
        config
            .overrides
            .push(format!("protocol.{name}.allow=never"));
    }
    Ok(config)
}

/// Whether a push refspec's source can name a checkpoint ref (`refs/tautly/…`, or
/// `tautly/…`, which git expands to it): a glob whose fixed start is compatible with it,
/// or a ref below it. A negative refspec (`^…`) and the matching one (`:`) cannot.
fn pushes_checkpoints(refspec: &str) -> bool {
    let spec = refspec.trim();
    let spec = spec.strip_prefix('+').unwrap_or(spec);
    if spec.starts_with('^') {
        return false;
    }
    let src = spec.split_once(':').map_or(spec, |(src, _)| src);
    if src.is_empty() {
        return false;
    }
    let names = ["refs/tautly/", "tautly/"];
    match src.split_once('*') {
        Some((start, _)) => names
            .iter()
            .any(|n| n.starts_with(start) || start.starts_with(n)),
        None => names.iter().any(|n| src.starts_with(n)),
    }
}

/// `(major, minor)` of `git version 2.44.0 (…)`. A release candidate (`2.44.0.rc1`,
/// `2.44.0-rc1`) comes before its release: `2.44.0-rc1` gives `(2, 43)`.
fn parse_version(text: &str) -> Option<(u32, u32)> {
    let number = text.trim().strip_prefix("git version ")?;
    let number = number.split_whitespace().next()?;
    let mut parts = number.split(['.', '-']);
    let digits = |p: &str| -> Option<u32> {
        (!p.is_empty() && p.bytes().all(|b| b.is_ascii_digit())).then(|| p.parse().ok())?
    };
    let major = digits(parts.next()?)?;
    let minor = parts
        .next()
        .and_then(|p| digits(p.split(|c: char| !c.is_ascii_digit()).next()?))?;
    let candidate = parts.take(2).any(|p| p.starts_with("rc"));
    if !candidate {
        Some((major, minor))
    } else if minor > 0 {
        Some((major, minor - 1))
    } else {
        Some((major.checked_sub(1)?, u32::MAX))
    }
}

/// A configured path with a leading `~/` expanded (as git does; `~user/` is left).
fn expand_home(path: &str) -> PathBuf {
    match (path.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => PathBuf::from(path),
    }
}

/// The repository's common directory (where `info/exclude` is): the one a linked
/// worktree's `commondir` names, else the repository itself.
fn common_dir(git_dir: &Path) -> PathBuf {
    match std::fs::read_to_string(git_dir.join("commondir")) {
        Ok(text) if !text.trim().is_empty() => git_dir.join(text.trim()),
        _ => git_dir.to_path_buf(),
    }
}

/// A regular file's bytes, up to `limit` (none: missing). Err: a symbolic link (unless
/// `follow`; otherwise never followed: `O_NOFOLLOW`), not a regular file (a FIFO is opened
/// without blocking and never read), longer than `limit`, or unreadable.
fn read_regular(path: &Path, follow: bool, limit: u64) -> Result<Option<Vec<u8>>, ()> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    let nofollow = if follow { 0 } else { libc::O_NOFOLLOW };
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(nofollow | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(()),
    };
    match file.metadata() {
        Ok(m) if m.is_file() && m.len() <= limit => {}
        _ => return Err(()),
    }
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes).map_err(drop)?;
    if bytes.len() as u64 > limit {
        return Err(());
    }
    Ok(Some(bytes))
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
        let program = git_on_path()?;
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
        Git::new(
            self.sb.as_ref(),
            &self.root,
            &self.git_dir,
            &self.program,
            Some(&self.index),
            self.groups.as_ref(),
            &self.version,
        )
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

    async fn take(&mut self, backup: bool) -> Result<CheckpointRef, Skip> {
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
    /// name a ref below `refs/tautly/` ([`pushes_checkpoints`]). Empty when the
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
    /// is overwritten or deleted), which rewind and switch (spec 7.12) add on top.
    #[allow(dead_code)] // the building block of rewind and switch (spec 7.12)
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

/// A path of plain names only (no root, `.` or `..`).
fn plain_relative(p: &Path) -> bool {
    p.components()
        .all(|c| matches!(c, std::path::Component::Normal(_)))
}

/// The first executable `git` on `PATH`.
pub(crate) fn git_on_path() -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join("git"))
        .find(|p| {
            std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
}

/// Whether the root's `.git` is a symbolic link. Checkpoints are then not taken: they
/// would write objects and refs to whatever repository it points at.
pub(crate) fn dot_git_is_link(root: &Path) -> bool {
    std::fs::symlink_metadata(root.join(".git")).is_ok_and(|m| m.file_type().is_symlink())
}

/// The repository of the work tree at `root`: its `.git` directory, or the directory a
/// `.git` file (`gitdir: <path>`, a linked worktree or submodule) names. None when `root`
/// has neither (a `.git` symbolic link is neither: see [`dot_git_is_link`]). Every git call names it (`--git-dir`) with the root as the work tree, so
/// neither the environment nor `core.worktree` can point git elsewhere.
pub(crate) fn git_dir(root: &Path) -> Option<PathBuf> {
    let dot = root.join(".git");
    let meta = std::fs::symlink_metadata(&dot).ok()?;
    if meta.is_dir() {
        return Some(dot);
    }
    if !meta.is_file() {
        return None;
    }
    let text = std::fs::read_to_string(&dot).ok()?;
    let named = text.lines().next()?.strip_prefix("gitdir:")?.trim();
    (!named.is_empty()).then(|| root.join(named))
}

/// Run `f` with git from `PATH` on the repository at `root`, for the free functions.
async fn with_plain_git<T>(
    sb: &dyn Sandbox,
    root: &Path,
    f: impl AsyncFnOnce(&Git<'_>) -> Result<T, CheckpointError>,
) -> Result<T, CheckpointError> {
    let program = git_on_path().ok_or(CheckpointError::NoGit)?;
    let git_dir = git_dir(root).ok_or_else(|| {
        CheckpointError::Invalid(format!("{} is not a git work tree", root.display()))
    })?;
    let version = GitVersion::new();
    let git = Git::new(sb, root, &git_dir, &program, None, None, &version);
    f(&git).await
}

/// Delete every checkpoint ref (of any session of the repository at `root`) whose commit
/// is older than `keep_days`; returns how many. The exclude rules kept for them in the
/// store at `excludes` go too (and entries of that age no ref names). Run at engine start;
/// `git gc` is never run, so the objects stay until git's own housekeeping.
pub async fn prune(
    sb: &dyn Sandbox,
    root: &Path,
    keep_days: u32,
    excludes: Option<&Path>,
) -> Result<u32, CheckpointError> {
    with_plain_git(sb, root, async |git| {
        let deadline = Checkpointer::housekeeping_deadline();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let cutoff = now.saturating_sub(u64::from(keep_days) * 24 * 3600);
        let (old, kept): (Vec<_>, Vec<_>) = git
            .refs(&format!("{REF_PREFIX}/"), deadline)
            .await?
            .into_iter()
            .partition(|r| r.date < cutoff);
        let names: Vec<String> = old.iter().map(|r| r.name.clone()).collect();
        let n = delete_refs(git, &names, deadline).await?;
        if let Some(dir) = excludes {
            let gone: HashSet<String> = old.into_iter().map(|r| r.commit).collect();
            let keep = kept.into_iter().map(|r| r.commit).collect();
            let cutoff = UNIX_EPOCH + Duration::from_secs(cutoff);
            // Also entries that old whose ref was never made (a put before a failed one).
            forget_excludes(dir, keep, move |c, modified| {
                gone.contains(c) || modified < cutoff
            })
            .await;
        }
        Ok(n)
    })
    .await
}

/// [`ExcludesStore::prune`] on the store at `dir`, off the async workers.
async fn forget_excludes(
    dir: &Path,
    keep: HashSet<String>,
    drop: impl Fn(&str, SystemTime) -> bool + Send + 'static,
) {
    let store = ExcludesStore::new(dir.to_path_buf());
    let _ = tokio::task::spawn_blocking(move || store.prune(&keep, drop)).await;
}

/// Delete session `session`'s checkpoint refs in the repository at `root` (`tautly
/// sessions delete`), and the exclude rules kept for them in the store at `excludes`
/// unless another ref (a branch's) names the same commit; returns how many refs. The
/// objects (and the session blobs) are shared and kept.
pub async fn delete_session_refs(
    sb: &dyn Sandbox,
    root: &Path,
    session: &str,
    excludes: Option<&Path>,
) -> Result<u32, CheckpointError> {
    check_session(session)?;
    with_plain_git(sb, root, async |git| {
        let deadline = Checkpointer::housekeeping_deadline();
        let own = format!("{REF_PREFIX}/{session}/");
        let (mine, others): (Vec<_>, Vec<_>) = git
            .refs(&format!("{REF_PREFIX}/"), deadline)
            .await?
            .into_iter()
            .partition(|r| r.name.starts_with(&own));
        let names: Vec<String> = mine.iter().map(|r| r.name.clone()).collect();
        let n = delete_refs(git, &names, deadline).await?;
        if let Some(dir) = excludes {
            let gone: HashSet<String> = mine.into_iter().map(|r| r.commit).collect();
            let keep = others.into_iter().map(|r| r.commit).collect();
            forget_excludes(dir, keep, move |c, _| gone.contains(c)).await;
        }
        Ok(n)
    })
    .await
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
    /// A permanent skip happened ([`Skip::is_permanent`]), or a missing object failed a
    /// checkpoint twice: no checkpoint is taken (or logged) for the rest of the session.
    off: bool,
    /// A missing object has failed a checkpoint ([`Skip::is_missing_object`]) and been
    /// warned about.
    missing_object: bool,
}

impl GitCheckpoints {
    pub fn shared(cp: Checkpointer) -> Arc<tokio::sync::Mutex<GitCheckpoints>> {
        Arc::new(tokio::sync::Mutex::new(GitCheckpoints {
            cp,
            fs_events: None,
            warned: None,
            off: false,
            missing_object: false,
        }))
    }
}

/// Where in the session a checkpoint is taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Point {
    /// Session start: always a full snapshot.
    SessionStart,
    /// A resumed session's start: always a full snapshot.
    ResumeStart,
    /// Before a command (`bash`, a stdio MCP call): always looks at the files, since a
    /// change the watcher has not delivered yet must not be attributed to the command.
    BeforeExec,
    /// At a user turn's end: reused without running git when the write generation did
    /// not move and the watcher saw no change.
    TurnEnd,
}

impl Point {
    /// How the log names it.
    fn logged(self) -> Option<CheckpointPoint> {
        Some(match self {
            Point::SessionStart => CheckpointPoint::SessionStart,
            Point::ResumeStart => CheckpointPoint::ResumeStart,
            Point::BeforeExec => CheckpointPoint::BeforeExec,
            Point::TurnEnd => CheckpointPoint::TurnEnd,
        })
    }
}

/// How the session-start checkpoint stands.
#[derive(Default)]
pub(crate) enum StartCheckpoint {
    /// Not started, done, or nothing to take.
    #[default]
    Idle,
    /// Checkpoints are on in a git work tree, but none can be taken (no `git` on `PATH`,
    /// or `.git` is a symbolic link): said once, at session start.
    Skipped(Skip),
    /// Being taken in the background (it holds the checkpoint lock from the start).
    Running(tokio::task::JoinHandle<()>),
}

/// One checkpoint to take and log, with what it needs from the session (so it can run in
/// the background).
struct CheckpointJob {
    log: Log,
    events: EventSink,
    turn: u32,
    step: u32,
    write_gen: u64,
    /// The watcher's event count (none: no watcher).
    watcher: Option<u64>,
}

impl CheckpointJob {
    /// Take the checkpoint at `point` and log it (`Checkpoint`, or `CheckpointSkipped`
    /// with a warning), holding the lock throughout, so the log has checkpoints in the
    /// order they were taken. A cancel abandons the snapshot (its processes are killed),
    /// logged as skipped.
    async fn run(self, st: &mut GitCheckpoints, point: Point, cancel: Option<&CancellationToken>) {
        if st.off {
            return;
        }
        let watcher_changed = self.watcher.is_none() || self.watcher != st.fs_events;
        st.fs_events = self.watcher;
        let write_gen = self.write_gen;
        let snap = async {
            match point {
                Point::SessionStart | Point::ResumeStart => st.cp.snapshot_now().await,
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
        let (turn, step) = (self.turn, self.step);
        let rec = match taken {
            Some(Ok(r)) => SessionRecord::Checkpoint {
                n: r.n,
                commit: r.commit,
                tree: r.tree,
                head: r.head,
                dirty: r.dirty,
                turn,
                step,
                point: point.logged(),
            },
            Some(Err(skip)) => {
                st.off = skip.is_permanent();
                if skip.is_missing_object() {
                    // It recurs: off for the session, without another warning.
                    if st.missing_object {
                        st.off = true;
                    } else {
                        st.missing_object = true;
                        self.events
                            .warn(format!(
                                "{} (an object git needs is missing; if it happens again, \
                                 checkpoints are off for this session)",
                                skip_warning(&skip)
                            ))
                            .await;
                    }
                } else {
                    let shown = skip.to_string();
                    let fresh = st.warned.as_ref() != Some(&(turn, shown.clone()));
                    st.warned = Some((turn, shown));
                    if fresh {
                        self.events.warn(skip_warning(&skip)).await;
                    }
                }
                SessionRecord::CheckpointSkipped {
                    reason: skip.code().into(),
                    turn,
                    step,
                    point: point.logged(),
                }
            }
            None => {
                st.cp.invalidate();
                SessionRecord::CheckpointSkipped {
                    reason: "cancelled".into(),
                    turn,
                    step,
                    point: point.logged(),
                }
            }
        };
        self.log(rec).await;
    }

    async fn log(&self, rec: SessionRecord) {
        let log = self.log.clone();
        if let Ok(Err(failure)) = tokio::task::spawn_blocking(move || log.append(&rec)).await
            && let Some(ev) = failure.event()
        {
            self.events.emit(ev).await;
        }
    }
}

impl Session {
    /// What a checkpoint of step `step` needs from the session now.
    fn checkpoint_job(&self, step: u32) -> CheckpointJob {
        CheckpointJob {
            log: self.log.clone(),
            events: self.events.clone(),
            turn: self.user_turn,
            step,
            write_gen: self.ledger.generation(),
            watcher: self
                .workspace
                .index()
                .filter(|i| i.watching())
                .map(|i| i.fs_events()),
        }
    }

    /// Take a checkpoint at `point` of provider step `step` and log it (see
    /// [`CheckpointJob::run`]). Nothing without a checkpointer.
    pub(crate) async fn git_checkpoint(
        &self,
        point: Point,
        step: u32,
        cancel: Option<&CancellationToken>,
    ) {
        let Some(shared) = self.checkpoints.clone() else {
            return;
        };
        let job = self.checkpoint_job(step);
        // The lock may be held by the session-start checkpoint for seconds: a cancel stops
        // waiting for it too.
        let mut st = match cancel {
            Some(c) => tokio::select! {
                st = shared.lock() => st,
                () = c.cancelled() => {
                    job.log(SessionRecord::CheckpointSkipped {
                        reason: "cancelled".into(),
                        turn: job.turn,
                        step,
                        point: point.logged(),
                    })
                    .await;
                    return;
                }
            },
            None => shared.lock().await,
        };
        job.run(&mut st, point, cancel).await;
    }

    /// At session start (after init or resume): the session-start checkpoint, in the
    /// background, so neither MCP start nor the first command waits for it. It takes the
    /// checkpoint lock before this returns, so every later checkpoint (the first command's
    /// included) comes after it; a turn's tool calls wait for it
    /// ([`settle_start_checkpoint_or`](Self::settle_start_checkpoint_or)). When none can be
    /// taken (no git, `.git` a symbolic link), a warning and a `CheckpointSkipped` record
    /// instead. A remote a push to would publish the checkpoint refs to (see
    /// [`Checkpointer::publishing_remotes`]) is warned about first, once.
    pub(crate) async fn begin_checkpoints(&mut self, resumed: bool) {
        let point = if resumed {
            Point::ResumeStart
        } else {
            Point::SessionStart
        };
        if let StartCheckpoint::Skipped(skip) = std::mem::take(&mut self.start_checkpoint) {
            self.warn(skip_warning(&skip)).await;
            self.log(SessionRecord::CheckpointSkipped {
                reason: skip.code().into(),
                turn: self.user_turn,
                step: self.last_step(),
                point: point.logged(),
            })
            .await;
            return;
        }
        let Some(shared) = self.checkpoints.clone() else {
            return;
        };
        let job = self.checkpoint_job(self.last_step());
        let mut st = shared.lock_owned().await;
        self.start_checkpoint = StartCheckpoint::Running(tokio::spawn(async move {
            let remotes = st.cp.publishing_remotes().await;
            if !remotes.is_empty() {
                job.events.warn(publishing_warning(&remotes)).await;
            }
            job.run(&mut st, point, None).await;
        }));
    }

    /// Wait for the session-start checkpoint, if it is still being taken: before a turn's
    /// first tool call (whose edits it must not see), and before an undo or fork reads the
    /// log. Unless `cancel` fires first (`Cancel` or `Shutdown`, spec 8): the checkpoint
    /// then goes on in the background and is waited for later (or abandoned at shutdown).
    /// True when it is settled.
    pub(crate) async fn settle_start_checkpoint_or(&mut self, cancel: &CancellationToken) -> bool {
        let StartCheckpoint::Running(job) = &mut self.start_checkpoint else {
            return true;
        };
        tokio::select! {
            biased;
            _ = job => {
                self.start_checkpoint = StartCheckpoint::Idle;
                true
            }
            () = cancel.cancelled() => false,
        }
    }

    /// At shutdown: abandon a session-start checkpoint still being taken (its git is
    /// killed).
    pub(crate) fn abandon_start_checkpoint(&mut self) {
        if let StartCheckpoint::Running(job) = std::mem::take(&mut self.start_checkpoint) {
            job.abort();
        }
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
    fn git_dir_follows_a_dot_git_file() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(git_dir(dir.path()), None);
        std::fs::write(
            dir.path().join(".git"),
            "gitdir: ../main/.git/worktrees/w\n",
        )
        .unwrap();
        assert_eq!(
            git_dir(dir.path()),
            Some(dir.path().join("../main/.git/worktrees/w"))
        );
        std::fs::write(dir.path().join(".git"), "gitdir: /abs/repo.git\n").unwrap();
        assert_eq!(git_dir(dir.path()), Some(PathBuf::from("/abs/repo.git")));
        std::fs::write(dir.path().join(".git"), "not a pointer\n").unwrap();
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
remote.origin.promisor\nfalse\0";
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
