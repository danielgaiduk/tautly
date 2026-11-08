//! Low-level file helpers the crates above share: advisory locks and owner-only atomic
//! writes. Owner-only directories: [`crate::paths::create_private_dir_all`].

use std::fs::{self, File};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::paths::create_private_dir_all;

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// `flock(2)` with `op` on `file`.
pub fn flock(file: &File, op: libc::c_int) -> io::Result<()> {
    // SAFETY: flock on a file descriptor `file` owns; no memory is shared.
    if unsafe { libc::flock(file.as_raw_fd(), op) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Take an exclusive, non-blocking advisory lock held until `file` is dropped; `false` when
/// it is still busy after a few tries over ~50 ms. A reader that only looks (the session
/// tree's liveness probe holds a shared lock for an instant) must not make a lock fail.
pub fn lock_exclusive_retrying(file: &File) -> io::Result<bool> {
    for attempt in 0..5 {
        match flock(file, libc::LOCK_EX | libc::LOCK_NB) {
            Ok(()) => return Ok(true),
            Err(e) if e.raw_os_error() == Some(libc::EWOULDBLOCK) => {}
            Err(e) => return Err(e),
        }
        if attempt < 4 {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
    Ok(false)
}

/// Replace `dest` with `bytes` through a temporary file in its directory (created 0700 when
/// missing), mode 0600; with `sync`, the data is on disk before the rename.
pub fn write_private_atomic(dest: &Path, bytes: &[u8], sync: bool) -> io::Result<()> {
    let dir = dest.parent().expect("a file path has a parent");
    create_private_dir_all(dir)?;
    let tmp = dir.join(format!(
        ".tmp-{}-{}",
        std::process::id(),
        TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(bytes)?;
        if sync {
            f.sync_data()?;
        }
        fs::rename(&tmp, dest)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}
