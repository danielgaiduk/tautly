//! `tautly eval` (spec 9.2, 9.3): run eval tasks headless and measure them.
//!
//! - `tautly eval --tasks <dir> --model <id> [--edit-format str_replace|patch|hashline]
//!   [--jobs N] [--trials N] [--out results.json]` runs every task of `<dir>` (each a
//!   directory with `task.toml`, `setup.sh` and `verify.sh`; see `evals/README.md`) `N`
//!   times. A run gets a fresh, private (0700) temporary directory, never the current work
//!   tree: `setup.sh` makes the task's repository there, `tautly -p <prompt> --output json
//!   --eval-run` works on it under the normal policy and sandbox (approvals only as the
//!   task file says, a budget of its `budget_usd`, killed after `timeout_s`; its session is
//!   marked `origin: eval`), and `verify.sh` decides (exit 0 passes), given the commit
//!   setup made as `EVAL_BASE` (read by the hardened git before the agent runs). The
//!   directory is removed afterwards, whatever happened (a failure, Ctrl-C, a panic), and
//!   so are the run's code index, lock, memory episodes and, without `--keep-logs`, its
//!   session; the cost ledger keeps its rows. The scripts run like `bisect`'s checks:
//!   through the bash tool, offline, without the provider keys in their environment,
//!   killed with their process groups.
//! - Figures come from the run's event stream: tokens and cost from its `usage` events
//!   (each priced at its own model by the run; the session-end episode's usage arrives
//!   after the turn finished and is counted, and also shown on its own), edit failures
//!   from failed `edit` and `write` results, blocked attempts, and edit repairs by stage
//!   from `meta.repair` (absent in older streams). Results JSON: `{ model, edit_format,
//!   tasks: [...], summary }`, one task entry per task and trial; `summary` adds `trials`
//!   and the `mean` and `stdev` of each figure across trials.
//! - `tautly eval --replay <dir>` runs the tasks of `<dir>/../starter` that have a scripted
//!   provider session `<dir>/<id>.json` (a build with the `mock` feature) and compares
//!   tokens per solved task with `<dir>/../baseline.json`: more than 10% over it exits 1;
//!   `--update-baseline` rewrites it.
//! - `tautly eval report <results.json>... [--markdown]` prints the comparison table:
//!   model, pass rate, tokens per solved task, cache hit ratio and cost. A file holds one
//!   results object or an array of them.
//!
//! Exit codes: 0 (whatever the pass rate), 1 an error or a token regression, 128 + the
//! signal after Ctrl-C, SIGTERM or SIGHUP (the results so far are still written).

use std::collections::{BTreeMap, VecDeque};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context as _, bail};
use futures::StreamExt as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tautly_core::bisect::{BashRunner, CancellationToken, TestRunner as _, Verdict};
use tautly_core::config::CheckpointConfig;
use tautly_core::{
    Checkpointer, CliOverrides, Config, printable, printable_multiline, provider_key_vars,
};
use tautly_sandbox::LocalSandbox;
use tokio::io::{AsyncBufReadExt as _, AsyncRead, BufReader};

use super::bisect::Signals;
use super::{cost_cell, pct, tokens, usd};
use crate::Cli;

/// Tokens per solved task may exceed the baseline by this share (spec 9.1).
const REGRESSION_LIMIT: f64 = 0.10;
/// The longest a setup or verify script may run (the bash tool's cap).
const MAX_SCRIPT_S: u64 = 600;
/// How long a run gets to shut down after SIGTERM before its group is killed.
const KILL_GRACE: Duration = Duration::from_secs(5);
/// Lines of a failed run's stderr kept for the message.
const STDERR_LINES: usize = 20;
const EDIT_FORMATS: [&str; 3] = ["str_replace", "patch", "hashline"];

#[derive(Debug, clap::Args)]
#[command(args_conflicts_with_subcommands = true)]
pub struct Args {
    #[command(subcommand)]
    report: Option<Report>,
    /// The task directory: one subdirectory per task (task.toml, setup.sh, verify.sh).
    #[arg(long, value_name = "DIR")]
    tasks: Option<PathBuf>,
    /// The model every run uses, as <provider>/<model>.
    #[arg(long, value_name = "ID", required_unless_present = "replay")]
    model: Option<String>,
    /// The edit tool's format instead of the model's own.
    #[arg(long, value_name = "FORMAT",
          value_parser = clap::builder::PossibleValuesParser::new(EDIT_FORMATS))]
    edit_format: Option<String>,
    /// Runs at once.
    #[arg(long, value_name = "N", default_value_t = 1,
          value_parser = clap::value_parser!(u32).range(1..=64))]
    jobs: u32,
    /// Runs of each task.
    #[arg(long, value_name = "N", default_value_t = 1,
          value_parser = clap::value_parser!(u32).range(1..=100))]
    trials: u32,
    /// Write the results JSON here (else to stdout, with the summary on stderr).
    #[arg(long, value_name = "FILE")]
    out: Option<PathBuf>,
    /// Replay the scripted sessions of DIR and compare with the baseline.
    #[arg(long, value_name = "DIR")]
    replay: Option<PathBuf>,
    /// With --replay: write the measured figure as the new baseline.
    #[arg(long, requires = "replay")]
    update_baseline: bool,
    /// Keep each run's session log in the data home (removed by default).
    #[arg(long)]
    keep_logs: bool,
}

