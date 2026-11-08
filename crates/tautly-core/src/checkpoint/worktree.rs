//! The free functions on the repository: pruning and deleting checkpoint refs, and the
//! linked work trees of `tautly worktree`.

use super::*;

/// Run `f` with git from `PATH` on the repository at `root`, for the free functions.
async fn with_plain_git<T>(
    sb: &dyn Sandbox,
    root: &Path,
    f: impl AsyncFnOnce(&Git<'_>) -> Result<T, CheckpointError>,
) -> Result<T, CheckpointError> {
    let program = git_on_path(root).ok_or(CheckpointError::NoGit)?;
    let git_dir = git_dir(root).ok_or_else(|| {
        CheckpointError::Invalid(format!("{} is not a git work tree", root.display()))
    })?;
    let version = GitVersion::new();
    // The default provider key variables (the free functions have no configuration).
    let strip = default_env_strip();
    let git = Git {
        env_strip: &strip,
        ..Git::new(sb, root, &git_dir, &program, None, None, &version)
    };
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

/// `git worktree add --no-checkout --detach <dir> <commit>` on the repository at `root` (a
/// fork's own worktree, spec 7.12.4), with every setting a checkpoint's git runs with: no
/// hook (a `post-checkout` one included), fsmonitor or transport runs. Nothing is checked
/// out here: the configuration a checkout in the new worktree reads is the worktree's own
/// (`includeIf "gitdir:…/worktrees/…"`, its `config.worktree`), which a call in the main
/// repository's context does not see, so a filter defined there would run (review C1).
/// [`worktree_checkout`] fills it. Only the new worktree's administrative files are
/// written; the user's index, HEAD and work tree are not.
pub(crate) async fn worktree_add(
    sb: &dyn Sandbox,
    root: &Path,
    dir: &Path,
    commit: &str,
) -> Result<(), CheckpointError> {
    let dir = dir.to_string_lossy();
    with_plain_git(sb, root, async |git| {
        let args = [
            "worktree",
            "add",
            "--no-checkout",
            "--detach",
            "--",
            &dir,
            commit,
        ];
        git.run(&args, None, Checkpointer::housekeeping_deadline())
            .await?;
        Ok(())
    })
    .await
}

/// Run `f` with git rooted at the linked worktree `dir` (`--git-dir` its own
/// administrative directory, whose back-link must name it; `--work-tree` it), so every
/// filter driver its own configuration defines is found and switched off.
async fn in_worktree<T>(
    dir: &Path,
    f: impl AsyncFnOnce(&Git<'_>) -> Result<T, CheckpointError>,
) -> Result<T, CheckpointError> {
    let sb = tautly_sandbox::LocalSandbox::new(dir.to_path_buf()).map_err(|e| {
        CheckpointError::Disk {
            path: dir.display().to_string(),
            source: e,
        }
    })?;
    with_plain_git(&sb, dir, f).await
}

/// Check out HEAD into the linked worktree `dir` made by [`worktree_add`] (`read-tree
/// --reset -u HEAD`, its own index), in its own configuration's context.
pub(crate) async fn worktree_checkout(dir: &Path) -> Result<(), CheckpointError> {
    in_worktree(dir, async |git| {
        git.run(
            &["read-tree", "--reset", "-u", "HEAD"],
            None,
            Checkpointer::housekeeping_deadline(),
        )
        .await?;
        Ok(())
    })
    .await
}

/// Move the linked worktree `dir` to `commit` (a bisect's next state, spec 7.12.7), in its
/// own configuration's context: `checkout --detach --force <commit>` (what a run changed
/// in tracked files goes), then `clean -f -d` (what a run left untracked goes; ignored
/// files, such as build caches, stay). No hook, filter or fsmonitor runs.
pub(crate) async fn worktree_move(dir: &Path, commit: &str) -> Result<(), CheckpointError> {
    in_worktree(dir, async |git| {
        let deadline = Checkpointer::housekeeping_deadline();
        git.run(
            &["checkout", "-q", "--detach", "--force", commit],
            None,
            deadline,
        )
        .await?;
        git.run(&["clean", "-q", "-f", "-d"], None, deadline)
            .await?;
        Ok(())
    })
    .await
}

/// What the linked worktree `dir` holds besides its HEAD, in its own configuration's
/// context: the paths changed (modified, deleted, untracked) and the ignored files
/// (Tautly's own scratch directory `.tautly/tmp` aside). From `git status --porcelain`.
pub(crate) async fn worktree_status(
    dir: &Path,
) -> Result<(Vec<String>, Vec<String>), CheckpointError> {
    let out = in_worktree(dir, async |git| {
        Ok(git
            .run(
                &[
                    "status",
                    "--porcelain=v1",
                    "-z",
                    "--untracked-files=all",
                    "--ignored=matching",
                    // No child in a submodule: it would read the submodule's own
                    // configuration, whose filters are not switched off (re-review N1).
                    // Populated submodules are refused apart ([`worktree_submodules`]).
                    "--ignore-submodules=all",
                ],
                None,
                Checkpointer::housekeeping_deadline(),
            )
            .await?)
    })
    .await?;
    Ok(parse_status(&out))
}

/// The populated submodules of the linked worktree `dir`: each gitlink of its index
/// (`ls-files -s`, mode 160000) whose path holds a `.git`, as git's own `worktree remove`
/// refuses them (re-review N1). Read in the worktree's context; nothing runs in a submodule.
pub(crate) async fn worktree_submodules(dir: &Path) -> Result<Vec<String>, CheckpointError> {
    let out = in_worktree(dir, async |git| {
        Ok(git
            .run(
                &["ls-files", "-s", "-z"],
                None,
                Checkpointer::housekeeping_deadline(),
            )
            .await?)
    })
    .await?;
    let mut found = Vec::new();
    for entry in out.split(|&b| b == 0).filter(|e| !e.is_empty()) {
        let Some(tab) = entry.iter().position(|&b| b == b'\t') else {
            continue;
        };
        if !entry.starts_with(GITLINK.as_bytes()) {
            continue;
        }
        let path = String::from_utf8_lossy(&entry[tab + 1..]).into_owned();
        if plain_relative(Path::new(&path))
            && std::fs::symlink_metadata(dir.join(&path).join(".git")).is_ok()
        {
            found.push(path);
        }
    }
    Ok(found)
}

/// Whether a branch or tag of the repository at `root` reaches `commit`.
pub(crate) async fn reached_by_a_branch_or_tag(
    sb: &dyn Sandbox,
    root: &Path,
    commit: &str,
) -> Result<bool, CheckpointError> {
    let contains = format!("--contains={commit}");
    with_plain_git(sb, root, async |git| {
        let out = git
            .run(
                &[
                    "for-each-ref",
                    "--count=1",
                    "--format=%(refname)",
                    &contains,
                    "refs/heads",
                    "refs/tags",
                ],
                None,
                Checkpointer::housekeeping_deadline(),
            )
            .await?;
        Ok(!out.iter().all(u8::is_ascii_whitespace))
    })
    .await
}

/// `git status --porcelain=v1 -z` output: the changed and the ignored paths.
pub(crate) fn parse_status(out: &[u8]) -> (Vec<String>, Vec<String>) {
    let (mut changed, mut ignored) = (Vec::new(), Vec::new());
    let mut entries = out.split(|&b| b == 0).filter(|e| !e.is_empty());
    while let Some(entry) = entries.next() {
        if entry.len() < 4 {
            continue;
        }
        let (code, path) = (
            &entry[..2],
            String::from_utf8_lossy(&entry[3..]).into_owned(),
        );
        // A rename or copy names its source next.
        if matches!(code[0], b'R' | b'C') {
            entries.next();
        }
        if code == b"!!" {
            let scratch = SPILL_STORE.to_string() + "/";
            if !path.starts_with(&scratch) && path != scratch {
                ignored.push(path);
            }
        } else {
            changed.push(path);
        }
    }
    (changed, ignored)
}

/// `git worktree remove --force <dir>` on the repository at `root`, through the same
/// hardened git. Always forced: git's own cleanliness check would run `status` in the
/// worktree's context, which may define a filter; [`worktree_status`] is checked first
/// instead (review C1).
pub(crate) async fn worktree_remove(
    sb: &dyn Sandbox,
    root: &Path,
    dir: &Path,
) -> Result<(), CheckpointError> {
    let dir = dir.to_string_lossy();
    with_plain_git(sb, root, async |git| {
        let args = ["worktree", "remove", "--force", "--", &dir];
        git.run(&args, None, Checkpointer::housekeeping_deadline())
            .await?;
        Ok(())
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
