//! `tautly -p "<prompt>"` (spec 3.4): send one message, stream what happens to the
//! output, then shut the session down and exit with a code mapped from how the turn ended
//! (0 completed, 1 error, 2 budget / max_turns / timeout, 130 cancelled).
//!
//! The session is headless (the caller builds it so): `ask` resolves to deny unless
//! `--approve` raised the mode, memory proposals are rejected unless `--approve-memory`,
//! and nothing ever waits for input.

use std::io::Write as _;

use tautly_core::{Command, Engine, Event, FinishReason};
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

/// What `--output` prints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    /// Every event, one JSON object per line (the protocol's `Event`), until the session
    /// has shut down.
    Json,
    /// The model's text only, ending with a newline; errors go to stderr.
    Text,
}

/// The exit code for a turn that ended for `reason`. A reason this version does not know
/// is an error.
pub fn exit_code(reason: FinishReason) -> i32 {
    match reason {
        FinishReason::Completed => 0,
        FinishReason::Budget | FinishReason::MaxTurns | FinishReason::Timeout => 2,
        FinishReason::Cancelled => 130,
        _ => 1,
    }
}

/// Run `prompt` as one turn of `engine`, writing to `out` as `output` says; returns the
/// exit code. After the turn the session is shut down and its remaining events are read
/// (and, as JSON, written) until the stream ends, so nothing the session started outlives
/// this call. Exit code 1 when the session ends, or reports an error, before the turn
/// starts. If `out` cannot be written, the turn is cancelled.
pub async fn run_oneshot(
    engine: Engine,
    mut events: mpsc::Receiver<Event>,
    prompt: String,
    output: OutputFormat,
    out: impl AsyncWrite + Unpin,
) -> i32 {
    let mut out = Printer {
        out,
        output,
        broken: false,
        last_text: None,
    };
    let mut code = None;
    if engine
        .send(Command::SendMessage { text: prompt })
        .await
        .is_ok()
    {
        let (mut started, mut cancelled) = (false, false);
        while let Some(ev) = events.recv().await {
            out.print(&ev).await;
            if out.broken && !cancelled {
                // Nobody reads the output: stop the turn instead of finishing it unseen.
                cancelled = true;
                let _ = engine.send(Command::Cancel).await;
            }
            match ev {
                Event::TurnStarted => started = true,
                Event::TurnFinished { reason, .. } => {
                    code = Some(exit_code(reason));
                    break;
                }
                Event::Error { .. } if !started => {
                    code = Some(1);
                    break;
                }
                _ => {}
            }
        }
    }
    let _ = engine.send(Command::Shutdown).await;
    drop(engine);
    while let Some(ev) = events.recv().await {
        if output == OutputFormat::Json {
            out.print(&ev).await;
        } else if let Event::Error { .. } = ev {
            out.print(&ev).await;
        }
    }
    out.finish().await;
    code.unwrap_or(1)
}

struct Printer<W> {
    out: W,
    output: OutputFormat,
    /// A write failed: nothing more is written.
    broken: bool,
    /// The last character of text written (text output).
    last_text: Option<char>,
}

impl<W: AsyncWrite + Unpin> Printer<W> {
    async fn print(&mut self, ev: &Event) {
        match self.output {
            OutputFormat::Json => {
                let Ok(mut line) = serde_json::to_string(ev) else {
                    return;
                };
                line.push('\n');
                self.write(line.as_bytes()).await;
            }
            OutputFormat::Text => match ev {
                Event::TextDelta { text } => {
                    if let Some(c) = text.chars().last() {
                        self.last_text = Some(c);
                    }
                    self.write(text.as_bytes()).await;
                }
                Event::Error { message, hint, .. } => {
                    let hint = hint
                        .as_deref()
                        .map(|h| format!(" ({h})"))
                        .unwrap_or_default();
                    let _ = writeln!(std::io::stderr(), "tautly: {message}{hint}");
                }
                _ => {}
            },
        }
    }

    async fn write(&mut self, bytes: &[u8]) {
        if self.broken {
            return;
        }
        if self.out.write_all(bytes).await.is_err() || self.out.flush().await.is_err() {
            self.broken = true;
        }
    }

    /// End text output with a newline.
    async fn finish(&mut self) {
        if self.output == OutputFormat::Text && self.last_text.is_some_and(|c| c != '\n') {
            self.write(b"\n").await;
        }
        if !self.broken {
            let _ = self.out.flush().await;
        }
    }
}