#[derive(Debug, clap::Subcommand)]
enum Report {
    /// Print the comparison table of results files.
    Report {
        #[arg(required = true, value_name = "RESULTS")]
        files: Vec<PathBuf>,
        /// As a markdown table.
        #[arg(long)]
        markdown: bool,
    },
}

pub async fn run(args: Args, cli: &Cli) -> anyhow::Result<i32> {
    if let Some(Report::Report { files, markdown }) = &args.report {
        return report(files, *markdown);
    }
    #[cfg(feature = "mock")]
    let mock_script = cli.mock_script.as_deref().map(absolute).transpose()?;
    #[cfg(not(feature = "mock"))]
    let mock_script: Option<PathBuf> = {
        let _ = cli;
        None
    };
    if args.replay.is_some() && !cfg!(feature = "mock") {
        bail!(
            "--replay needs a build with the mock feature: cargo run -p tautly --features mock \
             -- eval --replay DIR"
        );
    }
    if let (Some(model), None, None) = (&args.model, &mock_script, &args.replay) {
        crate::check_model(model).map_err(anyhow::Error::msg)?;
    }
    let replay = args.replay.as_deref().map(absolute).transpose()?;
    let tasks_dir = match (&args.tasks, &replay) {
        (Some(dir), _) => dir.clone(),
        (None, Some(r)) => r.parent().unwrap_or(r).join("starter"),
        (None, None) => bail!("--tasks DIR names the tasks to run"),
    };
    let mut tasks = load_tasks(&tasks_dir)?;
    if let Some(r) = &replay {
        tasks.retain(|t| r.join(format!("{}.json", t.id)).is_file());
        if tasks.is_empty() {
            bail!(
                "no task of {} has a script in {}",
                tasks_dir.display(),
                r.display()
            );
        }
    }
    let edit_format = args
        .edit_format
        .clone()
        // The replay scripts edit with str_replace arguments.
        .or_else(|| replay.as_ref().map(|_| "str_replace".to_string()));
    let config = Config::load(&std::env::current_dir()?, CliOverrides::default())
        .map(|l| l.config)
        .unwrap_or_default();
    let cancel = CancellationToken::new();
    let runner = Runner {
        exe: std::env::current_exe().context("cannot find the tautly executable")?,
        model: args.model.clone(),
        edit_format: edit_format.clone(),
        mock_script,
        replay: replay.clone(),
        env_remove: provider_key_vars(&config),
        cancel: cancel.clone(),
        data_home: tautly_core::data_home().context("cannot find the data directory")?,
        keep_logs: args.keep_logs,
    };
    let caught = Arc::new(AtomicI32::new(0));
    let watcher = tokio::spawn(Signals::new()?.cancel(cancel.clone(), Arc::clone(&caught)));
    // Queued trial by trial; with --jobs > 1 runs of the next trial may start before the
    // last one finishes, which is fine: every run is isolated.
    let jobs: Vec<(&Task, u32)> = (1..=args.trials)
        .flat_map(|n| tasks.iter().map(move |t| (t, n)))
        .collect();
    let mut entries: Vec<TaskResult> = futures::stream::iter(jobs)
        .map(|(t, n)| runner.run_one(t, n))
        .buffer_unordered(args.jobs as usize)
        .collect()
        .await;
    watcher.abort();
    entries.sort_by(|a, b| (&a.id, a.trial).cmp(&(&b.id, b.trial)));
    let summary = summarize(&entries, args.trials);
    let results = Results {
        model: args.model.clone(),
        edit_format,
        tasks: entries,
        summary,
    };
    let json = serde_json::to_string_pretty(&results)? + "\n";
    let text = summary_text(&results, tasks.len());
    match &args.out {
        Some(path) => {
            std::fs::write(path, &json)
                .with_context(|| format!("cannot write {}", path.display()))?;
            print!("{text}");
            println!("results: {}", printable(&path.to_string_lossy()));
        }
        None => {
            print!("{json}");
            eprint!("{text}");
        }
    }
    if cancel.is_cancelled() {
        eprintln!("tautly: eval cancelled; the runs in progress were stopped and removed");
        return Ok(128 + caught.load(Ordering::SeqCst).max(0));
    }
    match &replay {
        Some(dir) => regression_gate(dir, &results, args.update_baseline),
        None => Ok(0),
    }
}

/// `path` made absolute against the current directory (runs work elsewhere).
fn absolute(path: &Path) -> anyhow::Result<PathBuf> {
    std::path::absolute(path).with_context(|| format!("cannot resolve {}", path.display()))
}

// ------------------------------------------------------------------- tasks

/// One task, as its `task.toml` says.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Task {
    id: String,
    prompt: String,
    #[serde(default = "default_timeout")]
    timeout_s: u64,
    #[serde(default = "default_budget")]
    budget_usd: f64,
    #[serde(default)]
    #[allow(dead_code)] // Read by benchmark tooling, not by the runner.
    tags: Vec<String>,
    /// What runs without asking: nothing (headless asks are denied), `auto-edit` or
    /// `yolo`. Never the model's choice.
    #[serde(default)]
    approve: Option<Approve>,
    /// The task's directory (absolute).
    #[serde(skip)]
    dir: PathBuf,
}

fn default_timeout() -> u64 {
    600
}

