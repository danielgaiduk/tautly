//! Process signals as engine commands (spec 8): Ctrl-C (SIGINT) cancels the running turn,
//! a second Ctrl-C within [`DOUBLE_INTERRUPT`] shuts the session down; SIGTERM and SIGHUP
//! shut it down. Any signal after that (or with no session alive) ends the process the
//! way the signal would have, after killing every session's process groups.

use std::time::{Duration, Instant};

use std::sync::{Arc, Mutex, PoisonError};

use futures::{Stream, StreamExt};
use tautly_tools::ProcessGroups;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::engine::Engine;
use crate::protocol::Command;

/// Two interrupts this close together mean "exit".
pub const DOUBLE_INTERRUPT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    /// SIGINT (Ctrl-C).
    Interrupt,
    /// SIGTERM.
    Terminate,
    /// SIGHUP (the terminal went away).
    Hangup,
}

/// Turns signals into commands; remembers the last interrupt.
#[derive(Debug, Default)]
pub struct SignalPolicy {
    last_interrupt: Option<Instant>,
}

impl SignalPolicy {
    pub fn command(&mut self, sig: Signal, now: Instant) -> Command {
        match sig {
            Signal::Interrupt => {
                let again = self
                    .last_interrupt
                    .is_some_and(|t| now.saturating_duration_since(t) <= DOUBLE_INTERRUPT);
                self.last_interrupt = Some(now);
                if again {
                    Command::Shutdown
                } else {
                    Command::Cancel
                }
            }
            Signal::Terminate | Signal::Hangup => Command::Shutdown,
        }
    }
}

/// Routes process signals to every live engine of the process (spec 3.4: one process may
/// run several sessions). One [`SignalPolicy`] decides the command, which every engine
/// gets. Once `Shutdown` has been sent, or when no engine is alive, a signal *escalates*:
/// every process group of every engine is killed and the escalation runs (by default the
/// signal's default action is restored and the signal raised again, so the process exits
/// with 128 + the signal number: 130 for Ctrl-C, 143 for SIGTERM).
pub struct SignalRouter {
    state: Mutex<RouterState>,
    escalate: Box<dyn Fn(Signal) + Send + Sync>,
}

#[derive(Default)]
struct RouterState {
    engines: Vec<Registered>,
    policy: SignalPolicy,
    /// `Shutdown` was sent, or an engine could not take a command: the next signal
    /// escalates.
    shutdown_sent: bool,
}

struct Registered {
    commands: mpsc::WeakSender<Command>,
    groups: ProcessGroups,
}

impl SignalRouter {
    /// A router whose escalation is `escalate` (tests, embedders).
    pub fn new(escalate: impl Fn(Signal) + Send + Sync + 'static) -> Arc<SignalRouter> {
        Arc::new(SignalRouter {
            state: Mutex::default(),
            escalate: Box::new(escalate),
        })
    }

    /// Route signals to `engine` too (held weakly: never keeps it alive). A new session
    /// means the process goes on, so an earlier `Shutdown` no longer escalates.
    pub fn add(&self, engine: &Engine) {
        self.add_channel(engine.weak_commands(), engine.process_groups().clone());
    }

    fn add_channel(&self, commands: mpsc::WeakSender<Command>, groups: ProcessGroups) {
        let mut st = self.lock();
        st.engines.push(Registered { commands, groups });
        st.shutdown_sent = false;
    }

    /// Act on one signal. Never waits: a command is handed over with `try_send`, and an
    /// engine whose command channel is full counts as stuck, so the next signal escalates.
    /// An engine whose channel is closed (its actor stopped, a handle still alive) is
    /// dead: with no other engine the signal escalates at once.
    pub async fn deliver(&self, sig: Signal) {
        let mut st = self.lock();
        let live = |e: &Registered| e.commands.upgrade().filter(|tx| !tx.is_closed());
        let alive: Vec<_> = st.engines.iter().filter_map(live).collect();
        if st.shutdown_sent || alive.is_empty() {
            return self.escalate(st, sig);
        }
        st.engines.retain(|e| live(e).is_some());
        let cmd = st.policy.command(sig, Instant::now());
        let (mut stuck, mut taken) = (false, false);
        for tx in alive {
            match tx.try_send(cmd.clone()) {
                Ok(()) => taken = true,
                Err(mpsc::error::TrySendError::Full(_)) => stuck = true,
                // Stopped meanwhile.
                Err(mpsc::error::TrySendError::Closed(_)) => {}
            }
        }
        if !taken && !stuck {
            return self.escalate(st, sig);
        }
        if cmd == Command::Shutdown || stuck {
            st.shutdown_sent = true;
        }
    }

