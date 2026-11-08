//! `tautly -p "<prompt>"` (spec 3.4): send one message, stream what happens to the
//! output, then shut the session down and exit with a code mapped from how the turn ended
//! (0 completed, 1 error, 2 budget / max_turns / timeout, 130 cancelled).
//!
//! The session is headless (the caller builds it so): `ask` resolves to deny unless
//! `--approve` raised the mode, memory proposals are rejected unless `--approve-memory`,
//! and nothing ever waits for input. In text mode, what the user should know about those
//! refusals goes to stderr, so stdout stays the model's text.

use tautly_core::{Command, Engine, Event, FinishReason, McpState};
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

/// What `--output` prints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    /// Every event, one JSON object per line (the protocol's `Event`), until the session
    /// has shut down.
    Json,
    /// The model's text only, ending with a newline. Errors, warnings, headless refusals
    /// (a tool call denied or blocked) and MCP servers skipped or failed go to stderr, one
    /// `tautly: ...` line each.
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
    events: mpsc::Receiver<Event>,
    prompt: String,
    output: OutputFormat,
    out: impl AsyncWrite + Unpin,
) -> i32 {
    run_oneshot_to(engine, events, prompt, output, out, tokio::io::stderr()).await
}

/// [`run_oneshot`] with the text mode's diagnostics written to `err` instead of the
/// process's stderr.
pub async fn run_oneshot_to(
    engine: Engine,
    mut events: mpsc::Receiver<Event>,
    prompt: String,
    output: OutputFormat,
    out: impl AsyncWrite + Unpin,
    err: impl AsyncWrite + Unpin,
) -> i32 {
    let mut out = Printer {
        out,
        err,
        output,
        broken: false,
        last_text: None,
        step_break: false,
    };
    let mut code = None;
    if engine
        .send(Command::SendMessage {
            id: None,
            text: prompt,
        })
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
        if !matches!(ev, Event::TextDelta { .. }) {
            out.print(&ev).await;
        }
    }
    out.finish().await;
    code.unwrap_or(1)
}

struct Printer<W, E> {
    out: W,
    /// Diagnostics (text output).
    err: E,
    output: OutputFormat,
    /// A write failed: nothing more is written.
    broken: bool,
    /// The last character of text written (text output).
    last_text: Option<char>,
    /// A tool ran since the last text: the next text starts a new paragraph.
    step_break: bool,
}

impl<W: AsyncWrite + Unpin, E: AsyncWrite + Unpin> Printer<W, E> {
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
                    if !text.is_empty()
                        && std::mem::take(&mut self.step_break)
                        && let Some(last) = self.last_text
                    {
                        self.write(if last == '\n' { b"\n" } else { b"\n\n" }).await;
                    }
                    if let Some(c) = text.chars().last() {
                        self.last_text = Some(c);
                    }
                    self.write(text.as_bytes()).await;
                }
                ev => {
                    if matches!(ev, Event::ToolFinished { .. }) {
                        self.step_break = true;
                    }
                    if let Some(line) = diagnostic(ev) {
                        // On a terminal stdout and stderr share lines: end the text's line.
                        if self.last_text.is_some_and(|c| c != '\n') {
                            self.write(b"\n").await;
                            self.last_text = Some('\n');
                        }
                        // Messages may quote repository text (CP5 review M1).
                        let line = format!("tautly: {}\n", tautly_core::printable_multiline(&line));
                        // Diagnostics are best effort: a closed stderr changes nothing.
                        let _ = self.err.write_all(line.as_bytes()).await;
                        let _ = self.err.flush().await;
                    }
                }
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

/// What text output tells the user on stderr about `ev`, if anything: errors, warnings
/// (a rejected memory proposal names `--approve-memory`), tool calls refused headless or
/// blocked (`--approve`), and MCP servers skipped (`--approve-mcp`) or failed.
fn diagnostic(ev: &Event) -> Option<String> {
    let first_line = |text: &str| text.lines().next().unwrap_or("").trim().to_string();
    match ev {
        Event::Error { message, hint, .. } => Some(match hint {
            Some(hint) => format!("{message} ({hint})"),
            None => message.clone(),
        }),
        Event::Warning { message } => Some(format!("warning: {message}")),
        Event::ToolFinished {
            tool,
            output,
            blocked: true,
            ..
        } => Some(format!("{tool} blocked: {}", first_line(output))),
        Event::ToolFinished {
            tool,
            output,
            is_error: true,
            ..
        } => output
            .strip_prefix("denied:")
            .map(|why| format!("{tool} denied: {}", first_line(why))),
        Event::McpServerStatus {
            server,
            state,
            message: Some(message),
            ..
        } => {
            let state = match state {
                McpState::Skipped => "skipped",
                McpState::Failed => "failed",
                _ => return None,
            };
            Some(format!(
                "MCP server {server} {state}: {}",
                first_line(message)
            ))
        }
        _ => None,
    }
}