fn default_budget() -> f64 {
    1.0
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum Approve {
    AutoEdit,
    Yolo,
}

/// Every task under `dir` (subdirectories with a `task.toml`), by id.
fn load_tasks(dir: &Path) -> anyhow::Result<Vec<Task>> {
    let dir = dir
        .canonicalize()
        .with_context(|| format!("cannot read the task directory {}", dir.display()))?;
    let mut tasks = Vec::new();
    for e in std::fs::read_dir(&dir).with_context(|| format!("cannot read {}", dir.display()))? {
        let path = e?.path();
        let file = path.join("task.toml");
        if file.is_file() {
            tasks.push(load_task(&path, &file)?);
        }
    }
    if tasks.is_empty() {
        bail!(
            "no task in {} (each is a directory with a task.toml)",
            dir.display()
        );
    }
    tasks.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(tasks)
}

fn load_task(dir: &Path, file: &Path) -> anyhow::Result<Task> {
    let text =
        std::fs::read_to_string(file).with_context(|| format!("cannot read {}", file.display()))?;
    let mut t: Task =
        toml::from_str(&text).with_context(|| format!("{} is not a task", file.display()))?;
    let name = dir.file_name().unwrap_or_default().to_string_lossy();
    let valid = |c: char| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.');
    if t.id != name || t.id.is_empty() || !t.id.chars().all(valid) {
        bail!(
            "{}: id must be the directory's name ({}), in letters, digits, `-`, `_` and `.`",
            file.display(),
            printable(&name)
        );
    }
    if t.timeout_s == 0 || !(t.budget_usd.is_finite() && t.budget_usd >= 0.0) {
        bail!(
            "{}: timeout_s must be positive and budget_usd a non-negative amount",
            file.display()
        );
    }
    for script in ["setup.sh", "verify.sh"] {
        if !dir.join(script).is_file() {
            bail!("{}: no {script}", dir.display());
        }
    }
    t.dir = dir.to_path_buf();
    Ok(t)
}

// ------------------------------------------------------------------- runs

/// What every run is started with.
struct Runner {
    exe: PathBuf,
    model: Option<String>,
    edit_format: Option<String>,
    /// `--mock-script` of this process, for every run.
    mock_script: Option<PathBuf>,
    /// `--replay`: each task's own script, `<dir>/<id>.json`.
    replay: Option<PathBuf>,
    env_remove: Vec<String>,
    cancel: CancellationToken,
    /// Where runs leave their sessions, code indexes and locks.
    data_home: PathBuf,
    keep_logs: bool,
}

/// One run's figures, from its event stream.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct TaskResult {
    id: String,
    trial: u32,
    /// The run's session (its log is kept only with `--keep-logs`).
    session_id: Option<String>,
    passed: bool,
    /// Why it did not pass: `setup_failed`, `budget`, `timeout`, `max_turns`,
    /// `cancelled`, `error` or `verify_failed`.
    reason: Option<String>,
    cost_usd: f64,
    /// Some usage was the engine's estimate.
    cost_estimated: bool,
    /// Some usage was on a model without a known price: `cost_usd` covers the rest.
    cost_partial: bool,
    /// The part of `cost_usd` spent after the turn finished (the session-end episode).
    episode_cost_usd: f64,
    /// Main-model tokens.
    tokens_in: u64,
    tokens_out: u64,
    cache_read: u64,
    /// Side-model tokens (input + output).
    side_tokens: u64,
    /// `edit` and `write` results, and those that failed.
    edit_calls: u32,
    edit_failures: u32,
    /// Calls the loop breaker refused.
    blocked_attempts: u32,
    edit_repairs_by_stage: BTreeMap<String, u32>,
    duration_s: f64,
}

/// How a run's process ended.
enum Ended {
    Exited,
    Timeout,
    Cancelled,
}

impl Runner {
    async fn run_one(&self, task: &Task, trial: u32) -> TaskResult {
        let started = Instant::now();
        let mut r = TaskResult {
            id: task.id.clone(),
            trial,
            ..TaskResult::default()
        };
        let reason = if self.cancel.is_cancelled() {
            Some("cancelled".to_string())
        } else {
            match self.attempt(task, &mut r).await {
                Ok(reason) => reason,
                Err(e) => {
                    eprintln!(
                        "tautly: task {}: {}",
                        printable(&task.id),
                        printable_multiline(&format!("{e:#}"))
                    );
                    Some("error".to_string())
                }
            }
        };
        r.passed = reason.is_none();
        r.reason = reason;
        r.duration_s = (started.elapsed().as_secs_f64() * 1000.0).round() / 1000.0;
        let verdict = match &r.reason {
            None => "PASS".to_string(),
            Some(why) => format!("FAIL ({why})"),
        };
        eprintln!(
            "{verdict} {} [trial {trial}] {:.1}s {}",
            printable(&task.id),
            r.duration_s,
            usd(Some(r.cost_usd))
        );
        r
    }

