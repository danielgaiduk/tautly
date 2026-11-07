//! Materializing a [`FileState`] in place of another (spec 7.12.2): see the module docs
//! of [`crate::timetravel`].

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tautly_sandbox::Sandbox;
use tautly_session::{BlobRef, RelPath, SessionReader};

use super::{FileState, TtError};
use crate::checkpoint::{self, Checkpointer};
use crate::record::{BackupFile, RestoreBy, RestoreOutcome, SessionRecord, Wrote};
use crate::undo::{Disk, MAX_DISK_READ, UndoLog, read_disk};

/// Why a path is blocked (never written).
const WHY_LINK: &str = "a symbolic link or a file is above it (a write would land elsewhere)";
const WHY_DIR: &str = "a directory is in the way";
const WHY_UNKEPT: &str =
    "the git snapshot cannot hold it and its content cannot be kept (a link, or over 64 MiB)";

/// The permission bits a new file gets here (`0o666` less the umask), as the sandbox's
/// `write_atomic` creates one: found by making a file the same way in the temporary
/// directory (the umask is the process's; reading it with `umask(2)` would change it for
/// every thread meanwhile). None when that cannot be done.
fn new_file_mode() -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    let probe = tempfile::Builder::new()
        .prefix("tautly-mode-")
        .permissions(std::fs::Permissions::from_mode(0o666))
        .tempfile()
        .ok()?;
    let mode = probe.as_file().metadata().ok()?.permissions().mode() & 0o7777;
    Some(mode)
}

/// The note of a materialize with git checkpoints.
const NOTE_GIT: &str = "ignored files and submodules are not restored";

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
    /// edit, also one the agent's later checkpoints absorbed): nothing is written unless
    /// each is listed in `overwrite`.
    pub conflicts: Vec<String>,
    /// Paths that cannot be written whatever `overwrite` says: a directory, a symbolic
    /// link or a file is in the way (at the path or above it). Nothing is written while
    /// any is left.
    pub blocked: Vec<String>,
    pub dry_run: bool,
    /// The files were made the target (nothing was left to write counts too).
    pub applied: bool,
    /// Where the content the write replaces is (or, for a dry run, would be) kept:
    /// `git` (a snapshot ref, `backup_ref`) or `log` (the session log's blob store, in a
    /// `RestoreBegun` record). None: not kept (outside git, a file over 64 MiB or a link
    /// would be overwritten, or no log to keep it in).
    pub backup: Option<String>,
    /// The snapshot taken before the first write.
    pub backup_ref: Option<String>,
    /// What was not restored, or why the state may not be exact.
    pub note: Option<String>,
}

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

/// How to put the files back if a later step fails: the backup snapshot's commit (with
/// git), then each path it does not hold (outside git: every path written; in git: the
/// ignored ones) with its content before.
pub(crate) struct Rollback {
    git: Option<String>,
    files: Vec<(PathBuf, Disk)>,
}

/// What [`materialize_inner`] did.
pub(crate) struct Applied {
    pub report: MaterializeReport,
    /// Set when it was applied (with nothing to put back when nothing was written).
    pub rollback: Option<Rollback>,
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

