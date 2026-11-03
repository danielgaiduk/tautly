use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tautly_provider::{Capabilities, ToolSpec};
use tautly_sandbox::{Effect, Sandbox, SpawnSpec, parse_commands};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, BufReader};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio::time::Instant;

use crate::cap::{OUTPUT_CAP, cap_text};
use crate::filters;
use crate::procs::{GroupGuard, group_alive, killpg};
use crate::spec::{parse_args, tool_spec};
use crate::{AppliedEffect, Tool, ToolCx, ToolError, ToolResult};

const DEFAULT_TIMEOUT_S: u64 = 120;
const MAX_TIMEOUT_S: u64 = 600;
/// Output at or below this size is returned as is (up to the cap); above it, a
/// recognized tool's output is summarized.
const FILTER_THRESHOLD: usize = 2 * 1024;
/// Grace between SIGTERM and SIGKILL.
const TERM_GRACE: Duration = Duration::from_millis(100);
/// How often the group is checked during [`TERM_GRACE`].
const KILL_POLL: Duration = Duration::from_millis(5);
/// Absolute cap on how long to keep reading pipes after the shell exits (background children may hold them).
const DRAIN_GRACE: Duration = Duration::from_millis(200);
/// Upper bound on output kept in memory; the rest is drained and discarded.
const MAX_CAPTURE: usize = 16 * 1024 * 1024;
/// Longest piece of output read as one unit; longer lines are split, so a
/// newline-free flood never sits in memory whole.
const MAX_CHUNK: usize = 64 * 1024;
/// Chunks in flight from the pipe readers (at most `CHANNEL_CHUNKS * MAX_CHUNK`
/// bytes); a full channel blocks the readers, and so the command, instead of growing.
const CHANNEL_CHUNKS: usize = 32;

/// Removed from the child's inherited environment.
const SHELL_STATE_ENV: &[&str] = &[
    "CDPATH",
    "BASH_ENV",
    "ENV",
    "SHELLOPTS",
    "BASHOPTS",
    "PROMPT_COMMAND",
    "PS4",
];

pub struct BashTool;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    command: String,
    timeout_s: Option<u64>,
    /// Skip output filtering.
    raw: Option<bool>,
}

fn parse(args: &Value) -> Result<Args, ToolError> {
    let a: Args = parse_args(args)?;
    if let Some(t) = a.timeout_s
        && !(1..=MAX_TIMEOUT_S).contains(&t)
    {
        return Err(ToolError::BadArgs(format!(
            "timeout_s must be between 1 and {MAX_TIMEOUT_S}"
        )));
    }
    Ok(a)
}

/// Cap `full` to `cap` bytes (head + tail). When it was cut, the complete text is spilled
/// through the sandbox and its root-relative path returned.
pub async fn cap_output(full: &str, cap: usize, sb: &dyn Sandbox) -> (String, Option<PathBuf>) {
    let (text, truncated) = cap_text(full, cap);
    if !truncated {
        return (text, None);
    }
    (text, sb.spill(full.as_bytes()).await.ok())
}

/// The filtered summary of `full` when `command` ends in a recognized tool and its
/// output is large enough and parseable. The complete output is spilled and named in
/// the summary. `None` (use the plain cap) when any of that does not hold, when the
/// run was cut short (`exit_code` is `None`: killed, timed out or cancelled, so the
/// output is partial), or when the command failed but the summary shows no failure
/// or error, or the output holds a crash the summary leaves out (the parser missed
/// what went wrong).
async fn filtered_output(
    command: &str,
    full: &str,
    exit_code: Option<i32>,
    sb: &dyn Sandbox,
) -> Option<String> {
    let code = exit_code?;
    if full.len() <= FILTER_THRESHOLD {
        return None;
    }
    let summary = filters::apply(filters::detect_command(command)?, full)?;
    if code != 0
        && (summary.failed + summary.errors == 0 || filters::has_unparsed_crash(full, &summary))
    {
        return None;
    }
    let spill = sb.spill(full.as_bytes()).await.ok()?;
    Some(cap_text(&summary.render(&spill), OUTPUT_CAP).0)
}

enum Outcome {
    Exited(ExitStatus),
    TimedOut,
    Cancelled,
}