    /// Kill every engine's process groups, forget the engines and run the escalation.
    fn escalate(&self, mut st: std::sync::MutexGuard<'_, RouterState>, sig: Signal) {
        for e in &st.engines {
            e.groups.kill_all_now();
        }
        st.engines.clear();
        drop(st);
        (self.escalate)(sig);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, RouterState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A router and its listener task.
type Installed = Option<(Arc<SignalRouter>, JoinHandle<()>)>;

/// The process-wide router behind [`install_signal_handlers`], and its listener task.
static INSTALLED: Mutex<Installed> = Mutex::new(None);

/// Install the process-wide SIGINT, SIGTERM and SIGHUP listener and route signals to
/// `engine` as well. Must be called inside a Tokio runtime.
///
/// The listener runs on the runtime of the call that started it. If that runtime ends while
/// the process goes on, signals stay captured but undelivered until the next call, which
/// starts the listener again on its own runtime. For the CLI and the RPC server (one
/// runtime for the life of the process) it simply lives as long as the process.
///
/// While installed these signals no longer end the process by themselves; the escalation
/// (see [`SignalRouter`]) does. A client must not end the process with
/// `std::process::exit` before an engine's event stream has closed: that skips the
/// session's shutdown, including the end of its process groups.
pub fn install_signal_handlers(engine: &Engine) -> std::io::Result<()> {
    install_into(&INSTALLED, listen, |router| router.add(engine))
}

/// The router behind [`install_termination_handlers`] (SIGTERM and SIGHUP only).
static TERMINATION: Mutex<Installed> = Mutex::new(None);

/// Like [`install_signal_handlers`], for SIGTERM and SIGHUP only: SIGINT keeps its
/// default disposition until the client listens for it itself. For a client that owns
/// Ctrl-C (the REPL: a cancel, then an exit), so that no second listener counts presses
/// on its own clock. A process uses one of the two installers, not both.
pub fn install_termination_handlers(engine: &Engine) -> std::io::Result<()> {
    install_into(&TERMINATION, listen_termination, |router| {
        router.add(engine)
    })
}

/// [`install_signal_handlers`] on `slot`: start the listener with `listen` unless it runs,
/// then `register` the engine with the router.
fn install_into(
    slot: &Mutex<Installed>,
    listen: impl FnOnce(&Arc<SignalRouter>) -> std::io::Result<JoinHandle<()>>,
    register: impl FnOnce(&SignalRouter),
) -> std::io::Result<()> {
    let mut installed = slot.lock().unwrap_or_else(PoisonError::into_inner);
    let router = match installed.take() {
        Some((router, listener)) if !listener.is_finished() => {
            *installed = Some((Arc::clone(&router), listener));
            router
        }
        // Never started, or its runtime is gone: start it here. If that fails, the
        // earlier router stays (with its engines) for the next call to try again.
        earlier => {
            let router = earlier
                .as_ref()
                .map_or_else(|| SignalRouter::new(die_by), |(r, _)| Arc::clone(r));
            match listen(&router) {
                Ok(listener) => *installed = Some((Arc::clone(&router), listener)),
                Err(e) => {
                    *installed = earlier;
                    return Err(e);
                }
            }
            router
        }
    };
    register(&router);
    Ok(())
}

/// The real signals, fed to `router`.
fn listen(router: &Arc<SignalRouter>) -> std::io::Result<JoinHandle<()>> {
    let int = signal(SignalKind::interrupt())?;
    let term = signal(SignalKind::terminate())?;
    let hup = signal(SignalKind::hangup())?;
    let signals = futures::stream::unfold((int, term, hup), |(mut int, mut term, mut hup)| async {
        let sig = tokio::select! {
            Some(()) = int.recv() => Signal::Interrupt,
            Some(()) = term.recv() => Signal::Terminate,
            Some(()) = hup.recv() => Signal::Hangup,
            else => return None,
        };
        Some((sig, (int, term, hup)))
    });
    Ok(handle_signals(router, signals))
}

/// SIGTERM and SIGHUP, fed to `router`.
fn listen_termination(router: &Arc<SignalRouter>) -> std::io::Result<JoinHandle<()>> {
    let term = signal(SignalKind::terminate())?;
    let hup = signal(SignalKind::hangup())?;
    let signals = futures::stream::unfold((term, hup), |(mut term, mut hup)| async {
        let sig = tokio::select! {
            Some(()) = term.recv() => Signal::Terminate,
            Some(()) = hup.recv() => Signal::Hangup,
            else => return None,
        };
        Some((sig, (term, hup)))
    });
    Ok(handle_signals(router, signals))
}

/// Feed `signals` to `router` until the stream ends.
pub fn handle_signals(
    router: &Arc<SignalRouter>,
    signals: impl Stream<Item = Signal> + Send + 'static,
) -> JoinHandle<()> {
    let router = Arc::clone(router);
    tokio::spawn(async move {
        let mut signals = std::pin::pin!(signals);
        while let Some(sig) = signals.next().await {
            router.deliver(sig).await;
        }
    })
}

/// End the process the way `sig` would have: its default action, raised again.
fn die_by(sig: Signal) {
    let n = match sig {
        Signal::Interrupt => libc::SIGINT,
        Signal::Terminate => libc::SIGTERM,
        Signal::Hangup => libc::SIGHUP,
    };
    // SAFETY: restoring the default disposition and raising the signal have no memory
    // preconditions; the default action of these signals ends the process.
    unsafe {
        libc::signal(n, libc::SIG_DFL);
        libc::raise(n);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_interrupt_cancels_two_within_two_seconds_shut_down() {
        let t = Instant::now();
        let mut p = SignalPolicy::default();
        assert_eq!(p.command(Signal::Interrupt, t), Command::Cancel);
        assert_eq!(
            p.command(Signal::Interrupt, t + Duration::from_millis(1900)),
            Command::Shutdown
        );

        let mut p = SignalPolicy::default();
        assert_eq!(p.command(Signal::Interrupt, t), Command::Cancel);
        assert_eq!(
            p.command(Signal::Interrupt, t + Duration::from_millis(2500)),
            Command::Cancel
        );
        // The window starts again at the last interrupt.
        assert_eq!(
            p.command(Signal::Interrupt, t + Duration::from_millis(3000)),
            Command::Shutdown
        );
    }

    #[tokio::test]
    async fn a_full_command_channel_never_stalls_the_router() {
        let escalated = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&escalated);
        let router = SignalRouter::new(move |sig| seen.lock().unwrap().push(sig));
        // An engine that takes no commands: its channel is full.
        let (tx, _rx) = mpsc::channel(1);
        tx.try_send(Command::Cancel).unwrap();
        router.add_channel(tx.downgrade(), ProcessGroups::default());
        let quick = |sig| {
            let router = Arc::clone(&router);
            async move {
                tokio::time::timeout(Duration::from_millis(200), router.deliver(sig))
                    .await
                    .is_ok()
            }
        };
        assert!(
            quick(Signal::Interrupt).await,
            "the router waited on a full channel"
        );
        // The command could not be handed over: the next signal ends the process.
        assert!(quick(Signal::Interrupt).await);
        assert_eq!(*escalated.lock().unwrap(), [Signal::Interrupt]);
    }

    fn recording_router() -> (Arc<SignalRouter>, Arc<Mutex<Vec<Signal>>>) {
        let escalated = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&escalated);
        let router = SignalRouter::new(move |sig| seen.lock().unwrap().push(sig));
        (router, escalated)
    }

    #[tokio::test]
    async fn an_engine_whose_channel_is_closed_counts_as_dead() {
        let (router, escalated) = recording_router();
        // The engine's actor has stopped, but a sender (its handle) is still alive.
        let (tx, rx) = mpsc::channel(4);
        drop(rx);
        router.add_channel(tx.downgrade(), ProcessGroups::default());
        // Nobody can take the command: the signal ends the process at once.
        router.deliver(Signal::Interrupt).await;
        assert_eq!(*escalated.lock().unwrap(), [Signal::Interrupt]);

        // Beside a live engine, a closed one is dropped and the live one gets the command.
        let (router, escalated) = recording_router();
        let (dead, dead_rx) = mpsc::channel(4);
        drop(dead_rx);
        let (live, mut live_rx) = mpsc::channel(4);
        router.add_channel(dead.downgrade(), ProcessGroups::default());
        router.add_channel(live.downgrade(), ProcessGroups::default());
        router.deliver(Signal::Interrupt).await;
        assert_eq!(live_rx.try_recv().unwrap(), Command::Cancel);
        assert!(escalated.lock().unwrap().is_empty());
        assert_eq!(router.lock().engines.len(), 1);
    }

    #[tokio::test]
    async fn a_listener_that_cannot_restart_keeps_the_earlier_engines() {
        let (router, _) = recording_router();
        let (tx, mut rx) = mpsc::channel(4);
        router.add_channel(tx.downgrade(), ProcessGroups::default());
        // The listener's runtime is gone (its task finished).
        let finished = tokio::spawn(async {});
        while !finished.is_finished() {
            tokio::task::yield_now().await;
        }
        let slot = Mutex::new(Some((Arc::clone(&router), finished)));
        let failed = install_into(
            &slot,
            |_| Err(std::io::Error::other("no signals here")),
            |_| panic!("registered although the listener did not start"),
        );
        assert!(failed.is_err());
        // The next install starts the listener on the same router, its engines kept.
        let mut restarted_on = None;
        install_into(
            &slot,
            |r| {
                restarted_on = Some(Arc::clone(r));
                Ok(tokio::spawn(std::future::pending()))
            },
            |_| {},
        )
        .unwrap();
        let restarted_on = restarted_on.expect("the listener was started again");
        assert!(Arc::ptr_eq(&restarted_on, &router));
        restarted_on.deliver(Signal::Interrupt).await;
        assert_eq!(rx.try_recv().unwrap(), Command::Cancel);
    }

    #[test]
    fn terminate_and_hangup_shut_down() {
        let mut p = SignalPolicy::default();
        assert_eq!(
            p.command(Signal::Terminate, Instant::now()),
            Command::Shutdown
        );
        assert_eq!(p.command(Signal::Hangup, Instant::now()), Command::Shutdown);
    }
}