    /// A known content (`None`: absent) as compared.
    async fn val(&mut self, log: &Path, v: &Option<BlobRef>) -> Result<Val, TtError> {
        Ok(match v {
            Some(b) => Val::Blob {
                id: self.id(log, b).await?,
                mode: None,
            },
            None => Val::Absent,
        })
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

/// Pairs of `(root-relative path, content)` by path, each checked to stay in the root.
fn by_path(
    root: &Path,
    list: &[(RelPath, Option<BlobRef>)],
) -> Result<HashMap<PathBuf, Option<BlobRef>>, TtError> {
    let mut out = HashMap::new();
    for (p, b) in list {
        p.resolve(root)?;
        out.insert(PathBuf::from(p.as_str()), b.clone());
    }
    Ok(out)
}

/// Why `rel`'s parent chain cannot be written through: a component above it is a symbolic
/// link or not a directory (a write would land elsewhere, or git would replace it). Missing
/// components are fine (they are created).
pub(crate) fn parent_in_the_way(root: &Path, rel: &Path) -> bool {
    let mut at = root.to_path_buf();
    let Some(parent) = rel.parent() else {
        return false;
    };
    for name in parent.components() {
        at.push(name);
        match std::fs::symlink_metadata(&at) {
            Ok(m) if m.file_type().is_symlink() || !m.is_dir() => return true,
            Ok(_) => {}
            Err(_) => return false,
        }
    }
    false
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

/// The paths a human changed (spec 7.12.1): in each pair of consecutive checkpoints with
/// no command between them, a path that differs and whose new content no `Edit` or
/// `Undone` between them wrote. The last pair that changed a path decides (a `restored`
/// checkpoint, which a rewind or switch wrote, gives it back to the agent). With `after`,
/// only pairs whose later checkpoint lies after that offset count.
async fn human_changed(
    cp: &Checkpointer,
    blobs: &mut Blobs<'_>,
    expected: &FileState,
    after: Option<u64>,
) -> Result<BTreeSet<PathBuf>, TtError> {
    let deadline = Checkpointer::housekeeping_deadline();
    let mut owner: HashMap<PathBuf, bool> = HashMap::new();
    for pair in &expected.pairs {
        if after.is_some_and(|a| pair.offset <= a) {
            continue;
        }
        let explained: HashMap<&str, &Option<Option<BlobRef>>> = pair
            .explained
            .iter()
            .map(|(p, v)| (p.as_str(), v))
            .collect();
        for ch in cp.diff(&pair.from, &pair.to, deadline).await? {
            if ch.is_gitlink() {
                continue;
            }
            let agent = pair.restore
                || match explained.get(ch.path.to_string_lossy().as_ref()) {
                    None => false,
                    // Written by a tool whose result was not captured.
                    Some(None) => true,
                    Some(Some(v)) => {
                        let wrote = blobs.val(&expected.log, v).await?;
                        let now = match &ch.new {
                            Some((mode, id)) => Val::Blob {
                                id: id.clone(),
                                mode: Some(mode.clone()),
                            },
                            None => Val::Absent,
                        };
                        same(&wrote, &now)
                    }
                };
            owner.insert(ch.path, !agent);
        }
    }
    Ok(owner
        .into_iter()
        .filter(|(_, human)| *human)
        .map(|(p, _)| p)
        .collect())
}

/// One state's view of the paths: its overlay, its base's entries, and the contents it
/// falls back on for a path neither base has (an ignored file a tool wrote).
struct View<'s> {
    state: &'s FileState,
    overlay: HashMap<PathBuf, Option<BlobRef>>,
    entries: Option<HashMap<PathBuf, (String, String)>>,
    fallback: HashMap<PathBuf, Option<BlobRef>>,
    /// A path neither base has and the fallback lacks is not known (else: absent).
    strict: bool,
}

impl View<'_> {
    /// The content at `rel` and where it comes from; none when the state does not know
    /// it. `in_neither`: neither state's base has the path.
    async fn value(
        &self,
        blobs: &mut Blobs<'_>,
        rel: &Path,
        in_neither: bool,
    ) -> Result<Option<(Val, Src)>, TtError> {
        let blob = |v: &Option<BlobRef>| match v {
            Some(b) => Src::Blob(b.clone()),
            None => Src::Absent,
        };
        if let Some(v) = self.overlay.get(rel) {
            return Ok(Some((blobs.val(&self.state.log, v).await?, blob(v))));
        }
        let Some(entries) = &self.entries else {
            return Ok(None);
        };
        if let Some((mode, id)) = entries.get(rel) {
            let val = Val::Blob {
                id: id.clone(),
                mode: Some(mode.clone()),
            };
            return Ok(Some((val, Src::Base)));
        }
        if in_neither {
            match self.fallback.get(rel) {
                Some(v) => return Ok(Some((blobs.val(&self.state.log, v).await?, blob(v)))),
                None if self.strict => return Ok(None),
                None => {}
            }
        }
        Ok(Some((Val::Absent, Src::Absent)))
    }
}

/// Make the work tree `target` where it differs from `expected` (spec 7.12.2): see the
/// module docs. `cp` is the work tree's checkpointer (none outside git or with checkpoints
/// off: then only overlays count, and command effects are not restored). With `dry_run`,
/// or while conflicts remain that `overwrite` does not list (or blocked paths), nothing is
/// written. Without git no backup is kept (the engine's restores keep one in the session
/// log).
pub async fn materialize(
    sb: &dyn Sandbox,
    cp: Option<&mut Checkpointer>,
    target: &FileState,
    expected: &FileState,
    overwrite: &[RelPath],
    dry_run: bool,
) -> Result<MaterializeReport, TtError> {
    Ok(
        materialize_inner(sb, cp, target, expected, overwrite, dry_run, None)
            .await?
            .report,
    )
}

/// [`materialize`], logging to `log` (the session the restore leaves, when there is one,
/// and what restores):
/// a `RestoreBegun` before the first write (with each file's content before, outside git),
/// and a `RestoreEnded` when a write fails. Returns how to put the files back.
pub(crate) async fn materialize_inner(
    sb: &dyn Sandbox,
    mut cp: Option<&mut Checkpointer>,
    target: &FileState,
    expected: &FileState,
    overwrite: &[RelPath],
    dry_run: bool,
    log: Option<(&mut dyn UndoLog, RestoreBy)>,
) -> Result<Applied, TtError> {
    let (mut log, by) = match log {
        Some((log, by)) => (Some(log), by),
        None => (None, RestoreBy::Unknown),
    };
    let root = sb.root().to_path_buf();
    let deadline = Checkpointer::housekeeping_deadline();
    let overwrite: Vec<PathBuf> = overwrite
        .iter()
        .map(|p| p.resolve(&root).map(|_| PathBuf::from(p.as_str())))
        .collect::<Result<_, _>>()?;
    let mut notes: Vec<String> = Vec::new();
    match cp.as_deref() {
        Some(_) => notes.push(NOTE_GIT.to_string()),
        None => notes.push(format!(
            "only tool edits are restored; command effects are not ({})",
            if std::fs::symlink_metadata(root.join(".git")).is_ok() {
                "git checkpoints are off"
            } else {
                "not a git repository"
            }
        )),
    }
    notes.extend(target.partial.clone());
    let (tbase, ebase) = match cp.as_deref() {
        Some(_) => (target.base.clone(), expected.base.clone()),
        None => (None, None),
    };
    let same_log = target.log == expected.log;
    let t_over = by_path(&root, &target.overlay)?;
    let e_over = by_path(&root, &expected.overlay)?;
    // What an ignored file goes back to: for a rewind, its earliest content after the
    // target's checkpoint; for a switch, the last content the target's tools left in it.
    let later = if same_log {
        by_path(&root, &target.later)?
    } else {
        by_path(&root, &target.written)?
    };

    // The paths that may change.
    let mut cands: BTreeSet<PathBuf> = t_over
        .keys()
        .chain(e_over.keys())
        .chain(later.keys())
        .cloned()
        .collect();
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
        fallback: later,
        strict: !same_log,
    };
    let eview = View {
        state: expected,
        overlay: e_over,
        entries: base_entries(cp.as_deref(), ebase.as_deref(), &list).await?,
        fallback: by_path(&root, &expected.written)?,
        strict: false,
    };
    let in_base = |v: &View, p: &Path| v.entries.as_ref().is_some_and(|e| e.contains_key(p));

