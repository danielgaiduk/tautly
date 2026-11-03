//! Linux only: the engine process is a child subreaper (`PR_SET_CHILD_SUBREAPER`), so a
//! command's descendant that left its process group (`setsid`, a double fork) and lost its
//! parent is re-parented to us instead of to init. Such adopted orphans are reaped when they
//! die, and ended when the last session of the process shuts down. Elsewhere these are
//! no-ops; what escapes is documented in `tautly_tools::procs`.
//!
//! Our own children (the shells of running commands, waited for by the runtime) are never
//! touched: they are the leaders in [`tautly_tools::running_leaders`].

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// Sessions alive in this process.
static SESSIONS: AtomicUsize = AtomicUsize::new(0);

/// A session starts: become a subreaper (once).
pub(crate) fn session_started() {
    if SESSIONS.fetch_add(1, Ordering::SeqCst) == 0 {
        imp::enable();
    }
}

/// A session has shut down. The last one ends the orphans the process adopted.
pub(crate) async fn session_ended(grace: Duration) {
    if SESSIONS.fetch_sub(1, Ordering::SeqCst) == 1 {
        imp::end_orphans(grace).await;
    }
    imp::reap();
}

/// Reap adopted orphans that have died, so they do not pile up as zombies.
pub(crate) fn reap() {
    imp::reap();
}

#[cfg(target_os = "linux")]
mod imp {
    use std::time::Duration;

    /// Become a child subreaper; failure (an old kernel) only loses the adoption.
    pub fn enable() {
        // SAFETY: prctl with PR_SET_CHILD_SUBREAPER takes plain integers.
        unsafe {
            libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0);
        }
    }

    /// `(pid, zombie)` of every child of this process that is not a running command's shell.
    fn adopted() -> Vec<(i32, bool)> {
        // SAFETY: getpid has no preconditions.
        let me = unsafe { libc::getpid() };
        let ours: Vec<u32> = tautly_tools::running_leaders();
        let Ok(dir) = std::fs::read_dir("/proc") else {
            return Vec::new();
        };
        dir.filter_map(|e| {
            let e = e.ok()?;
            let pid: i32 = e.file_name().to_str()?.parse().ok()?;
            let stat = std::fs::read_to_string(e.path().join("stat")).ok()?;
            // `pid (comm) state ppid ...`; comm may hold spaces and parentheses.
            let rest = &stat[stat.rfind(')')? + 1..];
            let mut fields = rest.split_whitespace();
            let state = fields.next()?;
            let ppid: i32 = fields.next()?.parse().ok()?;
            let ours = u32::try_from(pid).is_ok_and(|p| ours.contains(&p));
            (ppid == me && !ours).then_some((pid, state == "Z"))
        })
        .collect()
    }

    pub fn reap() {
        for (pid, zombie) in adopted() {
            if zombie {
                // SAFETY: waits for one specific child that has exited.
                unsafe {
                    libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG);
                }
            }
        }
    }

    pub async fn end_orphans(grace: Duration) {
        let live: Vec<i32> = adopted()
            .into_iter()
            .filter(|&(_, z)| !z)
            .map(|(p, _)| p)
            .collect();
        if live.is_empty() {
            return;
        }
        // SAFETY (kill): plain signals to our own children.
        for &p in &live {
            unsafe {
                libc::kill(p, libc::SIGTERM);
            }
        }
        let end = tokio::time::Instant::now() + grace;
        while tokio::time::Instant::now() < end && adopted().iter().any(|&(_, z)| !z) {
            reap();
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        for (p, zombie) in adopted() {
            if !zombie {
                unsafe {
                    libc::kill(p, libc::SIGKILL);
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
        reap();
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    use std::time::Duration;

    pub fn enable() {}

    pub fn reap() {}

    pub async fn end_orphans(_grace: Duration) {}
}