    /// Set up, run and verify `task` in a temporary directory removed on return (or
    /// unwinding); why it did not pass, if it did not.
    async fn attempt(&self, task: &Task, r: &mut TaskResult) -> anyhow::Result<Option<String>> {
        // Private: on a shared /tmp nobody else reads the task's files or the agent's edits.
        let tmp = tempfile::Builder::new()
            .prefix("tautly-eval-")
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()
            .context("cannot make a temporary directory")?;
        let work = tmp.path().canonicalize()?;
        let scripts = BashRunner::new(self.env_remove.clone(), self.cancel.clone());
        let script_timeout = Duration::from_secs(task.timeout_s.min(MAX_SCRIPT_S));
        let setup = scripts
            .run(
                &work,
                &bash(&task.dir.join("setup.sh"), None),
                script_timeout,
            )
            .await;
        if setup != Verdict::Good {
            return Ok(Some(if self.cancel.is_cancelled() {
                "cancelled".to_string()
            } else {
                eprintln!(
                    "tautly: the setup of {} failed:\n{}",
                    printable(&task.id),
                    printable_multiline(&scripts.last_output().unwrap_or_default())
                );
                "setup_failed".to_string()
            }));
        }
        // What the run leaves in the data home goes when this does (also on an error).
        let _artifacts = Artifacts {
            data_home: self.data_home.clone(),
            hash: tautly_session::paths::project_hash(&work),
            id: tautly_session::paths::project_id(&work),
            keep_logs: self.keep_logs,
        };
        // The commit setup made, for verify to compare protected files with: the agent
        // cannot rewrite it.
        let base = head_commit(&work).await;
        let (ended, finish, stderr) = self.run_tautly(task, &work, r).await?;
        let reason = match ended {
            Ended::Cancelled => Some("cancelled"),
            Ended::Timeout => Some("timeout"),
            Ended::Exited => match finish.as_deref() {
                Some("completed") => None,
                Some(why @ ("budget" | "timeout" | "max_turns" | "cancelled")) => Some(why),
                _ => {
                    eprintln!(
                        "tautly: the run of {} ended with an error:\n{}",
                        printable(&task.id),
                        printable_multiline(&stderr)
                    );
                    Some("error")
                }
            },
        };
        // The budget holds for everything the run spent, its session-end episode included.
        let reason = match reason {
            None if r.cost_usd > task.budget_usd => Some("budget"),
            other => other,
        };
        if let Some(why) = reason {
            return Ok(Some(why.to_string()));
        }
        let verify = scripts
            .run(
                &work,
                &bash(&task.dir.join("verify.sh"), base.as_deref()),
                script_timeout,
            )
            .await;
        Ok(match verify {
            Verdict::Good => None,
            _ if self.cancel.is_cancelled() => Some("cancelled".to_string()),
            _ => Some("verify_failed".to_string()),
        })
    }

    /// `tautly -p` on `work` until it exits, `task.timeout_s` passes or eval is cancelled,
    /// summing its events into `r`; how it ended, the turn's finish reason and the last
    /// lines of its stderr.
    async fn run_tautly(
        &self,
        task: &Task,
        work: &Path,
        r: &mut TaskResult,
    ) -> anyhow::Result<(Ended, Option<String>, String)> {
        let mut cmd = tokio::process::Command::new(&self.exe);
        let script = match &self.replay {
            Some(dir) => Some(dir.join(format!("{}.json", task.id))),
            None => self.mock_script.clone(),
        };
        if let Some(s) = script {
            cmd.arg("--mock-script").arg(s);
        }
        cmd.args(["-p", &task.prompt, "--output", "json", "--eval-run"])
            .args(["--budget-usd", &task.budget_usd.to_string()]);
        if let Some(m) = &self.model {
            cmd.args(["--model", m]);
        }
        if let Some(f) = &self.edit_format {
            cmd.args(["--edit-format", f]);
        }
        match task.approve {
            Some(Approve::AutoEdit) => _ = cmd.args(["--approve", "auto-edit"]),
            Some(Approve::Yolo) => _ = cmd.args(["--approve", "yolo"]),
            None => {}
        }
        cmd.current_dir(work)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Its own group: a terminal's Ctrl-C reaches eval, which stops it.
            .process_group(0)
            .kill_on_drop(true);
        let mut child = cmd.spawn().context("cannot start tautly")?;
        let mut group = Group(child.id().and_then(|p| i32::try_from(p).ok()));
        let (out, err) = (child.stdout.take(), child.stderr.take());
        let mut finish = None;
        let mut tail = VecDeque::new();
        let ended = {
            let read = async {
                tokio::join!(events(out, r, &mut finish), keep_tail(err, &mut tail));
                child.wait().await
            };
            tokio::select! {
                _ = read => Ended::Exited,
                () = tokio::time::sleep(Duration::from_secs(task.timeout_s)) => Ended::Timeout,
                () = self.cancel.cancelled() => Ended::Cancelled,
            }
        };
        if !matches!(ended, Ended::Exited) {
            // A run shuts its session down on SIGTERM (its tools' groups with it).
            group.signal(libc::SIGTERM);
            if tokio::time::timeout(KILL_GRACE, child.wait())
                .await
                .is_err()
            {
                group.signal(libc::SIGKILL);
                let _ = child.wait().await;
            }
        }
        group.0 = None;
        let stderr = Vec::from(tail).join("\n");
        Ok((ended, finish, stderr))
    }
}

/// A run's process group, killed if it is dropped before the run was reaped (an error or
/// a panic on the way).
struct Group(Option<i32>);

impl Group {
    fn signal(&self, sig: i32) {
        if let Some(pgid) = self.0 {
            // SAFETY: killpg only sends a signal; the group is the run's own, not reaped yet.
            unsafe {
                libc::killpg(pgid, sig);
            }
        }
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        self.signal(libc::SIGKILL);
    }
}

/// What task scripts get in their environment so no toolchain reaches the network.
const OFFLINE_ENV: &str = "GOPROXY=off GOTOOLCHAIN=local GOFLAGS=-mod=readonly \
                           CARGO_NET_OFFLINE=true npm_config_offline=true \
                           npm_config_update_notifier=false";

