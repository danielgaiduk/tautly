//! Process signals as engine commands (spec 8): Ctrl-C (SIGINT) cancels the running turn,
//! a second Ctrl-C within [`DOUBLE_INTERRUPT`] shuts the session down; SIGTERM and SIGHUP
//! shut it down.

use std::time::{Duration, Instant};

use futures::{Stream, StreamExt};
use tokio::signal::unix::{SignalKind, signal};
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

/// Listen for SIGINT, SIGTERM and SIGHUP and send the engine the matching commands
/// ([`SignalPolicy`]). The listener holds the engine weakly, and ends once it has sent
/// `Shutdown` or finds the engine gone. While it is installed these signals no longer
/// end the process by themselves: the client exits when the engine's event stream ends.
/// Must be called inside a Tokio runtime.
pub fn install_signal_handlers(engine: &Engine) -> std::io::Result<JoinHandle<()>> {
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
    Ok(handle_signals(engine, signals))
}

/// [`install_signal_handlers`] with the signals injected (tests, embedders that own the
/// process's signal handling).
pub fn handle_signals(
    engine: &Engine,
    signals: impl Stream<Item = Signal> + Send + 'static,
) -> JoinHandle<()> {
    let commands = engine.weak_commands();
    tokio::spawn(async move {
        let mut signals = std::pin::pin!(signals);
        let mut policy = SignalPolicy::default();
        while let Some(sig) = signals.next().await {
            let cmd = policy.command(sig, Instant::now());
            let last = cmd == Command::Shutdown;
            // Held only for the send, so the listener never keeps a session alive.
            let Some(tx) = commands.upgrade() else {
                return;
            };
            if tx.send(cmd).await.is_err() || last {
                return;
            }
        }
    })
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
