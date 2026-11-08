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
use crate::{AppliedEffect, SaveKind, Saved, TestCounts, Tool, ToolCx, ToolError, ToolResult};

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
/// The same cap when no process is left in the command's group: the pipes end as soon as
/// what was written is read, unless a process that left the group (setsid) holds them.
const EOF_DRAIN: Duration = Duration::from_secs(5);
/// Upper bound on output kept in memory when the sandbox offers no spill file to stream
/// to; the rest is drained and discarded.
const MAX_CAPTURE: usize = 16 * 1024 * 1024;
/// Output beyond this streams to a spill file, keeping only its head and tail in memory
/// (spec 9.1: 100 MB of output, bounded RSS). Such output is not summarized by the filters.
const STREAM_AT: usize = 4 * 1024 * 1024;
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

/// What a recognized tool's output says, parsed once for the filtered summary and the
/// counts: `None` for an unrecognized command, unparseable output, or a run that was cut
/// short (`exit_code` is `None`: killed, timed out or cancelled, so the output is partial).
fn parse_summary(command: &str, full: &str, exit_code: Option<i32>) -> Option<filters::Summary> {
    exit_code?;
    filters::apply(filters::detect_command(command)?, full)
}

/// Whether a failed run (`code != 0`) shows nothing the summary accounts for: no failure or
/// error in it, or a crash in the output that it leaves out (the parser missed what went
/// wrong), so neither the summary nor its counts can be trusted.
fn contradicts_exit(summary: &filters::Summary, full: &str, code: i32) -> bool {
    code != 0
        && (summary.failed + summary.errors == 0 || filters::has_unparsed_crash(full, summary))
}

/// Passed and failed counts of a test tool's summary (builds and linters report no passes);
/// a crashed test binary or collection error counts as a failure. None when the summary
/// contradicts the exit code ([`contradicts_exit`]): the footer must not say "4 passed,
/// 0 failed" about a run that exited non-zero.
fn test_counts(
    summary: &filters::Summary,
    full: &str,
    exit_code: Option<i32>,
) -> Option<TestCounts> {
    if contradicts_exit(summary, full, exit_code?) {
        return None;
    }
    Some(TestCounts {
        passed: summary.passed?,
        failed: summary.failed.saturating_add(summary.errors),
    })
}

/// The filtered summary of `full` when `summary` (see [`parse_summary`]) exists and the
/// output is large enough. The complete output is spilled and named in the summary.
/// `None` (use the plain cap) when any of that does not hold, when the command failed but
/// the summary shows no failure or error, or the output holds a crash the summary leaves
/// out (the parser missed what went wrong).
async fn filtered_output(
    summary: Option<&filters::Summary>,
    full: &str,
    exit_code: Option<i32>,
    sb: &dyn Sandbox,
) -> Option<String> {
    let code = exit_code?;
    if full.len() <= FILTER_THRESHOLD {
        return None;
    }
    let summary = summary?;
    if contradicts_exit(summary, full, code) {
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
    /// All of the output, or only its head once it streams to a spill file.
    buf: Vec<u8>,
    tx: Option<&'a mpsc::Sender<String>>,
    sb: &'a dyn Sandbox,
    stream: Option<Streamed>,
    /// The sandbox offers no spill file (or failed to make one): keep up to MAX_CAPTURE.
    in_memory: bool,
}

/// Output past [`STREAM_AT`], written to a spill file as it arrives.
struct Streamed {
    path: PathBuf,
    file: tautly_sandbox::SpillWriter,
    total: u64,
    /// The last bytes seen (at least [`OUTPUT_CAP`] once there are that many).
    tail: Vec<u8>,
    /// A write failed: the file is incomplete and is not offered.
    broken: bool,
}

impl Capture<'_> {
    async fn push(&mut self, line: Vec<u8>) {
        if let Some(tx) = self.tx {
            // Bounded and lossy by design: never block the command on a slow consumer.
            let _ = tx.try_send(String::from_utf8_lossy(&line).into_owned());
        }
        if self.stream.is_none() && !self.in_memory && self.buf.len() + line.len() > STREAM_AT {
            match self.sb.spill_stream().await {
                Ok(Some((path, file))) => {
                    let mut s = Streamed {
                        path,
                        file,
                        total: 0,
                        tail: Vec::new(),
                        broken: false,
                    };
                    // Only the head of what was held so far stays in memory.
                    let head = self.buf[..OUTPUT_CAP.min(self.buf.len())].to_vec();
                    s.write(&std::mem::take(&mut self.buf)).await;
                    self.buf = head;
                    self.stream = Some(s);
                }
                _ => self.in_memory = true,
            }
        }
        match &mut self.stream {
            Some(s) => s.write(&line).await,
            None => {
                let room = MAX_CAPTURE.saturating_sub(self.buf.len());
                self.buf.extend_from_slice(&line[..line.len().min(room)]);
            }
        }
    }
}