    let mut blobs = Blobs {
        cp: cp.as_deref(),
        readers: HashMap::new(),
        ids: HashMap::new(),
    };
    let human = match cp.as_deref() {
        Some(c) => {
            let after = if same_log { target.after } else { None };
            human_changed(c, &mut blobs, expected, after).await?
        }
        None => BTreeSet::new(),
    };
    let mut planned = Vec::new();
    let mut conflicts = Vec::new();
    let mut blocked = Vec::new();
    for rel in cands {
        let neither = !in_base(&tview, &rel) && !in_base(&eview, &rel);
        let Some((want, src)) = tview.value(&mut blobs, &rel, neither).await? else {
            continue;
        };
        let exp = eview
            .value(&mut blobs, &rel, neither)
            .await?
            .map(|(v, _)| v);
        if exp.as_ref().is_some_and(|e| same(&want, e)) {
            continue;
        }
        if parent_in_the_way(&root, &rel) {
            // Nothing below a link or a file is this tree's; writing there would land
            // elsewhere (or replace the link).
            if want != Val::Absent {
                blocked.push((rel, WHY_LINK));
            }
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
                blocked.push((rel, WHY_DIR));
            }
            continue;
        }
        if exp.is_none_or(|e| !same(&e, &now)) || human.contains(&rel) {
            conflicts.push(rel.clone());
        }
        planned.push(Planned { rel, src, disk });
    }
    drop(blobs);
    let text = |p: &PathBuf| p.to_string_lossy().into_owned();
    let git = cp.is_some();
    // The paths a git snapshot of the tree now would not hold (ignored files): their
    // content before is kept in the session log instead, or they are not written at all.
    let tree = match cp.as_deref() {
        Some(c) if !planned.is_empty() => Some(c.tree_id(true, deadline).await?),
        _ => None,
    };
    let held: BTreeSet<PathBuf> = match (cp.as_deref(), &tree) {
        (Some(c), Some(tree)) => {
            let present: Vec<PathBuf> = planned
                .iter()
                .filter(|p| p.disk != Disk::Absent)
                .map(|p| p.rel.clone())
                .collect();
            c.entries(tree, &present, deadline)
                .await?
                .into_keys()
                .collect()
        }
        _ => BTreeSet::new(),
    };
    let storable = |d: &Disk| matches!(d, Disk::Absent | Disk::File { .. });
    let mut kept_in_log = Vec::new();
    let mut i = 0;
    while i < planned.len() {
        if held.contains(&planned[i].rel) {
            i += 1;
            continue;
        }
        // With git, a path the snapshot cannot restore is never overwritten unbacked.
        if git && (log.is_none() || !storable(&planned[i].disk)) {
            let p = planned.remove(i);
            conflicts.retain(|c| *c != p.rel);
            blocked.push((p.rel, WHY_UNKEPT));
            continue;
        }
        kept_in_log.push(i);
        i += 1;
    }
    blocked.sort();
    for why in [WHY_LINK, WHY_DIR, WHY_UNKEPT] {
        let names: Vec<String> = blocked
            .iter()
            .filter(|(_, w)| *w == why)
            .map(|(p, _)| text(p))
            .collect();
        if !names.is_empty() {
            notes.push(format!("blocked, {why}: {}", names.join(", ")));
        }
    }
    let unlisted = conflicts.iter().any(|c| !overwrite.contains(c)) || !blocked.is_empty();
    let backup = match (cp.as_deref(), &log) {
        _ if planned.is_empty() => None,
        (Some(_), _) => Some("git"),
        (None, Some(_)) if planned.iter().all(|p| storable(&p.disk)) => Some("log"),
        (None, _) => None,
    };
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
        conflicts: conflicts.iter().map(text).collect(),
        blocked: blocked.iter().map(|(p, _)| text(p)).collect(),
        dry_run,
        applied: false,
        backup: backup.map(str::to_string),
        backup_ref: None,
        note: None,
    };
    if !planned.is_empty() && backup.is_none() {
        notes.push("no backup is kept of what this overwrites".to_string());
    }
    report.note = (!notes.is_empty()).then(|| notes.join("; "));
    if dry_run || unlisted {
        return Ok(Applied {
            report,
            rollback: None,
        });
    }

    // The backup, before the first write.
    let mut rollback = Rollback {
        git: None,
        files: kept_in_log
            .iter()
            .map(|&i| (planned[i].rel.clone(), planned[i].disk.clone()))
            .collect(),
    };
    if let (Some(c), Some(tree)) = (cp.as_deref_mut(), &tree) {
        let made = c.commit_backup(tree, deadline).await?;
        report.backup_ref = Some(made.refname);
        rollback.git = Some(made.commit);
    }
    if let Some(log) = log.as_deref_mut()
        && !planned.is_empty()
    {
        let mut files = Vec::with_capacity(kept_in_log.len());
        let new_mode = new_file_mode();
        for &i in &kept_in_log {
            let p = &planned[i];
            let path = RelPath::from_abs(&root, &root.join(&p.rel))
                .ok_or_else(|| TtError::BadPath(text(&p.rel)))?;
            let (existed, blob, mode) = match &p.disk {
                Disk::Absent => (false, None, None),
                Disk::File { bytes, mode } => (true, Some(log.blob(bytes)?), Some(*mode)),
                _ => (true, None, None),
            };
            let wrote = match &p.src {
                Src::Blob(b) => Some(Wrote::Blob(b.clone())),
                Src::Absent => Some(Wrote::Absent),
                Src::Base => None,
            };
            // A blob is written over a file with that file's mode, else as a new file with
            // the default mode, the umask respected (see `write`; Task 50 M6, N4).
            let wrote_mode = match (&p.src, &p.disk) {
                (Src::Blob(_), Disk::File { mode, .. }) => Some(*mode),
                (Src::Blob(_), _) => new_mode,
                _ => None,
            };
            files.push(BackupFile {
                path,
                existed,
                from_blob: blob,
                from_mode: mode,
                to: wrote,
                to_mode: wrote_mode,
            });
        }
        let paths = planned
            .iter()
            .map(|p| {
                RelPath::from_abs(&root, &root.join(&p.rel))
                    .ok_or_else(|| TtError::BadPath(text(&p.rel)))
            })
            .collect::<Result<_, _>>()?;
        log.record(&SessionRecord::RestoreBegun {
            backup_ref: report.backup_ref.clone(),
            files,
            paths,
            by,
        })?;
    }
    let written = write(sb, cp.as_deref(), target, tbase.as_deref(), &planned).await;
    if let Some(c) = cp.as_deref_mut() {
        c.invalidate();
    }
    if let Err(e) = written {
        return Err(fail(sb, cp.as_deref(), log, &rollback, report.backup_ref, e).await);
    }
    report.applied = true;
    Ok(Applied {
        report,
        rollback: Some(rollback),
    })
}

