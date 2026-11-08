//! The hardened `git` runner of the checkpointer, its output parsers and the process group
//! each call runs in.

use super::*;

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
pub(super) struct Git<'a> {
    pub(super) sb: &'a dyn Sandbox,
    /// The sandbox root: the working directory of every call.
    pub(super) root: &'a Path,
    /// The repository (`--git-dir`), resolved from the root's `.git`.
    pub(super) git_dir: &'a Path,
    /// The work tree (`--work-tree`): the root, so `core.worktree` cannot move it.
    pub(super) work_tree: &'a Path,
    pub(super) program: &'a Path,
    pub(super) index: Option<&'a Path>,
    pub(super) groups: Option<&'a ProcessGroups>,
    /// What the configuration sets that matters here, read once per `Git` (one
    /// operation).
    pub(super) config: tokio::sync::OnceCell<RepoConfig>,
    /// The git program's version (`(major, minor)`; none: unknown), asked once per
    /// checkpointer (session).
    pub(super) version: &'a GitVersion,
    /// Variables removed from git's environment besides [`ENV_REMOVED`]: the provider key
    /// variables, so no child tautly starts in the work tree sees a provider key.
    pub(super) env_strip: &'a [String],
}

/// The cell a checkpointer keeps its git's version in.
pub(super) type GitVersion = tokio::sync::OnceCell<Option<(u32, u32)>>;

/// Longest a read of the repository's configuration may take, whatever the operation's
/// deadline: an `include.path` naming a FIFO blocks every git call, and each would wait
/// out its whole deadline (60 s for a restore). Below the default snapshot timeout, so
/// that case turns checkpoints off for the session ([`Skip::ConfigTimeout`]) instead of
/// costing every command the full timeout.
const CONFIG_READ_TIMEOUT: Duration = Duration::from_secs(2);

/// The first git release that honours `GIT_NO_LAZY_FETCH`.
const LAZY_FETCH_OFF_SINCE: (u32, u32) = (2, 44);

/// Most bytes of exclude rules (the user's excludes file and `info/exclude`) kept for a
/// checkpoint; above it, undo falls back to the rules as they are then.
pub(super) const MAX_EXCLUDES: u64 = 256 * 1024;

/// What one read of the configuration (every scope, includes followed) gives.
#[derive(Debug, Default)]
pub(super) struct RepoConfig {
    /// `-c` settings that switch off every filter driver and every protocol the
    /// configuration allows.
    pub(super) overrides: Vec<String>,
    /// `extensions.partialClone`, a promisor remote or a remote's partial-clone filter is
    /// set: missing objects would be fetched (lazily) from a remote.
    pub(super) partial_clone: bool,
}

