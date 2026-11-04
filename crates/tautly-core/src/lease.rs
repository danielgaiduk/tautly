//! The working-tree lease (spec 7.12.1): every engine holds a shared `flock` on
//! `<data home>/locks/<root hash>.lock` for as long as it runs; a rewind or a branch switch
//! needs it exclusive, so it is refused while another live session, in this process or
//! another, uses the same working tree.
//!
//! `flock` locks belong to an open file: each engine opens the lock file itself, so two
//! engines of one process exclude each other like two processes do. Converting the shared
//! lock to an exclusive one is not atomic (the kernel may drop the shared lock first);
//! when the conversion fails the shared lock is taken again before the refusal returns.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};

use crate::workspace::Workspace;

/// Why the lease cannot be had exclusively.
#[derive(Debug, thiserror::Error)]
pub enum LeaseError {
    #[error(
        "another live session uses this working tree; close it first (rewind and switch \
         change files under it)"
    )]
    Busy,
    #[error("cannot lock the working tree: {0}")]
    Io(#[from] io::Error),
}

/// An engine's shared hold on its working tree.
pub struct Lease {
    file: Arc<File>,
    path: PathBuf,
    /// The workspace whose live sessions also count (sessions of this process that share
    /// it; each holds its own lease too).
    workspace: Option<Weak<Workspace>>,
}

/// The lease held exclusively; shared again when dropped.
pub struct ExclusiveGuard {
    file: Arc<File>,
}

impl Drop for ExclusiveGuard {
    fn drop(&mut self) {
        let _ = flock(&self.file, libc::LOCK_SH);
    }
}

/// `<data home>/locks/<root hash>.lock`.
pub fn lock_path(data_home: &Path, root: &Path) -> PathBuf {
    data_home.join("locks").join(format!(
        "{}.lock",
        tautly_session::paths::project_hash(root)
    ))
}

fn flock(file: &File, op: libc::c_int) -> io::Result<()> {
    // SAFETY: flock on a file descriptor `file` owns; no memory is shared.
    if unsafe { libc::flock(file.as_raw_fd(), op) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

impl Lease {
    /// Open the lock file of `root` under `data_home` (created owner-only, with its
    /// directory) and lock it shared. Waits while a rewind or switch elsewhere holds it
    /// exclusively (only for that rewind's materializing).
    pub fn shared(data_home: &Path, root: &Path) -> io::Result<Lease> {
        let path = lock_path(data_home, root);
        if let Some(dir) = path.parent() {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)?;
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&path)?;
        flock(&file, libc::LOCK_SH)?;
        Ok(Lease {
            file: Arc::new(file),
            path,
            workspace: None,
        })
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
    /// file) uses the working tree.
    pub fn try_exclusive(&self) -> Result<ExclusiveGuard, LeaseError> {
        if let Some(ws) = self.workspace.as_ref().and_then(Weak::upgrade)
            && ws.live_sessions() > 1
        {
            return Err(LeaseError::Busy);
        }
        match flock(&self.file, libc::LOCK_EX | libc::LOCK_NB) {
            Ok(()) => Ok(ExclusiveGuard {
                file: Arc::clone(&self.file),
            }),
            Err(e) => {
                // The conversion may have dropped the shared lock: take it back.
                let _ = flock(&self.file, libc::LOCK_SH);
                if e.raw_os_error() == Some(libc::EWOULDBLOCK) {
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
    use super::*;

    #[test]
    fn exclusive_only_when_alone_and_shared_again_after() {
        let home = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let a = Lease::shared(home.path(), root.path()).unwrap();
        {
            let _g = a.try_exclusive().unwrap();
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
}