/// A failure after files were written: put them back, log how it ended, and say both.
pub(crate) async fn fail(
    sb: &dyn Sandbox,
    cp: Option<&Checkpointer>,
    log: Option<&mut dyn UndoLog>,
    rollback: &Rollback,
    backup_ref: Option<String>,
    source: TtError,
) -> TtError {
    let rolled_back = roll_back(sb, cp, rollback).await.map_err(|e| e.to_string());
    if let Some(log) = log {
        // Best effort: the error below says what happened either way.
        let _ = log.record(&SessionRecord::RestoreEnded {
            outcome: match rolled_back {
                Ok(()) => RestoreOutcome::RolledBack,
                Err(_) => RestoreOutcome::Failed,
            },
        });
    }
    TtError::Failed {
        source: Box::new(source),
        backup_ref,
        rolled_back,
    }
}

/// Refuse to write `rel` unless the sandbox resolves it to itself (no link on the way).
pub(crate) fn confined(sb: &dyn Sandbox, root: &Path, rel: &Path) -> Result<PathBuf, TtError> {
    let full = root.join(rel);
    let refuse = || TtError::Other(format!("{} no longer resolves to itself", rel.display()));
    if parent_in_the_way(root, rel) {
        return Err(refuse());
    }
    match sb.resolve(&full) {
        Ok(r) if r == full => Ok(full),
        _ => Err(refuse()),
    }
}

