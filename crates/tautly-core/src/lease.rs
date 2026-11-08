//! The working-tree lease (spec 7.12.1): every engine holds a shared `flock` on
//! `<data home>/locks/<root hash>.lock` for as long as it runs; an undo, a rewind or a
//! branch switch needs it exclusive, so it is refused while another live session, in this
//! process or another, uses the same working tree.
//!
//! `flock` locks belong to an open file: each engine opens the lock file itself, so two
//! engines of one process exclude each other like two processes do. Converting the shared
//! lock to an exclusive one is not atomic (the kernel may drop the shared lock first), so
//! upgraders are serialized on a second lock file, `<root hash>.gate`, held exclusively
//! (without waiting) for the whole upgrade, the exclusive hold and the downgrade. Only the
//! gate's holder ever holds the lease exclusively, so taking the shared lock back (after a
//! refused upgrade, or when the guard drops) never waits.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use crate::workspace::Workspace;

/// Why the lease cannot be had exclusively.
#[derive(Debug, thiserror::Error)]
pub enum LeaseError {
    #[error(
        "another live session uses this working tree; close it first (undo, rewind and \
         switch change files under it)"
    )]
    Busy,
    #[error("cannot lock the working tree: {0}")]
    Io(#[from] io::Error),
}

/// An engine's shared hold on its working tree.
pub struct Lease {
    file: Arc<File>,
    gate: PathBuf,
    path: PathBuf,
    /// The workspace whose live sessions also count (sessions of this process that share
    /// it; each holds its own lease too).
    workspace: Option<Weak<Workspace>>,
}

/// The lease held exclusively; shared again when dropped, then the gate is let go.
pub struct ExclusiveGuard {
    file: Arc<File>,
    /// Dropped after the downgrade.
    _gate: File,
}

impl Drop for ExclusiveGuard {
    fn drop(&mut self) {
        // No other holder can hold it exclusively (that needs the gate): never waits.
        let _ = flock(&self.file, libc::LOCK_SH);
        // Let go of the gate explicitly: a child forked by any thread of the process
        // holds a copy of this descriptor until it execs, and only closing ours would
        // leave the gate locked through it (Task 50 M1). The lock belongs to the open
        // file description, which the copy shares, so this frees it for every copy.
        let _ = flock(&self._gate, libc::LOCK_UN);
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        // As for the gate: a forked copy of the descriptor must not keep the working tree
        // leased once this engine is gone. An exclusive guard (which shares the file) never
        // outlives the lease it came from.
        let _ = flock(&self.file, libc::LOCK_UN);
    }
}

/// `<data home>/locks/<root hash>.lock`.
pub fn lock_path(data_home: &Path, root: &Path) -> PathBuf {
    data_home.join("locks").join(format!(
        "{}.lock",
        tautly_session::paths::project_hash(root)
    ))
}

pub(crate) use tautly_session::files::flock;

fn would_block(e: &io::Error) -> bool {
    e.raw_os_error() == Some(libc::EWOULDBLOCK)
}

/// The lock file at `path`, created owner-only with its directory.
pub(crate) fn open(path: &Path) -> io::Result<File> {
    if let Some(dir) = path.parent() {
        tautly_session::paths::create_private_dir_all(dir)?;
    }
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
}