/// Read `r` in chunks into `tx`. The task lives in `set`, so dropping the set (when
/// the call returns, even if a background child still holds the pipe open) aborts
/// it and closes our end of the pipe.
fn spawn_reader<R: AsyncRead + Unpin + Send + 'static>(
    set: &mut JoinSet<()>,
    r: R,
    tx: mpsc::Sender<Vec<u8>>,
) {
    set.spawn(async move {
        let mut r = BufReader::with_capacity(MAX_CHUNK, r);
        loop {
            match read_chunk(&mut r).await {
                Ok(chunk) if chunk.is_empty() => break,
                Ok(chunk) => {
                    if tx.send(chunk).await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
}

/// Bytes up to and including the next newline, or `MAX_CHUNK` bytes, whichever
/// comes first. Empty at end of input.
async fn read_chunk<R: AsyncBufRead + Unpin>(r: &mut R) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let buf = r.fill_buf().await?;
        if buf.is_empty() {
            return Ok(out);
        }
        let room = MAX_CHUNK - out.len();
        let window = &buf[..buf.len().min(room)];
        let (n, done) = match window.iter().position(|&b| b == b'\n') {
            Some(i) => (i + 1, true),
            None => (window.len(), window.len() == room),
        };
        out.extend_from_slice(&window[..n]);
        r.consume(n);
        if done {
            return Ok(out);
        }
    }
}

struct Capture<'a> {
    buf: Vec<u8>,
    tx: Option<&'a mpsc::Sender<String>>,
}

impl Capture<'_> {
    fn push(&mut self, line: Vec<u8>) {
        if let Some(tx) = self.tx {
            // Bounded and lossy by design: never block the command on a slow consumer.
            let _ = tx.try_send(String::from_utf8_lossy(&line).into_owned());
        }
        let room = MAX_CAPTURE.saturating_sub(self.buf.len());
        self.buf.extend_from_slice(&line[..line.len().min(room)]);
    }
}

/// Marks the code index stale when dropped and starts a background refresh to catch it up.
struct StaleOnDrop(Option<std::sync::Arc<tautly_index::Index>>);

impl Drop for StaleOnDrop {
    fn drop(&mut self) {
        if let Some(index) = &self.0 {
            index.mark_stale();
            // Coalesced with any refresh already running; errors land in `last_error`.
            drop(index.refresh_stale_in_background());
        }
    }
}

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &'static str {
        "bash"
    }

    fn spec(&self, _caps: &Capabilities) -> ToolSpec {
        tool_spec(
            "bash",
            "Run a shell command (sh -c) in the project root. Returns combined stdout and stderr \
             followed by the exit code. Long output is truncated to head and tail with the full \
             text saved to a file you can read.",
            json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "Command line to run."},
                    "timeout_s": {
                        "type": "integer", "minimum": 1, "maximum": MAX_TIMEOUT_S,
                        "description": "Timeout in seconds (default 120)."
                    },
                    "raw": {"type": "boolean", "description": "Skip output filtering."}
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        )
    }

    fn effect(&self, args: &Value) -> Result<Effect, ToolError> {
        let a = parse(args)?;
        let argv = parse_commands(&a.command)
            .ok()
            .and_then(|cmds| cmds.into_iter().next())
            .map(|c| c.argv)
            .unwrap_or_else(|| vec!["/bin/sh".into(), "-c".into(), a.command.clone()]);
        Ok(Effect::Exec { argv })
    }

    async fn run(&self, args: Value, sb: &dyn Sandbox, cx: &ToolCx) -> ToolResult {
        // Whatever the command did to the files (even if cancelled half way), the index
        // can no longer vouch for them until it catches up.
        let _stale = StaleOnDrop(cx.index().cloned());
        let a = match parse(&args) {
            Ok(a) => a,
            Err(e) => return ToolResult::error(e.to_string()),
        };
        let timeout_s = a.timeout_s.unwrap_or(DEFAULT_TIMEOUT_S);
        let spec = SpawnSpec {
            argv: vec!["/bin/sh".into(), "-c".into(), a.command.clone()],
            cwd: Path::new(".").to_path_buf(),
            // Overrides only: the sandbox spawn inherits the rest of the environment.
            env: vec![
                ("TAUTLY".into(), "1".into()),
                ("PAGER".into(), "cat".into()),
                ("GIT_PAGER".into(), "cat".into()),
                ("TERM".into(), "dumb".into()),
            ],
            // Inherited values that would change how the shell runs the command
            // (`cd` targets, startup files, options, tracing hooks) behind the policy.
            env_remove: SHELL_STATE_ENV.iter().map(|v| v.to_string()).collect(),
            new_process_group: true,
        };
        let mut child = match sb.spawn(spec).await {
            Ok(c) => c,
            Err(e) => return ToolResult::error(format!("failed to start: {e}")),
        };
        let pgid = child.id();
        // Kills the group if this future is dropped before the command is settled.
        let guard = pgid.map(|p| GroupGuard::new(cx.process_groups(), p));
        let (line_tx, mut lines) = mpsc::channel(CHANNEL_CHUNKS);
        let mut readers = JoinSet::new();
        if let Some(out) = child.stdout.take() {
            spawn_reader(&mut readers, out, line_tx.clone());
        }
        if let Some(err) = child.stderr.take() {
            spawn_reader(&mut readers, err, line_tx.clone());
        }
        drop(line_tx);

        let mut cap = Capture {
            buf: Vec::new(),
            tx: cx.output_tx(),
        };
        let deadline = Instant::now() + Duration::from_secs(timeout_s);
        let mut exit: Option<ExitStatus> = None;
        let mut pipes_open = true;
        let mut drain_until: Option<Instant> = None;
        let outcome = loop {
            if let (Some(status), false) = (exit, pipes_open) {
                break Outcome::Exited(status);
            }
            // Checked explicitly: with a flooding background child `recv` is always ready
            // and the biased select below would never reach the drain timer.
            if let (Some(status), Some(d)) = (exit, drain_until)
                && Instant::now() >= d
            {
                break Outcome::Exited(status);
            }
            tokio::select! {
                biased;
                _ = cx.cancel().cancelled(), if exit.is_none() => break Outcome::Cancelled,
                _ = tokio::time::sleep_until(deadline), if exit.is_none() => break Outcome::TimedOut,
                line = lines.recv(), if pipes_open => match line {
                    Some(l) => {
                        cap.push(l);
                    }
                    None => pipes_open = false,
                },
                _ = tokio::time::sleep_until(drain_until.unwrap_or(deadline)), if drain_until.is_some() => {
                    if let Some(status) = exit {
                        break Outcome::Exited(status);
                    }
                }
                r = child.wait(), if exit.is_none() => match r {
                    Ok(status) => {
                        exit = Some(status);
                        drain_until = Some(Instant::now() + DRAIN_GRACE);
                    }
                    Err(e) => {
                        // The command started, so it may have written files.
                        let mut r = ToolResult::error(format!("wait failed: {e}"));
                        r.effects.push(AppliedEffect::Exec { may_write: true });
                        return r;
                    }
                },
            }
        };

        if !matches!(outcome, Outcome::Exited(_)) {
            if let Some(pgid) = pgid {
                killpg(pgid, libc::SIGTERM);
                // Most commands die on SIGTERM at once: wait only while the group lives.
                let end = Instant::now() + TERM_GRACE;
                loop {
                    // Reap the shell, so an exited group does not linger as a zombie.
                    let _ = child.try_wait();
                    if !group_alive(pgid) || Instant::now() >= end {
                        break;
                    }
                    tokio::time::sleep(KILL_POLL).await;
                }
                killpg(pgid, libc::SIGKILL);
            }
            let _ = child.wait().await;
            // Collect what was already written before the kill.
            let end = Instant::now() + TERM_GRACE;
            while let Ok(Some(l)) = tokio::time::timeout_at(end, lines.recv()).await {
                cap.push(l);
            }
        }

        // A group with a background child left (a dev server) stays registered, so the
        // session ends it at shutdown.
        if let Some(g) = guard {
            g.settle();
        }
        let (is_error, trailer, exit_code) = match outcome {
            Outcome::Exited(s) => {
                let code = s.code().unwrap_or_else(|| {
                    use std::os::unix::process::ExitStatusExt;
                    128 + s.signal().unwrap_or(0)
                });
                (code != 0, format!("exit code: {code}"), Some(code))
            }
            Outcome::TimedOut => (true, format!("killed: timed out after {timeout_s}s"), None),
            Outcome::Cancelled => (true, "killed: cancelled".to_string(), None),
        };
        // Stop the readers now (a background child may still hold the pipes) and
        // release what is in flight.
        drop(readers);
        drop(lines);
        let full = match String::from_utf8(cap.buf) {
            Ok(s) => s,
            Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
        };
        let filtered = if a.raw == Some(true) {
            None
        } else {
            filtered_output(&a.command, &full, exit_code, sb).await
        };
        // A summary already names the spilled file; otherwise the cap spills when it cuts.
        let (mut text, spilled) = match filtered {
            Some(summary) => (summary, None),
            None => cap_output(&full, OUTPUT_CAP, sb).await,
        };
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        if let Some(p) = spilled {
            text.push_str(&format!("[full output saved to {}]\n", p.display()));
        }
        text.push_str(&trailer);
        let mut r = ToolResult::ok(text);
        r.is_error = is_error;
        r.meta.exit_code = exit_code;
        r.effects.push(AppliedEffect::Exec { may_write: true });
        r
    }
}
