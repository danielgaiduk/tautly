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
        let mut st = self.lock();
        st.engines.push(Registered {
            commands: engine.weak_commands(),
            groups: engine.process_groups().clone(),
        });
        st.shutdown_sent = false;
    }

    /// Act on one signal.
    pub async fn deliver(&self, sig: Signal) {
        let (cmd, targets) = {
            let mut st = self.lock();
            let alive: Vec<_> = st
                .engines
                .iter()
                .filter_map(|e| e.commands.upgrade())
                .collect();
            if st.shutdown_sent || alive.is_empty() {
                for e in &st.engines {
                    e.groups.kill_all_now();
                }
                st.engines.clear();
                drop(st);
                (self.escalate)(sig);
                return;
            }
            st.engines.retain(|e| e.commands.upgrade().is_some());
            let cmd = st.policy.command(sig, Instant::now());
            if cmd == Command::Shutdown {
                st.shutdown_sent = true;
            }
            (cmd, alive)
        };
        for tx in targets {
            // An engine that stopped meanwhile is simply skipped.
            let _ = tx.send(cmd.clone()).await;
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, RouterState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The process-wide router behind [`install_signal_handlers`].
static INSTALLED: Mutex<Option<Arc<SignalRouter>>> = Mutex::new(None);

/// Install the process-wide SIGINT, SIGTERM and SIGHUP listener (once; it lives as long as
/// the process) and route signals to `engine` as well. Must be called inside a Tokio
/// runtime. While installed these signals no longer end the process by themselves; the
/// escalation (see [`SignalRouter`]) does. A client must not end the process with
/// `std::process::exit` before an engine's event stream has closed: that skips the
/// session's shutdown, including the end of its process groups.
pub fn install_signal_handlers(engine: &Engine) -> std::io::Result<()> {
    let mut installed = INSTALLED.lock().unwrap_or_else(PoisonError::into_inner);
    let router = match &*installed {
        Some(r) => Arc::clone(r),
        None => {
            let int = signal(SignalKind::interrupt())?;
            let term = signal(SignalKind::terminate())?;
            let hup = signal(SignalKind::hangup())?;
            let signals =
                futures::stream::unfold((int, term, hup), |(mut int, mut term, mut hup)| async {
                    let sig = tokio::select! {
                        Some(()) = int.recv() => Signal::Interrupt,
                        Some(()) = term.recv() => Signal::Terminate,
                        Some(()) = hup.recv() => Signal::Hangup,
                        else => return None,
                    };
                    Some((sig, (int, term, hup)))
                });
            let router = SignalRouter::new(die_by);
            drop(handle_signals(&router, signals));
            *installed = Some(Arc::clone(&router));
            router
        }
    };
    router.add(engine);
    Ok(())
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
