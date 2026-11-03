//! The process groups a session's commands started (spec 8: shutdown terminates every
//! spawned process group).
//!
//! `bash` runs each command in a process group of its own and registers it here while the
//! command runs. A group that still has members when the command returns (a background
//! child such as a dev server) stays registered, *settled*, so the engine can end it at
//! shutdown ([`ProcessGroups::terminate_all`]); a group that emptied is released at once.
//! Groups that have died since are pruned whenever one is registered or listed, so the
//! registry stays as small as the set of live background groups.
//!
//! **Reused numbers.** A settled group's leader (the shell) has been reaped, and no pid can
//! be reused while a process group with that number exists. So if a live process has the
//! pid equal to a settled entry, the old group is gone and its number was reused: the
//! entry is dropped without signalling anything.
//!
//! **What escapes.** Cleanup works on process groups. A command that leaves its group is
//! not covered: `setsid` (a new session), job control (`set -m`, `set -o monitor`, which
//! puts each background job in its own group), daemonizers (`daemonize`,
//! `start-stop-daemon`, `tmux new -d`, `screen -dm`, `docker run -d`), and every child if
//! tautly itself is killed with SIGKILL. The permission policy asks before `setsid` and
//! job control; on Linux the engine also makes itself a child subreaper, so orphans that
//! left their group are adopted and ended at shutdown. The rest waits for the OS-level
//! sandbox.
//!
//! **Zombies** count as members (`killpg(pgid, 0)` succeeds for them). Normally the
//! runtime reaps our own children and init reaps orphans at once; where nothing reaps
//! orphans (tautly as PID 1 in a container without an init) a dead group stays "alive":
//! cancel then waits its full grace and `terminate_all` its whole `grace`, but nothing is
//! signalled wrongly. On Linux the engine reaps the orphans it adopts as subreaper.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How often [`ProcessGroups::terminate_all`] looks whether the groups are gone.
const POLL: Duration = Duration::from_millis(5);

/// Leaders of the commands running now, across every registry in the process: our own
/// children, which an orphan reaper must leave to the runtime.
static RUNNING: Mutex<BTreeSet<u32>> = Mutex::new(BTreeSet::new());

fn running() -> std::sync::MutexGuard<'static, BTreeSet<u32>> {
    RUNNING.lock().unwrap_or_else(|e| e.into_inner())
}

/// The pids of the commands running now in this process (their group leaders).
pub fn running_leaders() -> Vec<u32> {
    running().iter().copied().collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// The command runs; its leader is our child.
    Running,
    /// The command returned and its leader was reaped; background children remain.
    Settled,
}

/// A shared set of process group ids. Clones share the set.
#[derive(Debug, Clone, Default)]
pub struct ProcessGroups {
    live: Arc<Mutex<BTreeMap<u32, State>>>,
}

impl ProcessGroups {
    /// Track `pgid`, whose leader is a command starting now (pruning groups that died).
    pub fn register(&self, pgid: u32) {
        let mut live = self.lock();
        live.retain(|&g, &mut st| still_ours(g, st));
        live.insert(pgid, State::Running);
        running().insert(pgid);
    }

    /// The command that led `pgid` has returned and its leader was reaped; what is left in
    /// the group are background children. Released if none are.
    pub fn settle(&self, pgid: u32) {
        running().remove(&pgid);
        let mut live = self.lock();
        if live.contains_key(&pgid) && still_ours(pgid, State::Settled) {
            live.insert(pgid, State::Settled);
        } else {
            live.remove(&pgid);
        }
    }

    /// Stop tracking `pgid`.
    pub fn release(&self, pgid: u32) {
        running().remove(&pgid);
        self.lock().remove(&pgid);
    }

    /// The groups that are still ours and have members, in ascending order (pruning the
    /// others).
    pub fn live(&self) -> Vec<u32> {
        let mut live = self.lock();
        live.retain(|&g, &mut st| still_ours(g, st));
        live.keys().copied().collect()
    }

    /// End every tracked group: SIGTERM, up to `grace` for them to go, then SIGKILL for
    /// what is left. Returns how many groups were signalled; the set is empty afterwards.
    pub async fn terminate_all(&self, grace: Duration) -> usize {
        let groups: Vec<(u32, State)> = {
            let mut live = self.lock();
            live.retain(|&g, &mut st| still_ours(g, st));
            live.iter().map(|(&g, &st)| (g, st)).collect()
        };
        for &(g, _) in &groups {
            killpg(g, libc::SIGTERM);
        }
        let end = tokio::time::Instant::now() + grace;
        while groups.iter().any(|&(g, st)| still_ours(g, st)) && tokio::time::Instant::now() < end {
            tokio::time::sleep(POLL).await;
        }
        for &(g, st) in &groups {
            if still_ours(g, st) {
                killpg(g, libc::SIGKILL);
            }
        }
        let mut live = self.lock();
        for (g, _) in &groups {
            live.remove(g);
            running().remove(g);
        }
        groups.len()
    }

    /// SIGKILL every tracked group now (no grace: for paths that cannot wait, such as a
    /// runtime being dropped or a signal escalation).
    pub fn kill_all_now(&self) {
        let mut live = self.lock();
        for (&g, &st) in live.iter() {
            if still_ours(g, st) {
                killpg(g, libc::SIGKILL);
            }
            running().remove(&g);
        }
        live.clear();
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<u32, State>> {
        self.live.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Whether the group `pgid` in state `st` is alive and still the one we started.
fn still_ours(pgid: u32, st: State) -> bool {
    group_alive(pgid) && (st == State::Running || !pid_alive(pgid))
}

/// Whether a process with pid `pid` exists (zombies included).
fn pid_alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: signal 0 only checks for existence and permission; nothing is delivered.
    unsafe { libc::kill(pid, 0) == 0 }
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

    /// The command returned and its leader was reaped: keep the group registered while a
    /// background child is in it.
    pub fn settle(mut self) {
        self.armed = false;
        self.groups.settle(self.pgid);
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
