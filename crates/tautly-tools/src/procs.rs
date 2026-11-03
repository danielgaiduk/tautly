//! The process groups a session's commands started (spec 8: shutdown terminates every
//! spawned process group).
//!
//! `bash` runs each command in a process group of its own and registers it here while the
//! command runs. A group that still has members when the command returns (a background
//! child such as a dev server) stays registered, so the engine can end it at shutdown
//! ([`ProcessGroups::terminate_all`]); a group that emptied is released at once. Groups
//! that have died since are pruned whenever one is registered or listed, so the registry
//! stays as small as the set of live background groups.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How often [`ProcessGroups::terminate_all`] looks whether the groups are gone.
const POLL: Duration = Duration::from_millis(5);

/// A shared set of process group ids. Clones share the set.
#[derive(Debug, Clone, Default)]
pub struct ProcessGroups {
    live: Arc<Mutex<BTreeSet<u32>>>,
}

impl ProcessGroups {
    /// Track `pgid` (pruning groups that have died).
    pub fn register(&self, pgid: u32) {
        let mut live = self.lock();
        live.retain(|&g| group_alive(g));
        live.insert(pgid);
    }

    /// Stop tracking `pgid`.
    pub fn release(&self, pgid: u32) {
        self.lock().remove(&pgid);
    }

    /// The groups that still have members, in ascending order (pruning the others).
    pub fn live(&self) -> Vec<u32> {
        let mut live = self.lock();
        live.retain(|&g| group_alive(g));
        live.iter().copied().collect()
    }

    /// End every tracked group: SIGTERM, up to `grace` for them to go, then SIGKILL for
    /// what is left. Returns how many groups were signalled; the set is empty afterwards.
    pub async fn terminate_all(&self, grace: Duration) -> usize {
        let groups = self.live();
        for &g in &groups {
            killpg(g, libc::SIGTERM);
        }
        let end = tokio::time::Instant::now() + grace;
        while groups.iter().any(|&g| group_alive(g)) && tokio::time::Instant::now() < end {
            tokio::time::sleep(POLL).await;
        }
        for &g in &groups {
            if group_alive(g) {
                killpg(g, libc::SIGKILL);
            }
        }
        let mut live = self.lock();
        for g in &groups {
            live.remove(g);
        }
        groups.len()
    }

    /// SIGKILL every tracked group now (no grace: for paths that cannot wait, such as a
    /// runtime being dropped).
    pub fn kill_all_now(&self) {
        let mut live = self.lock();
        for &g in live.iter() {
            if group_alive(g) {
                killpg(g, libc::SIGKILL);
            }
        }
        live.clear();
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeSet<u32>> {
        self.live.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Whether process group `pgid` still has a member this process may signal. A group
/// whose only member is an unreaped zombie still counts.
pub fn group_alive(pgid: u32) -> bool {
    let Ok(pgid) = libc::pid_t::try_from(pgid) else {
        return false;
    };
    // SAFETY: signal 0 only checks for existence and permission; nothing is delivered.
    unsafe { libc::killpg(pgid, 0) == 0 }
}

/// A running command's group: registered while it lives. Dropped while still armed (the
/// call's future was dropped half way), it kills the group at once, so no child outlives
/// an abandoned call. Disarmed, it keeps the group registered only if members remain.
pub(crate) struct GroupGuard {
    groups: ProcessGroups,
    pgid: u32,
    armed: bool,
}

impl GroupGuard {
    pub fn new(groups: &ProcessGroups, pgid: u32) -> Self {
        groups.register(pgid);
        GroupGuard {
            groups: groups.clone(),
            pgid,
            armed: true,
        }
    }

    /// The command returned: keep the group registered while a background child is in it.
    pub fn settle(mut self) {
        self.armed = false;
        if !group_alive(self.pgid) {
            self.groups.release(self.pgid);
        }
    }
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        if self.armed {
            killpg(self.pgid, libc::SIGKILL);
            self.groups.release(self.pgid);
        }
    }
}

/// Send `sig` to process group `pgid`; failure (the group is gone) is harmless.
pub(crate) fn killpg(pgid: u32, sig: libc::c_int) {
    let Ok(pgid) = libc::pid_t::try_from(pgid) else {
        return;
    };
    // SAFETY: killpg has no memory-safety preconditions; failure (e.g. ESRCH) is harmless.
    unsafe {
        libc::killpg(pgid, sig);
    }
}
