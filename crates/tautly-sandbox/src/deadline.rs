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

type Job = Box<dyn FnOnce() + Send>;

pub(crate) struct Worker {
    deadline: Duration,
    /// A job ran past its deadline and the worker has not finished it yet. Set by its
    /// caller and cleared by the worker, both under this lock.
    stuck: Mutex<bool>,
    jobs: Mutex<Option<mpsc::Sender<Job>>>,
    /// Worker threads started (one, unless starting failed).
    spawned: AtomicUsize,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Clears `stuck` when the worker is done with a job, however it ended.
struct Done(&'static Mutex<bool>);

impl Drop for Done {
    fn drop(&mut self) {
        *lock(self.0) = false;
    }
}

impl Worker {
    pub(crate) const fn new(deadline: Duration) -> Worker {
        Worker {
            deadline,
            stuck: Mutex::new(false),
            jobs: Mutex::new(None),
            spawned: AtomicUsize::new(0),
        }
    }

    /// `f`'s result if it ends within the deadline.
    pub(crate) fn run<T: Send + 'static>(
        &'static self,
        f: impl FnOnce() -> T + Send + 'static,
    ) -> Result<T, Refusal> {
        if *lock(&self.stuck) {
            return Err(Refusal::Stuck);
        }
        let (tx, rx) = mpsc::sync_channel(1);
        let stuck = &self.stuck;
        let job: Job = Box::new(move || {
            let done = Done(stuck);
            let result = catch_unwind(AssertUnwindSafe(f));
            // Sent before `done` clears `stuck`: a caller that finds no result under the
            // lock marks the worker stuck, and this clear comes after.
            let _ = tx.send(result);
            drop(done);
        });
        self.submit(job)?;
        match rx.recv_timeout(self.deadline) {
            Ok(r) => r.map_err(|_| Refusal::Panicked),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let mut stuck = lock(&self.stuck);
                match rx.try_recv() {
                    Ok(r) => r.map_err(|_| Refusal::Panicked),
                    Err(_) => {
                        *stuck = true;
                        Err(Refusal::TimedOut)
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(Refusal::Panicked),
        }
    }

    /// Hand `job` to the worker thread, starting it on first use.
    fn submit(&'static self, job: Job) -> Result<(), Refusal> {
        let mut jobs = lock(&self.jobs);
        if jobs.is_none() {
            let (tx, rx) = mpsc::channel::<Job>();
            std::thread::Builder::new()
                .name("shell-parse".into())
                .spawn(move || {
                    for job in rx {
                        job();
                    }
                })
                .map_err(|_| Refusal::Unavailable)?;
            self.spawned.fetch_add(1, Ordering::Relaxed);
            *jobs = Some(tx);
        }
        let tx = jobs.as_ref().ok_or(Refusal::Unavailable)?;
        tx.send(job).map_err(|_| Refusal::Unavailable)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn worker(ms: u64) -> &'static Worker {
        Box::leak(Box::new(Worker::new(Duration::from_millis(ms))))
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
        assert_eq!(
            w.run(|| -> u8 {
                std::thread::sleep(Duration::from_millis(2300));
                panic!("late")
            }),
            Err(Refusal::TimedOut)
        );
        std::thread::sleep(Duration::from_millis(600));
        assert_eq!(w.run(|| 2), Ok(2));
    }

    #[test]
    fn a_job_past_the_deadline_refuses_others_until_it_ends() {
        let w = worker(100);
        let started = Instant::now();
        assert_eq!(
            w.run(|| std::thread::sleep(Duration::from_millis(600))),
            Err(Refusal::TimedOut)
        );
        assert!(started.elapsed() < Duration::from_millis(500));
        let started = Instant::now();
        assert_eq!(w.run(|| 1), Err(Refusal::Stuck));
        assert!(started.elapsed() < Duration::from_millis(50));
        std::thread::sleep(Duration::from_millis(700));
        assert_eq!(w.run(|| 1), Ok(1));
    }

    /// Review M2: concurrent callers that all time out leave one worker thread, so at
    /// most one stuck job, and the state clears once the worker catches up.
    #[test]
    fn concurrent_timeouts_leave_one_worker() {
        let w = worker(50);
        let callers: Vec<_> = (0..8)
            .map(|_| {
                std::thread::spawn(move || w.run(|| std::thread::sleep(Duration::from_millis(200))))
            })
            .collect();
        for c in callers {
            assert!(c.join().unwrap().is_err());
        }
        assert_eq!(w.spawned.load(Ordering::Relaxed), 1);
        // The queued jobs run one after another; then the worker is free again.
        std::thread::sleep(Duration::from_millis(8 * 200 + 300));
        assert_eq!(w.run(|| 3), Ok(3));
    }
}
