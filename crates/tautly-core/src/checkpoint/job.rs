//! The checkpointer in the session: when a checkpoint is taken, and the jobs that take it.

use super::*;

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
    /// Before a rewind or branch switch writes: always a full snapshot.
    BeforeRestore,
    /// At a turn's start when the previous turn's end has no checkpoint: always a full
    /// snapshot.
    TurnStart,
}

impl Point {
    /// How the log names it.
    fn logged(self) -> Option<CheckpointPoint> {
        Some(match self {
            Point::SessionStart => CheckpointPoint::SessionStart,
            Point::ResumeStart => CheckpointPoint::ResumeStart,
            Point::BeforeExec => CheckpointPoint::BeforeExec,
            Point::TurnEnd => CheckpointPoint::TurnEnd,
            Point::BeforeRestore => CheckpointPoint::BeforeRestore,
            Point::TurnStart => CheckpointPoint::TurnStart,
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
    pub(super) async fn run(
        self,
        st: &mut GitCheckpoints,
        point: Point,
        cancel: Option<&CancellationToken>,
    ) -> bool {
        if st.off {
            return false;
        }
        let watcher_changed = self.watcher.is_none() || self.watcher != st.fs_events;
        st.fs_events = self.watcher;
        let write_gen = self.write_gen;
        let snap = async {
            match point {
                Point::SessionStart
                | Point::ResumeStart
                | Point::BeforeRestore
                | Point::TurnStart => st.cp.snapshot_now().await,
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
        let made = matches!(taken, Some(Ok(_)));
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
        made
    }

    async fn log(&self, rec: SessionRecord) {
        let log = self.log.clone();
        if let Ok(Err(failure)) = tokio::task::spawn_blocking(move || log.append(&rec)).await
            && let Some(ev) = failure.event()
        {
            self.events.untagged().emit(ev).await;
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

    /// After a cancelled turn (whose end has a `CheckpointSkipped{cancelled}`): its end
    /// checkpoint, taken in the background once its commands have stopped, so the files
    /// as it left them are known before a human changes them (Task 49 N1). The next turn
    /// waits for it ([`settle_turn_end`](Self::settle_turn_end)).
    pub(crate) fn recover_turn_end(&mut self) {
        let Some(shared) = self.checkpoints.clone() else {
            return;
        };
        let job = self.checkpoint_job(self.last_step());
        let log = job.log.clone();
        self.end_recovery = Some(tokio::spawn(async move {
            let mut st = shared.lock().await;
            let taken = job.run(&mut st, Point::TurnEnd, None).await;
            // Durable at once, inside the lock: a reader that takes the lock sees it.
            let _ = tokio::task::spawn_blocking(move || log.checkpoint()).await;
            drop(st);
            taken
        }));
    }

    /// At a turn's start: wait for the previous turn's end checkpoint being recovered;
    /// when none could be taken, take a `turn_start` one now, before anything else runs.
    pub(crate) async fn settle_turn_end(&mut self, cancel: &CancellationToken) {
        let Some(mut job) = self.end_recovery.take() else {
            return;
        };
        let taken = tokio::select! {
            r = &mut job => r.unwrap_or(false),
            () = cancel.cancelled() => {
                job.abort();
                return;
            }
        };
        if !taken {
            self.git_checkpoint(Point::TurnStart, self.last_step(), Some(cancel))
                .await;
        }
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
    /// A cancelled turn's end checkpoint being recovered is waited for too (every job that
    /// reads or moves the log calls this, review CP5-A I2); its outcome is kept for the
    /// next turn's [`settle_turn_end`](Self::settle_turn_end). True when both are settled.
    pub(crate) async fn settle_start_checkpoint_or(&mut self, cancel: &CancellationToken) -> bool {
        if let StartCheckpoint::Running(job) = &mut self.start_checkpoint {
            tokio::select! {
                biased;
                _ = job => self.start_checkpoint = StartCheckpoint::Idle,
                () = cancel.cancelled() => return false,
            }
        }
        if let Some(job) = &mut self.end_recovery {
            let taken = tokio::select! {
                biased;
                r = job => r.unwrap_or(false),
                () = cancel.cancelled() => return false,
            };
            // A finished handle cannot be awaited again: keep its outcome in a fresh one.
            self.end_recovery = Some(tokio::spawn(std::future::ready(taken)));
        }
        true
    }

    /// At shutdown: let a cancelled turn's end checkpoint still being taken finish, for at
    /// most the snapshot bound (`timeout_ms`) and a margin; then it is abandoned (its git
    /// is killed). The files as that turn left them are what tells a later human edit
    /// apart.
    pub(crate) async fn finish_turn_end(&mut self) {
        let Some(mut job) = self.end_recovery.take() else {
            return;
        };
        let bound = Duration::from_millis(self.setup.config.checkpoints.timeout_ms)
            + Duration::from_secs(2);
        if tokio::time::timeout(bound, &mut job).await.is_err() {
            job.abort();
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