impl Lease {
    /// Open the lock file of `root` under `data_home` and lock it shared without waiting:
    /// `Ok(None)` while a rewind or switch elsewhere holds it exclusively (for its
    /// materializing only; see [`shared`](Self::shared)).
    pub fn try_shared(data_home: &Path, root: &Path) -> io::Result<Option<Lease>> {
        let path = lock_path(data_home, root);
        let file = open(&path)?;
        match flock(&file, libc::LOCK_SH | libc::LOCK_NB) {
            Ok(()) => Ok(Some(Lease {
                file: Arc::new(file),
                gate: path.with_extension("gate"),
                path,
                workspace: None,
            })),
            Err(e) if would_block(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// [`try_shared`](Self::try_shared), tried again every 20 ms for up to `wait`: an
    /// error ([`io::ErrorKind::WouldBlock`]) when the exclusive holder does not let go in
    /// time. Blocking: call it off the async workers.
    pub fn shared_within(data_home: &Path, root: &Path, wait: Duration) -> io::Result<Lease> {
        let until = Instant::now() + wait;
        loop {
            if let Some(lease) = Lease::try_shared(data_home, root)? {
                return Ok(lease);
            }
            if Instant::now() >= until {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "a rewind or switch elsewhere holds the working tree",
                ));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// [`shared_within`](Self::shared_within) 10 s.
    pub fn shared(data_home: &Path, root: &Path) -> io::Result<Lease> {
        Lease::shared_within(data_home, root, Duration::from_secs(10))
    }

    /// Also refuse the exclusive lease while another session of this process shares
    /// `workspace` ([`Workspace::live_sessions`]).
    pub(crate) fn for_workspace(mut self, workspace: &Arc<Workspace>) -> Lease {
        self.workspace = Some(Arc::downgrade(workspace));
        self
    }

    /// The lock file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The lease, exclusively, without waiting: [`LeaseError::Busy`] while another live
    /// session (an engine of this process sharing the workspace, or any holder of the lock
    /// file) uses the working tree, or another upgrade is under way.
    pub fn try_exclusive(&self) -> Result<ExclusiveGuard, LeaseError> {
        if let Some(ws) = self.workspace.as_ref().and_then(Weak::upgrade)
            && ws.live_sessions() > 1
        {
            return Err(LeaseError::Busy);
        }
        let gate = open(&self.gate)?;
        match flock(&gate, libc::LOCK_EX | libc::LOCK_NB) {
            Ok(()) => {}
            Err(e) if would_block(&e) => return Err(LeaseError::Busy),
            Err(e) => return Err(e.into()),
        }
        match flock(&self.file, libc::LOCK_EX | libc::LOCK_NB) {
            Ok(()) => Ok(ExclusiveGuard {
                file: Arc::clone(&self.file),
                _gate: gate,
            }),
            Err(e) => {
                // The conversion may have dropped the shared lock: take it back (no one
                // else can hold it exclusively while the gate is ours).
                let _ = flock(&self.file, libc::LOCK_SH);
                let _ = flock(&gate, libc::LOCK_UN);
                if would_block(&e) {
                    Err(LeaseError::Busy)
                } else {
                    Err(LeaseError::Io(e))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::fd::AsRawFd;

    use super::*;

    #[test]
    fn exclusive_only_when_alone_and_shared_again_after() {
        let home = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let a = Lease::shared(home.path(), root.path()).unwrap();
        {
            let _g = a.try_exclusive().unwrap();
            // While it is held, a new engine does not get in (nor wait long).
            assert!(
                Lease::try_shared(home.path(), root.path())
                    .unwrap()
                    .is_none()
            );
            let err = Lease::shared_within(home.path(), root.path(), Duration::from_millis(50));
            assert_eq!(err.err().map(|e| e.kind()), Some(io::ErrorKind::WouldBlock));
        }
        let b = Lease::shared(home.path(), root.path()).unwrap();
        assert!(matches!(a.try_exclusive(), Err(LeaseError::Busy)));
        assert!(matches!(b.try_exclusive(), Err(LeaseError::Busy)));
        drop(b);
        let g = a.try_exclusive().unwrap();
        drop(g);
        // Shared again: another engine can start.
        let c = Lease::shared(home.path(), root.path()).unwrap();
        assert!(matches!(c.try_exclusive(), Err(LeaseError::Busy)));
    }

    #[test]
    fn a_second_upgrade_is_refused_while_one_runs() {
        let home = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let a = Lease::shared(home.path(), root.path()).unwrap();
        let g = a.try_exclusive().unwrap();
        // Someone else holding the gate (an upgrade in flight) refuses ours.
        drop(g);
        let gate = open(&a.gate).unwrap();
        flock(&gate, libc::LOCK_EX).unwrap();
        assert!(matches!(a.try_exclusive(), Err(LeaseError::Busy)));
        // Unlocked before the close, as the guard does: a child another test thread forks
        // now holds a copy of the descriptor, which would keep a plain close's lock.
        flock(&gate, libc::LOCK_UN).unwrap();
        drop(gate);
        assert!(a.try_exclusive().is_ok());
    }

    /// A copy of `file`'s descriptor (as a forked child holds one until it execs): the same
    /// open file description, so the same `flock` lock.
    fn copy_of(file: &File) -> std::os::fd::OwnedFd {
        use std::os::fd::FromRawFd;
        // SAFETY: dup of a descriptor we own; the new one is owned by the result.
        let fd = unsafe { libc::dup(file.as_raw_fd()) };
        assert!(fd >= 0);
        unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) }
    }

    #[test]
    fn a_gate_let_go_is_free_while_a_forked_copy_lives() {
        // Task 50 M1: a child forked by any thread of the process carries every open file
        // until it execs; closing ours must not leave the gate locked through that copy.
        let home = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let a = Lease::shared(home.path(), root.path()).unwrap();
        let g = a.try_exclusive().unwrap();
        let copy = copy_of(&g._gate);
        drop(g);
        assert!(a.try_exclusive().is_ok());
        drop(copy);
    }

    #[test]
    fn a_lease_dropped_is_free_while_a_forked_copy_lives() {
        let home = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let gone = Lease::shared(home.path(), root.path()).unwrap();
        let copy = copy_of(&gone.file);
        drop(gone);
        let b = Lease::shared(home.path(), root.path()).unwrap();
        assert!(b.try_exclusive().is_ok());
        drop(copy);
    }
}