/// Write the planned paths: deletions (links unlinked, never followed), the base's
/// contents through git, the overlay's blobs through the sandbox; each path checked to
/// resolve to itself just before.
async fn write(
    sb: &dyn Sandbox,
    cp: Option<&Checkpointer>,
    target: &FileState,
    base: Option<&str>,
    planned: &[Planned],
) -> Result<(), TtError> {
    let root = sb.root().to_path_buf();
    // A link at a path goes first (unlinked), so nothing is written through it.
    for p in planned {
        if matches!(p.src, Src::Absent) || matches!(p.disk, Disk::Link(_)) {
            if parent_in_the_way(&root, &p.rel) {
                return Err(TtError::Other(format!(
                    "{} no longer resolves to itself",
                    p.rel.display()
                )));
            }
            checkpoint::remove(sb, &root.join(&p.rel)).await?;
        }
    }
    let from_base: Vec<PathBuf> = planned
        .iter()
        .filter(|p| matches!(p.src, Src::Base))
        .map(|p| p.rel.clone())
        .collect();
    if let (Some(c), Some(b), false) = (cp, base, from_base.is_empty()) {
        for rel in &from_base {
            confined(sb, &root, rel)?;
        }
        c.checkout(b, &from_base, Checkpointer::housekeeping_deadline())
            .await?;
    }
    let mut reader = None;
    for p in planned {
        let Src::Blob(b) = &p.src else {
            continue;
        };
        let full = confined(sb, &root, &p.rel)?;
        if reader.is_none() {
            reader = Some(SessionReader::<SessionRecord>::open(&target.log)?);
        }
        let bytes = reader.as_ref().expect("opened").read_blob(b)?;
        // Over a file with its mode, a new one with the default mode (`new_file_mode`).
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
    match (&r.git, cp) {
        (Some(commit), Some(cp)) => {
            cp.restore(commit, false).await?;
        }
        (Some(_), None) => {
            return Err(TtError::Other("no checkpointer to restore with".into()));
        }
        (None, _) => {}
    }
    let mut lost = Vec::new();
    for (rel, disk) in &r.files {
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
            Disk::Large { .. } | Disk::Link(_) | Disk::Other => {
                lost.push(rel.display().to_string());
            }
        }
    }
    if !lost.is_empty() {
        return Err(TtError::Other(format!(
            "not kept, so not put back: {}",
            lost.join(", ")
        )));
    }
    Ok(())
}