impl<'a> Git<'a> {
    pub(super) fn new(
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
            env_strip: &[],
        }
    }

    /// Run `git <args>` (with `input` on stdin) until `deadline`; its stdout on success.
    /// Every call runs with every configured filter driver and protocol switched off
    /// (see [`Git::config`]): not only `add` and `checkout` convert content, any command
    /// that refreshes the index (`write-tree`) may run a clean filter to compare a racily
    /// clean file, and any command that reads an object a partial clone lacks may fetch
    /// it. In a partial clone, a git older than 2.44 (which cannot be told not to fetch)
    /// runs nothing at all.
    pub(super) async fn run(
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
    /// - Every hook it declares (`hook.<name>.*`, git 2.54) is disabled and loses its
    ///   events: `core.hooksPath=/dev/null` only moves the directory of file hooks.
    ///
    /// Names git cannot take on the command line fail the call (no snapshot, rather than
    /// a program run).
    async fn config(&self, deadline: Instant) -> Result<&RepoConfig, Fail> {
        self.config
            .get_or_try_init(|| self.read_config(deadline))
            .await
    }

    /// Within [`CONFIG_READ_TIMEOUT`] as well as `deadline`; running out of the former is
    /// [`Fail::ConfigTimeout`].
    async fn read_config(&self, deadline: Instant) -> Result<RepoConfig, Fail> {
        let wanted = r"^(filter\..+|hook\..+|protocol\..+\.allow|extensions\.partialclone|remote\..+\.(promisor|partialclonefilter))$";
        let cap = Instant::now() + CONFIG_READ_TIMEOUT;
        let out = match self
            .exec(
                &["config", "-z", "--get-regexp", wanted],
                &[],
                None,
                deadline.min(cap),
            )
            .await
        {
            Ok(out) => out,
            // Nothing matched.
            Err(Fail::Exit { code: Some(1), .. }) => return Ok(RepoConfig::default()),
            Err(Fail::Timeout) if cap <= deadline => return Err(Fail::ConfigTimeout),
            Err(e) => return Err(e),
        };
        parse_config(&out)
    }

    /// The user's excludes file: `core.excludesFile` of the global configuration (its
    /// includes followed), else `$XDG_CONFIG_HOME/git/ignore` (`~/.config/git/ignore`).
    /// Never a value the repository's own configuration sets: that would let a repository
    /// name any file of the user's to be read. The system configuration is not read by
    /// any checkpoint git call (`GIT_CONFIG_NOSYSTEM`). Err: it could not be told.
    pub(super) async fn user_excludes_file(
        &self,
        deadline: Instant,
    ) -> Result<Option<PathBuf>, Fail> {
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
            // English messages: `Skip::is_missing_object` matches git's error text.
            ("LC_ALL", "C"),
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
        env_remove.extend(self.env_strip.iter().cloned());
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
    pub(super) async fn text(&self, args: &[&str], deadline: Instant) -> Result<String, Fail> {
        let out = self.run(args, None, deadline).await?;
        Ok(String::from_utf8_lossy(&out).trim().to_string())
    }

    /// `(HEAD commit, its tree)`; none when HEAD is unborn.
    pub(super) async fn head(&self, deadline: Instant) -> Result<Option<(String, String)>, Fail> {
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
    pub(super) async fn diff(
        &self,
        a: &str,
        b: &str,
        deadline: Instant,
    ) -> Result<Vec<Change>, Fail> {
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
    pub(super) async fn update_refs(
        &self,
        commands: Vec<u8>,
        deadline: Instant,
    ) -> Result<(), Fail> {
        if commands.is_empty() {
            return Ok(());
        }
        self.run(&["update-ref", "--stdin", "-z"], Some(commands), deadline)
            .await
            .map(drop)
    }

    /// Every ref below `prefix`.
    pub(super) async fn refs(&self, prefix: &str, deadline: Instant) -> Result<Vec<RefInfo>, Fail> {
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
pub(super) struct RefInfo {
    pub(super) name: String,
    pub(super) date: u64,
    pub(super) commit: String,
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
pub(super) fn parse_raw_diff(out: &[u8]) -> Result<Vec<Change>, Fail> {
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
pub(super) fn parse_config(out: &[u8]) -> Result<RepoConfig, Fail> {
    let mut filters = std::collections::BTreeSet::new();
    let mut hooks = std::collections::BTreeSet::new();
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
        } else if let Some(rest) = key.strip_prefix("hook.") {
            // `hook.<name>.<var>`: a hook declared in configuration (git 2.54), which
            // `core.hooksPath` does not switch off. (`hook.<var>` declares none.)
            let Some((name, _var)) = rest.rsplit_once('.') else {
                continue;
            };
            if name.contains('=') {
                return Err(unusable("hook", name));
            }
            hooks.insert(name.to_string());
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
    // An empty `event` clears the hook's events; a git without `enabled` ignores it.
    for name in hooks {
        for setting in ["enabled=false", "event="] {
            config.overrides.push("-c".to_string());
            config.overrides.push(format!("hook.{name}.{setting}"));
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
pub(super) fn pushes_checkpoints(refspec: &str) -> bool {
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
pub(super) fn parse_version(text: &str) -> Option<(u32, u32)> {
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

/// The provider key variables no git call inherits, before the configured ones
/// ([`Checkpointer::with_env_remove`]).
pub(super) fn default_env_strip() -> Vec<String> {
    tautly_mcp::PROVIDER_KEY_VARS
        .iter()
        .map(|v| v.to_string())
        .collect()
}

/// The repository's common directory (where `info/exclude` is): the one a linked
/// worktree's `commondir` names, else the repository itself.
pub(super) fn common_dir(git_dir: &Path) -> PathBuf {
    match std::fs::read_to_string(git_dir.join("commondir")) {
        Ok(text) if !text.trim().is_empty() => git_dir.join(text.trim()),
        _ => git_dir.to_path_buf(),
    }
}

/// A regular file's bytes, up to `limit` (none: missing). Err: a symbolic link (unless
/// `follow`; otherwise never followed: `O_NOFOLLOW`), not a regular file (a FIFO is opened
/// without blocking and never read), longer than `limit`, or unreadable.
pub(super) fn read_regular(path: &Path, follow: bool, limit: u64) -> Result<Option<Vec<u8>>, ()> {
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

pub(super) fn bytes_to_os(b: &[u8]) -> OsString {
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
    pub(super) fn new(groups: Option<&ProcessGroups>, pgid: u32) -> Group {
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
