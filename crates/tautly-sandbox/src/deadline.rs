//! Work with a deadline on one long-lived worker thread (the shell parse: tree-sitter's
//! error recovery can run for minutes on a short crafted input and cannot be cancelled).
//!
//! A job past the deadline is abandoned: its caller gets [`Refusal::TimedOut`] and, until
//! the worker finishes it, every new job is refused at once ([`Refusal::Stuck`]). There is
//! one worker thread per [`Worker`], so at most one job is ever stuck, whatever the number
//! of callers. A panicking job is caught: its caller gets [`Refusal::Panicked`] and the
//! worker carries on.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError, mpsc};
use std::time::Duration;

/// Why a job gave no result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// An earlier job is still running past its deadline.
    Stuck,
    /// This job ran past the deadline (it goes on, abandoned).
    TimedOut,
    /// This job panicked.
    Panicked,
    /// The worker thread could not be started.
    Unavailable,
}

/// A job, given its number in queue order.
type Job = Box<dyn FnOnce(u64) + Send>;

struct Queue {
    tx: Option<mpsc::Sender<(u64, Job)>>,
    /// The number of the next job sent.
    next: u64,
}

pub(crate) struct Worker {
    deadline: Duration,
    /// The last job that ran past its deadline, while the worker has not finished it. Set
    /// by its caller and cleared by the worker, both under this lock. Jobs run in queue
    /// order, so no earlier abandoned job is still running once this one ends.
    stuck: Mutex<Option<u64>>,
    jobs: Mutex<Queue>,
    /// Worker threads started (one, unless starting failed).
    spawned: AtomicUsize,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Clears `stuck` when the worker is done with job `.1`, however it ended, if that is the
/// last abandoned job.
struct Done(&'static Mutex<Option<u64>>, u64);

impl Drop for Done {
    fn drop(&mut self) {
        let mut stuck = lock(self.0);
        if stuck.is_some_and(|last| last <= self.1) {
            *stuck = None;
        }
    }
}

impl Worker {
    pub(crate) const fn new(deadline: Duration) -> Worker {
        Worker {
            deadline,
            stuck: Mutex::new(None),
            jobs: Mutex::new(Queue { tx: None, next: 0 }),
            spawned: AtomicUsize::new(0),
        }
    }

    /// `f`'s result if it ends within the deadline.
    pub(crate) fn run<T: Send + 'static>(
        &'static self,
        f: impl FnOnce() -> T + Send + 'static,
    ) -> Result<T, Refusal> {
        if lock(&self.stuck).is_some() {
            return Err(Refusal::Stuck);
        }
        let (tx, rx) = mpsc::sync_channel(1);
        let stuck = &self.stuck;
        let job: Job = Box::new(move |id| {
            let done = Done(stuck, id);
            let result = catch_unwind(AssertUnwindSafe(f));
            // Sent before `done` clears `stuck`: a caller that finds no result under the
            // lock marks the worker stuck, and this clear comes after.
            let _ = tx.send(result);
            drop(done);
        });
        let id = self.submit(job)?;
        match rx.recv_timeout(self.deadline) {
            Ok(r) => r.map_err(|_| Refusal::Panicked),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let mut stuck = lock(&self.stuck);
                match rx.try_recv() {
                    Ok(r) => r.map_err(|_| Refusal::Panicked),
                    Err(_) => {
                        *stuck = Some(stuck.map_or(id, |last| last.max(id)));
                        Err(Refusal::TimedOut)
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(Refusal::Panicked),
        }
    }

    /// Hand `job` to the worker thread, starting it on first use. Returns the job's number.
    fn submit(&'static self, job: Job) -> Result<u64, Refusal> {
        let mut jobs = lock(&self.jobs);
        if jobs.tx.is_none() {
            let (tx, rx) = mpsc::channel::<(u64, Job)>();
            std::thread::Builder::new()
                .name("shell-parse".into())
                .spawn(move || {
                    for (id, job) in rx {
                        job(id);
                    }
                })
                .map_err(|_| Refusal::Unavailable)?;
            self.spawned.fetch_add(1, Ordering::Relaxed);
            jobs.tx = Some(tx);
        }
        let id = jobs.next;
        let tx = jobs.tx.as_ref().ok_or(Refusal::Unavailable)?;
        tx.send((id, job)).map_err(|_| Refusal::Unavailable)?;
        jobs.next += 1;
        Ok(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn worker(ms: u64) -> &'static Worker {
        Box::leak(Box::new(Worker::new(Duration::from_millis(ms))))
    }

    /// Waits (generously, for a loaded runner) until the worker takes jobs again.
    fn eventually_ok(w: &'static Worker) {
        let until = Instant::now() + Duration::from_secs(30);
        loop {
            match w.run(|| 3) {
                Ok(n) => return assert_eq!(n, 3),
                Err(e) => assert!(Instant::now() < until, "still refused: {e:?}"),
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// A job that runs until `release` is sent or dropped.
    fn blocked() -> (mpsc::Sender<()>, impl FnOnce() + Send + 'static) {
        let (release, gate) = mpsc::channel::<()>();
        (release, move || {
            let _ = gate.recv();
        })
    }

    /// Review N1: with two abandoned jobs, the first one ending does not clear `stuck`
    /// while the second still runs.
    #[test]
    fn only_the_last_abandoned_job_clears_stuck() {
        // Long enough that both jobs are queued before either caller gives up.
        let w = worker(1000);
        let (release_a, a) = blocked();
        let (release_b, b) = blocked();
        let first = std::thread::spawn(move || w.run(a));
        let second = std::thread::spawn(move || w.run(b));
        assert_eq!(first.join().unwrap(), Err(Refusal::TimedOut));
        assert_eq!(second.join().unwrap(), Err(Refusal::TimedOut));
        drop(release_a);
        // Time for the worker to finish A; the assertion holds however long it takes.
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(w.run(|| 1), Err(Refusal::Stuck));
        drop(release_b);
        eventually_ok(w);
    }

    #[test]
    fn a_job_within_the_deadline_returns_its_result() {
        let w = worker(2000);
        assert_eq!(w.run(|| 7), Ok(7));
        assert_eq!(w.run(|| "again"), Ok("again"));
    }

    /// Review M1: a panicking job clears the in-flight state (and is reported).
    #[test]
    fn a_panic_is_reported_and_the_worker_carries_on() {
        let w = worker(2000);
        assert_eq!(w.run(|| -> u8 { panic!("boom") }), Err(Refusal::Panicked));
        assert_eq!(w.run(|| 1), Ok(1));
        // Also when the panic comes after the deadline: the worker clears `stuck`.
        let (release, wait) = blocked();
        assert_eq!(
            w.run(|| -> u8 {
                wait();
                panic!("late")
            }),
            Err(Refusal::TimedOut)
        );
        drop(release);
        eventually_ok(w);
    }

    #[test]
    fn a_job_past_the_deadline_refuses_others_until_it_ends() {
        let w = worker(100);
        let (release, job) = blocked();
        assert_eq!(w.run(job), Err(Refusal::TimedOut));
        assert_eq!(w.run(|| 1), Err(Refusal::Stuck));
        drop(release);
        eventually_ok(w);
    }

    /// Review M2: concurrent callers that all time out leave one worker thread, so at
    /// most one stuck job, and the state clears once the worker catches up.
    #[test]
    fn concurrent_timeouts_leave_one_worker() {
        let w = worker(50);
        let (release, gate) = mpsc::channel::<()>();
        let gate = std::sync::Arc::new(Mutex::new(gate));
        let callers: Vec<_> = (0..8)
            .map(|_| {
                let gate = gate.clone();
                std::thread::spawn(move || {
                    w.run(move || {
                        let _ = lock(&gate).recv();
                    })
                })
            })
            .collect();
        for c in callers {
            assert!(c.join().unwrap().is_err());
        }
        assert_eq!(w.spawned.load(Ordering::Relaxed), 1);
        // The queued jobs run one after another; then the worker is free again.
        drop(release);
        eventually_ok(w);
    }
}