impl Streamed {
    async fn write(&mut self, bytes: &[u8]) {
        use tokio::io::AsyncWriteExt as _;
        if !self.broken && self.file.write_all(bytes).await.is_err() {
            self.broken = true;
        }
        self.total += bytes.len() as u64;
        self.tail.extend_from_slice(bytes);
        // Trimmed in batches, so each byte is moved a bounded number of times.
        if self.tail.len() > 2 * OUTPUT_CAP {
            self.tail.drain(..self.tail.len() - OUTPUT_CAP);
        }
    }

    /// The capped text (head, omitted-bytes marker, tail, as [`cap_text`] makes it) and
    /// the spill file, unless a write failed.
    async fn finish(mut self, head: &[u8]) -> (String, Option<PathBuf>) {
        use tokio::io::AsyncWriteExt as _;
        if self.file.flush().await.is_err() {
            self.broken = true;
        }
        let head = &head[..utf8_floor(head, OUTPUT_CAP / 2)];
        let keep = OUTPUT_CAP - OUTPUT_CAP / 2;
        let mut start = self.tail.len().saturating_sub(keep);
        // Not inside a UTF-8 sequence.
        while start < self.tail.len() && (self.tail[start] & 0xC0) == 0x80 {
            start += 1;
        }
        let tail = &self.tail[start..];
        let omitted = self.total - head.len() as u64 - tail.len() as u64;
        let text = format!(
            "{}\n… [{omitted} bytes omitted] …\n{}",
            String::from_utf8_lossy(head),
            String::from_utf8_lossy(tail)
        );
        (text, (!self.broken).then_some(self.path))
    }
}

/// The longest prefix of `b` up to `max` bytes that does not end inside a UTF-8 sequence.
fn utf8_floor(b: &[u8], max: usize) -> usize {
    let mut end = max.min(b.len());
    while end > 0 && end < b.len() && (b[end] & 0xC0) == 0x80 {
        end -= 1;
    }
    end
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
            // (`cd` targets, startup files, options, tracing hooks) behind the policy, and
            // the session's provider API keys (see `ToolCx::with_env_remove`).
            env_remove: SHELL_STATE_ENV
                .iter()
                .map(|v| v.to_string())
                .chain(cx.env_remove().iter().cloned())
                .collect(),
            // Its own group, so cancel and shutdown can end everything it started. What
            // leaves the group (setsid, job control, daemonizers) is not reached; see
            // `procs` for the list and the mitigations.
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
            sb,
            stream: None,
            in_memory: false,
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
                    Some(l) => cap.push(l).await,
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
                        // Nothing left in the group: only what was already written is in
                        // the pipes, so read it to the end (under load that can take longer
                        // than DRAIN_GRACE). A background child may hold them forever.
                        let grace = match pgid {
                            Some(p) if !group_alive(p) => EOF_DRAIN,
                            _ => DRAIN_GRACE,
                        };
                        drain_until = Some(Instant::now() + grace);
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
                cap.push(l).await;
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
        let streamed = match cap.stream.take() {
            Some(s) => Some(s.finish(&cap.buf).await),
            None => None,
        };
        // Output that streamed to a spill file is too large to summarize: head and tail.
        let full = match (&streamed, String::from_utf8(cap.buf)) {
            (Some(_), _) => String::new(),
            (None, Ok(s)) => s,
            (None, Err(e)) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
        };
        let summary = parse_summary(&a.command, &full, exit_code).filter(|_| streamed.is_none());
        let filtered = if a.raw == Some(true) || streamed.is_some() {
            None
        } else {
            filtered_output(summary.as_ref(), &full, exit_code, sb).await
        };
        let was_filtered = filtered.is_some();
        // A summary already names the spilled file; otherwise the cap spills when it cuts.
        let (mut text, spilled) = match (streamed, filtered) {
            (Some(streamed), _) => streamed,
            (None, Some(summary)) => (summary, None),
            (None, None) => cap_output(&full, OUTPUT_CAP, sb).await,
        };
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        if let Some(p) = spilled {
            text.push_str(&format!("[full output saved to {}]\n", p.display()));
        }
        text.push_str(&trailer);
        let mut r = ToolResult::ok(text);
        // A plain harness would have returned the capped raw output (spec 7.11.1).
        if was_filtered {
            r.meta.saved = Saved::of(SaveKind::Filter, full.len(), r.output.len());
        }
        r.is_error = is_error;
        r.meta.exit_code = exit_code;
        r.meta.counts = summary
            .as_ref()
            .and_then(|s| test_counts(s, &full, exit_code));
        r.effects.push(AppliedEffect::Exec { may_write: true });
        r
    }
}