/// `bash '<path>'` quoted for the shell, offline, with `EVAL_BASE=<commit>` when given.
fn bash(path: &Path, base: Option<&str>) -> String {
    let base = base
        .filter(|c| !c.is_empty() && c.chars().all(|ch| ch.is_ascii_hexdigit()))
        .map(|c| format!("EVAL_BASE={c} "))
        .unwrap_or_default();
    let path = path.to_string_lossy().replace('\'', r"'\''");
    format!("{OFFLINE_ENV} {base}bash '{path}'")
}

/// The commit HEAD of the repository at `work` names, by the checkpoints' hardened git.
async fn head_commit(work: &Path) -> Option<String> {
    let sb = Arc::new(LocalSandbox::new(work.to_path_buf()).ok()?);
    let cp = Checkpointer::new(
        sb,
        work.to_path_buf(),
        "eval".into(),
        CheckpointConfig::default(),
    )?;
    cp.head_commit().await
}

/// A run's code index, lock, memory episodes and (without `--keep-logs`) session, all
/// keyed by its temporary root; removed on drop. The cost ledger's rows stay: they are
/// real spending.
struct Artifacts {
    data_home: PathBuf,
    hash: String,
    id: String,
    keep_logs: bool,
}

impl Drop for Artifacts {
    fn drop(&mut self) {
        let plain = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric());
        if !plain(&self.hash) || !plain(&self.id) {
            return;
        }
        let h = &self.data_home;
        if let Ok(entries) = std::fs::read_dir(h.join("index")) {
            let prefix = format!("{}.", self.hash);
            for e in entries.flatten() {
                if e.file_name().to_string_lossy().starts_with(&prefix) {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
        let _ = std::fs::remove_file(h.join("locks").join(format!("{}.lock", self.hash)));
        let _ = std::fs::remove_dir_all(h.join("memory").join(&self.id));
        if !self.keep_logs {
            let _ = std::fs::remove_dir_all(h.join("sessions").join(&self.id));
        }
    }
}

/// Sum the JSON events of `out` into `r`; `finish` is the turn's finish reason once it
/// finished. Lines that are not JSON events are skipped.
async fn events(
    out: Option<impl AsyncRead + Unpin>,
    r: &mut TaskResult,
    finish: &mut Option<String>,
) {
    let Some(out) = out else { return };
    let mut reader = BufReader::new(out);
    let mut line = Vec::new();
    // Bytes, not lines of text: a line that is not UTF-8 must not stop the draining.
    while matches!(reader.read_until(b'\n', &mut line).await, Ok(n) if n > 0) {
        if let Ok(ev) = serde_json::from_slice::<Value>(&line) {
            see(r, finish, &ev);
        }
        line.clear();
    }
}

/// One event. Read leniently: a field an older or newer stream lacks counts as absent.
fn see(r: &mut TaskResult, finish: &mut Option<String>, ev: &Value) {
    let num = |k: &str| ev.get(k).and_then(Value::as_u64).unwrap_or(0);
    let flag = |k: &str| ev.get(k).and_then(Value::as_bool).unwrap_or(false);
    match ev.get("type").and_then(Value::as_str) {
        Some("usage") => {
            if ev.get("role").and_then(Value::as_str) == Some("main") {
                r.tokens_in += num("tokens_in");
                r.tokens_out += num("tokens_out");
                r.cache_read += num("cache_read");
            } else {
                r.side_tokens += num("tokens_in") + num("tokens_out");
            }
            match ev.get("cost_usd").and_then(Value::as_f64) {
                Some(c) => {
                    r.cost_usd += c;
                    if finish.is_some() {
                        r.episode_cost_usd += c;
                    }
                }
                None => r.cost_partial = true,
            }
            r.cost_estimated |= flag("estimated");
        }
        Some("tool_finished") => {
            if matches!(
                ev.get("tool").and_then(Value::as_str),
                Some("edit" | "write")
            ) {
                r.edit_calls += 1;
                r.edit_failures += u32::from(flag("is_error"));
            }
            r.blocked_attempts += u32::from(flag("blocked"));
            if let Some(stage) = ev.pointer("/meta/repair").and_then(Value::as_str) {
                *r.edit_repairs_by_stage
                    .entry(stage.to_string())
                    .or_default() += 1;
            }
        }
        Some("session_changed") if r.session_id.is_none() => {
            r.session_id = ev
                .get("session_id")
                .and_then(Value::as_str)
                .map(str::to_string);
        }
        Some("turn_finished") => {
            let reason = ev
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            *finish = Some(reason.to_string());
        }
        _ => {}
    }
}

/// Keep the last lines of `err` in `tail`.
async fn keep_tail(err: Option<impl AsyncRead + Unpin>, tail: &mut VecDeque<String>) {
    let Some(err) = err else { return };
    let mut reader = BufReader::new(err);
    let mut line = Vec::new();
    while matches!(reader.read_until(b'\n', &mut line).await, Ok(n) if n > 0) {
        if tail.len() == STDERR_LINES {
            tail.pop_front();
        }
        tail.push_back(String::from_utf8_lossy(&line).trim_end().to_string());
        line.clear();
    }
}

// ------------------------------------------------------------------- results

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct Results {
    model: Option<String>,
    edit_format: Option<String>,
    tasks: Vec<TaskResult>,
    summary: Summary,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct Summary {
    trials: u32,
    pass_rate: f64,
    total_cost_usd: f64,
    /// The part of `total_cost_usd` the session-end episodes spent.
    episode_cost_usd: f64,
    cost_estimated: bool,
    cost_partial: bool,
    edit_failure_rate: f64,
    tokens_per_solved_task: Option<f64>,
    cache_hit_ratio: Option<f64>,
    blocked_attempts: u32,
    edit_repairs_by_stage: BTreeMap<String, u32>,
    /// Each figure's mean and sample standard deviation across trials.
    mean: Figures,
    stdev: Figures,
}

/// The figures of a set of runs (none: not defined for it).
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct Figures {
    pass_rate: Option<f64>,
    total_cost_usd: Option<f64>,
    edit_failure_rate: Option<f64>,
    tokens_per_solved_task: Option<f64>,
    cache_hit_ratio: Option<f64>,
}

/// `tokens_per_solved_task = Σ(tokens_in + tokens_out + side_tokens) / solved`,
/// `cache_hit_ratio = Σcache_read / Σ(tokens_in + cache_read)`, edit failures per `edit`
/// and `write` result.
fn figures(runs: &[&TaskResult]) -> Figures {
    let solved = runs.iter().filter(|r| r.passed).count();
    let tokens: u64 = runs
        .iter()
        .map(|r| r.tokens_in + r.tokens_out + r.side_tokens)
        .sum();
    let cache: u64 = runs.iter().map(|r| r.cache_read).sum();
    let input: u64 = runs.iter().map(|r| r.tokens_in).sum();
    let edits: u32 = runs.iter().map(|r| r.edit_calls).sum();
    let failed: u32 = runs.iter().map(|r| r.edit_failures).sum();
    Figures {
        pass_rate: (!runs.is_empty()).then(|| solved as f64 / runs.len() as f64),
        total_cost_usd: Some(runs.iter().map(|r| r.cost_usd).sum()),
        edit_failure_rate: Some(if edits == 0 {
            0.0
        } else {
            f64::from(failed) / f64::from(edits)
        }),
        tokens_per_solved_task: (solved > 0).then(|| tokens as f64 / solved as f64),
        cache_hit_ratio: (input + cache > 0).then(|| cache as f64 / (input + cache) as f64),
    }
}

/// The mean and sample standard deviation of the defined values (0 for one value).
fn mean_stdev(values: &[Option<f64>]) -> (Option<f64>, Option<f64>) {
    let v: Vec<f64> = values.iter().flatten().copied().collect();
    if v.is_empty() {
        return (None, None);
    }
    let n = v.len() as f64;
    let mean = v.iter().sum::<f64>() / n;
    if v.len() == 1 {
        return (Some(mean), Some(0.0));
    }
    let var = v.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (n - 1.0);
    (Some(mean), Some(var.sqrt()))
}

fn summarize(runs: &[TaskResult], trials: u32) -> Summary {
    let all: Vec<&TaskResult> = runs.iter().collect();
    let pooled = figures(&all);
    let per_trial: Vec<Figures> = (1..=trials)
        .map(|n| figures(&runs.iter().filter(|r| r.trial == n).collect::<Vec<_>>()))
        .collect();
    let spread =
        |f: fn(&Figures) -> Option<f64>| mean_stdev(&per_trial.iter().map(f).collect::<Vec<_>>());
    let (mut mean, mut stdev) = (Figures::default(), Figures::default());
    (mean.pass_rate, stdev.pass_rate) = spread(|f| f.pass_rate);
    (mean.total_cost_usd, stdev.total_cost_usd) = spread(|f| f.total_cost_usd);
    (mean.edit_failure_rate, stdev.edit_failure_rate) = spread(|f| f.edit_failure_rate);
    (mean.tokens_per_solved_task, stdev.tokens_per_solved_task) =
        spread(|f| f.tokens_per_solved_task);
    (mean.cache_hit_ratio, stdev.cache_hit_ratio) = spread(|f| f.cache_hit_ratio);
    let mut repairs = BTreeMap::new();
    for r in runs {
        for (stage, n) in &r.edit_repairs_by_stage {
            *repairs.entry(stage.clone()).or_default() += n;
        }
    }
    Summary {
        trials,
        pass_rate: pooled.pass_rate.unwrap_or(0.0),
        total_cost_usd: pooled.total_cost_usd.unwrap_or(0.0),
        episode_cost_usd: runs.iter().map(|r| r.episode_cost_usd).sum(),
        cost_estimated: runs.iter().any(|r| r.cost_estimated),
        cost_partial: runs.iter().any(|r| r.cost_partial),
        edit_failure_rate: pooled.edit_failure_rate.unwrap_or(0.0),
        tokens_per_solved_task: pooled.tokens_per_solved_task,
        cache_hit_ratio: pooled.cache_hit_ratio,
        blocked_attempts: runs.iter().map(|r| r.blocked_attempts).sum(),
        edit_repairs_by_stage: repairs,
        mean,
        stdev,
    }
}

/// `amount` with the marks of `s`: `?` when it covers only the priced part, `(est.)`.
fn cost(s: &Summary, amount: f64) -> String {
    cost_cell(Some(amount), s.cost_partial, s.cost_estimated)
}

/// The run's summary, for the terminal.
fn summary_text(results: &Results, tasks: usize) -> String {
    let s = &results.summary;
    let runs = results.tasks.len();
    let solved = results.tasks.iter().filter(|r| r.passed).count();
    let edits: u32 = results.tasks.iter().map(|r| r.edit_calls).sum();
    let failed: u32 = results.tasks.iter().map(|r| r.edit_failures).sum();
    let mut out = format!(
        "eval: {runs} run{} of {tasks} task{} ({} trial{}), model {}{}\n",
        plural(runs),
        plural(tasks),
        s.trials,
        plural(s.trials as usize),
        printable(results.model.as_deref().unwrap_or("(configured)")),
        results
            .edit_format
            .as_deref()
            .map(|f| format!(", edit format {f}"))
            .unwrap_or_default(),
    );
    out.push_str(&format!(
        "passed: {solved} of {runs} ({})\n",
        pct(Some(s.pass_rate))
    ));
    out.push_str(&format!("cost: {}\n", cost(s, s.total_cost_usd)));
    if s.trials > 1 {
        let per = s.mean.total_cost_usd.unwrap_or(0.0);
        out.push_str(&format!("  per trial: {}\n", cost(s, per)));
    }
    out.push_str(&format!(
        "  of which session-end episodes: {}\n",
        usd(Some(s.episode_cost_usd))
    ));
    out.push_str(&format!(
        "tokens per solved task: {}\n",
        tokens(s.tokens_per_solved_task)
    ));
    out.push_str(&format!("cache hit ratio: {}\n", pct(s.cache_hit_ratio)));
    out.push_str(&format!(
        "edit failures: {failed} of {edits} edit and write calls ({}); blocked attempts: {}\n",
        pct(Some(s.edit_failure_rate)),
        s.blocked_attempts
    ));
    if !s.edit_repairs_by_stage.is_empty() {
        let stages: Vec<String> = s
            .edit_repairs_by_stage
            .iter()
            .map(|(stage, n)| format!("{} {n}", printable(stage)))
            .collect();
        out.push_str(&format!("edit repairs: {}\n", stages.join(", ")));
    }
    if s.trials > 1 {
        out.push_str(&format!(
            "across trials: pass rate {} ± {}, tokens per solved task {} ± {}\n",
            pct(s.mean.pass_rate),
            pct(s.stdev.pass_rate),
            tokens(s.mean.tokens_per_solved_task),
            tokens(s.stdev.tokens_per_solved_task),
        ));
    }
    out
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

// ------------------------------------------------------------------- replay gate

#[derive(Debug, Serialize, Deserialize)]
struct Baseline {
    tokens_per_solved_task: f64,
}

/// Compare the results' tokens per solved task with `<dir>/../baseline.json` (or rewrite
/// it); every replayed task must pass. The exit code.
fn regression_gate(dir: &Path, results: &Results, update: bool) -> anyhow::Result<i32> {
    let path = dir.parent().unwrap_or(dir).join("baseline.json");
    let failed: Vec<&str> = results
        .tasks
        .iter()
        .filter(|t| !t.passed)
        .map(|t| t.id.as_str())
        .collect();
    if !failed.is_empty() {
        eprintln!(
            "tautly: replayed tasks failed, so the figure is not comparable: {}",
            printable(&failed.join(", "))
        );
        return Ok(1);
    }
    let summary = &results.summary;
    let Some(now) = summary.tokens_per_solved_task else {
        eprintln!("tautly: no replayed task was solved, so there is no figure to compare");
        return Ok(1);
    };
    if update {
        let text = serde_json::to_string_pretty(&Baseline {
            tokens_per_solved_task: now,
        })? + "\n";
        std::fs::write(&path, text).with_context(|| format!("cannot write {}", path.display()))?;
        eprintln!(
            "baseline updated: {} tokens per solved task ({})",
            tokens(Some(now)),
            printable(&path.to_string_lossy())
        );
        return Ok(0);
    }
    let text = std::fs::read_to_string(&path).with_context(|| {
        format!(
            "cannot read the baseline {} (write one with --update-baseline)",
            path.display()
        )
    })?;
    let base: Baseline = serde_json::from_str(&text)
        .with_context(|| format!("{} is not a baseline", path.display()))?;
    let change = if base.tokens_per_solved_task > 0.0 {
        now / base.tokens_per_solved_task - 1.0
    } else {
        f64::INFINITY
    };
    let line = format!(
        "{} tokens per solved task against a baseline of {} ({:+.1}%, limit +{:.0}%)",
        tokens(Some(now)),
        tokens(Some(base.tokens_per_solved_task)),
        change * 100.0,
        REGRESSION_LIMIT * 100.0
    );
    // Exactly 10% over is within (with room for rounding: 1100 / 1000 - 1 > 0.1 in f64).
    if change > REGRESSION_LIMIT + 1e-9 {
        eprintln!("tautly: token regression: {line}");
        return Ok(1);
    }
    eprintln!("token use within the baseline: {line}");
    Ok(0)
}

// ------------------------------------------------------------------- report

/// `tautly eval report`: one row per results object.
fn report(files: &[PathBuf], markdown: bool) -> anyhow::Result<i32> {
    let mut all = Vec::new();
    for f in files {
        let text =
            std::fs::read_to_string(f).with_context(|| format!("cannot read {}", f.display()))?;
        let v: Value =
            serde_json::from_str(&text).with_context(|| format!("{} is not JSON", f.display()))?;
        let items = match v {
            Value::Array(items) => items,
            one => vec![one],
        };
        for item in items {
            let r: Results = serde_json::from_value(item).with_context(|| {
                format!("{} holds something that is not eval results", f.display())
            })?;
            all.push(r);
        }
    }
    let header = [
        "Model",
        "Pass rate",
        "Tokens per solved task",
        "Cache hit ratio",
        "Cost per trial",
    ];
    let rows: Vec<[String; 5]> = all.iter().map(row).collect();
    print!(
        "{}",
        if markdown {
            markdown_table(&header, &rows)
        } else {
            text_table(&header, &rows)
        }
    );
    Ok(0)
}

fn row(r: &Results) -> [String; 5] {
    let s = &r.summary;
    let mut model = printable(r.model.as_deref().unwrap_or("-"));
    if let Some(f) = &r.edit_format {
        model.push_str(&format!(" ({})", printable(f)));
    }
    let mut pass = pct(Some(s.pass_rate));
    if s.trials > 1 {
        pass.push_str(&format!(" ± {}", pct(s.stdev.pass_rate)));
    }
    [
        model,
        pass,
        tokens(s.tokens_per_solved_task),
        pct(s.cache_hit_ratio),
        // Per trial, so results with different trial counts compare.
        cost(s, s.mean.total_cost_usd.unwrap_or(s.total_cost_usd)),
    ]
}

fn markdown_table(header: &[&str; 5], rows: &[[String; 5]]) -> String {
    // `\` first, so a trailing backslash cannot escape the cell's closing `|`.
    let cell = |c: &str| c.replace('\\', "\\\\").replace('|', "\\|");
    let mut out = format!("| {} |\n|---|---:|---:|---:|---:|\n", header.join(" | "));
    for r in rows {
        let cells: Vec<String> = r.iter().map(|c| cell(c)).collect();
        out.push_str(&format!("| {} |\n", cells.join(" | ")));
    }
    out
}

fn text_table(header: &[&str; 5], rows: &[[String; 5]]) -> String {
    let widths: Vec<usize> = (0..5)
        .map(|i| {
            rows.iter()
                .map(|r| r[i].chars().count())
                .chain([header[i].len()])
                .max()
                .unwrap_or(0)
        })
        .collect();
    let line = |cells: Vec<&str>| {
        let mut l = String::new();
        for (i, c) in cells.iter().enumerate() {
            let pad = " ".repeat(widths[i] - c.chars().count());
            // The model left-aligned, the figures right-aligned.
            if i == 0 {
                l.push_str(&format!("{c}{pad}  "));
            } else {
                l.push_str(&format!("{pad}{c}  "));
            }
        }
        l.trim_end().to_string() + "\n"
    };
    let mut out = line(header.to_vec());
    for r in rows {
        out.push_str(&line(r.iter().map(String::as_str).collect()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mean_and_sample_stdev() {
        assert_eq!(mean_stdev(&[]), (None, None));
        assert_eq!(mean_stdev(&[None, Some(2.0)]), (Some(2.0), Some(0.0)));
        let (m, s) = mean_stdev(&[Some(1.0), Some(0.0), Some(1.0)]);
        assert!((m.unwrap() - 2.0 / 3.0).abs() < 1e-12);
        assert!((s.unwrap() - (1.0f64 / 3.0).sqrt()).abs() < 1e-12);
    }

    #[test]
    fn events_are_read_leniently() {
        let mut r = TaskResult::default();
        let mut finish = None;
        for ev in [
            serde_json::json!({"type": "usage", "role": "main", "tokens_in": 10, "cost_usd": null}),
            // An older stream: no `meta.repair`, no `blocked`.
            serde_json::json!({"type": "tool_finished", "tool": "write", "is_error": true, "meta": {}}),
            serde_json::json!({"type": "turn_finished", "reason": "completed"}),
            serde_json::json!({"type": "usage", "role": "side", "tokens_in": 3, "tokens_out": 1,
                               "cost_usd": 0.5, "estimated": true}),
        ] {
            see(&mut r, &mut finish, &ev);
        }
        assert_eq!((r.tokens_in, r.side_tokens), (10, 4));
        assert!(r.cost_partial && r.cost_estimated);
        assert_eq!((r.edit_calls, r.edit_failures), (1, 1));
        assert_eq!((r.cost_usd, r.episode_cost_usd), (0.5, 0.5));
        assert_eq!(finish.as_deref(), Some("completed"));
    }

    #[test]
    fn scripts_are_quoted_for_the_shell() {
        let cmd = bash(Path::new("/a b/it's.sh"), None);
        assert!(cmd.ends_with(r" bash '/a b/it'\''s.sh'"), "{cmd}");
        assert!(cmd.starts_with("GOPROXY=off "), "{cmd}");
        let cmd = bash(Path::new("/v.sh"), Some("abc123"));
        assert!(cmd.ends_with(" EVAL_BASE=abc123 bash '/v.sh'"), "{cmd}");
        // Only a commit id is passed on.
        assert!(!bash(Path::new("/v.sh"), Some("x; rm")).contains("EVAL_BASE"));
    }

    #[test]
    fn markdown_cells_escape_backslash_then_bar() {
        let row = [
            "a\\".to_string(),
            "b|c".into(),
            "".into(),
            "".into(),
            "".into(),
        ];
        let t = markdown_table(&["1", "2", "3", "4", "5"], &[row]);
        assert!(t.contains(r"| a\\ | b\|c |"), "{t}");
    }
}
