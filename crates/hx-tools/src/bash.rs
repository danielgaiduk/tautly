use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::time::Duration;

use async_trait::async_trait;
use hx_provider::{Capabilities, ToolSpec};
use hx_sandbox::{Effect, Sandbox, SpawnSpec, parse_commands};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::cap::{OUTPUT_CAP, cap_text};
use crate::spec::{parse_args, tool_spec};
use crate::{Tool, ToolCx, ToolError, ToolResult};

const DEFAULT_TIMEOUT_S: u64 = 120;
const MAX_TIMEOUT_S: u64 = 600;
/// Grace between SIGTERM and SIGKILL.
const TERM_GRACE: Duration = Duration::from_millis(100);
/// Absolute cap on how long to keep reading pipes after the shell exits (background children may hold them).
const DRAIN_GRACE: Duration = Duration::from_millis(200);
/// Upper bound on output kept in memory; the rest is drained and discarded.
const MAX_CAPTURE: usize = 16 * 1024 * 1024;

pub struct BashTool;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    command: String,
    timeout_s: Option<u64>,
    /// Reserved for the output-filter task; accepted and ignored for now.
    #[allow(dead_code)]
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

enum Outcome {
    Exited(ExitStatus),
    TimedOut,
    Cancelled,
}

fn spawn_reader<R: AsyncRead + Unpin + Send + 'static>(r: R, tx: mpsc::UnboundedSender<Vec<u8>>) {
    tokio::spawn(async move {
        let mut r = BufReader::new(r);
        loop {
            let mut line = Vec::new();
            match r.read_until(b'\n', &mut line).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if tx.send(line).is_err() {
                        break;
                    }
                }
            }
        }
    });
}

fn killpg(pgid: u32, sig: libc::c_int) {
    let Ok(pgid) = libc::pid_t::try_from(pgid) else {
        return;
    };
    // SAFETY: killpg has no memory-safety preconditions; failure (e.g. ESRCH) is harmless.
    unsafe {
        libc::killpg(pgid, sig);
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
        if self.buf.len() < MAX_CAPTURE {
            self.buf.extend_from_slice(&line);
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
        let a = match parse(&args) {
            Ok(a) => a,
            Err(e) => return ToolResult::error(e.to_string()),
        };
        let timeout_s = a.timeout_s.unwrap_or(DEFAULT_TIMEOUT_S);
        let spec = SpawnSpec {
            argv: vec!["/bin/sh".into(), "-c".into(), a.command],
            cwd: Path::new(".").to_path_buf(),
            // Overrides only: the sandbox spawn inherits the rest of the environment.
            env: vec![
                ("HX".into(), "1".into()),
                ("PAGER".into(), "cat".into()),
                ("GIT_PAGER".into(), "cat".into()),
                ("TERM".into(), "dumb".into()),
            ],
            new_process_group: true,
        };
        let mut child = match sb.spawn(spec).await {
            Ok(c) => c,
            Err(e) => return ToolResult::error(format!("failed to start: {e}")),
        };
        let pgid = child.id();
        let (line_tx, mut lines) = mpsc::unbounded_channel();
        if let Some(out) = child.stdout.take() {
            spawn_reader(out, line_tx.clone());
        }
        if let Some(err) = child.stderr.take() {
            spawn_reader(err, line_tx.clone());
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
                    Err(e) => return ToolResult::error(format!("wait failed: {e}")),
                },
            }
        };

        if !matches!(outcome, Outcome::Exited(_)) {
            if let Some(pgid) = pgid {
                killpg(pgid, libc::SIGTERM);
                tokio::time::sleep(TERM_GRACE).await;
                killpg(pgid, libc::SIGKILL);
            }
            let _ = child.wait().await;
            // Collect what was already written before the kill.
            let end = Instant::now() + TERM_GRACE;
            while let Ok(Some(l)) = tokio::time::timeout_at(end, lines.recv()).await {
                cap.push(l);
            }
        }

        let (is_error, trailer) = match outcome {
            Outcome::Exited(s) => {
                let code = s.code().unwrap_or_else(|| {
                    use std::os::unix::process::ExitStatusExt;
                    128 + s.signal().unwrap_or(0)
                });
                (code != 0, format!("exit code: {code}"))
            }
            Outcome::TimedOut => (true, format!("killed: timed out after {timeout_s}s")),
            Outcome::Cancelled => (true, "killed: cancelled".to_string()),
        };
        let full = String::from_utf8_lossy(&cap.buf).into_owned();
        let (mut text, spilled) = cap_output(&full, OUTPUT_CAP, sb).await;
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        if let Some(p) = spilled {
            text.push_str(&format!("[full output saved to {}]\n", p.display()));
        }
        text.push_str(&trailer);
        ToolResult {
            output: text,
            is_error,
            effects: Vec::new(),
        }
    }
}
