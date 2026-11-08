//! Session bisect (spec 7.12.7): the step at which a check started failing, found by
//! running it on the files of past step boundaries.
//!
//! - **The plan** ([`plan`]): the endpoints (good: 0, bad: the tip, unless given) and the
//!   step boundaries between them whose files are known exactly ([`state_at`] with no
//!   partial note and a checkpoint to start from). The others are never run: they are
//!   listed in [`BisectPlan::skipped`], and a result next to one says it may lie there.
//!   An endpoint that is not exact is refused ([`BisectError::Partial`]). The log is read
//!   once for the whole plan.
//! - **The pre-pass** ([`prepass`]): runs of the identical normalized command the agent
//!   logged narrow the default endpoints before anything runs. A logged run in step `s`
//!   saw the files of the boundary before `s` when the log's write generation did not move
//!   between them: no other call of step `s` wrote anything (no `Edit`, no command, no
//!   write of unknown outcome), so the only checkpoint between is the one taken before the
//!   run. Endpoints given explicitly are kept. An endpoint the pre-pass chose that does not
//!   verify falls back to the default one ([`BisectOutcome::notes`] says so).
//! - **The runs** ([`run`]): one temporary worktree
//!   `<data home>/worktrees/<project-id>/bisect-<ulid>`, made by [`worktree::create`] at
//!   the first state and moved to each next one (its state composed as a commit, then
//!   `git checkout --detach --force` plus `git clean -f -d`, by the checkpoints' hardened
//!   git rooted at the worktree: no hook, filter or fsmonitor runs). Ignored files (build
//!   caches) are kept between runs; whatever else a run left goes. Tautly itself writes
//!   nothing in the user's work tree, index or HEAD: only objects and the worktree's
//!   administrative files. The check is another matter: v1 has no OS isolation, so the
//!   check (and the scripts of past states it runs) can write anywhere its user can. The
//!   endpoints are verified first (good must pass, bad must fail:
//!   [`BisectError::EndpointMismatch`], with the last lines of the run's output), then the
//!   candidates are bisected; a skipped run (exit 125, a timeout, a check that could not
//!   run) is passed over for its nearest neighbour.
//! - **Cleanup**: the worktree is removed at the end, on success, error, cancel or panic
//!   (`git worktree remove --force`, then whatever is left of the directory and of its
//!   `.git/worktrees/<id>` entry). Each bisect holds an exclusive `flock` on
//!   `bisect-<ulid>.lock` beside its worktree for as long as it runs (taken before the
//!   worktree is made, removed only after it is gone, let go with an explicit `LOCK_UN`),
//!   and marks the worktree's administrative directory with a [`MARKER`] file. After a
//!   crash the next bisect of the project removes only what is provably a dead bisect's: a
//!   `bisect-<ULID>` name, an existing lock file nobody holds (opened without creating
//!   it), and the marker in the administrative entry that names the directory. A worktree
//!   of any other origin (the user's, a fork's) is never touched, whatever its name. Lock
//!   files and administrative entries left without their worktree go the same way.
//! - **The check** ([`BashRunner`]) runs through the `bash` tool's path: its own process
//!   group, killed on a timeout or cancel (and any background child it left once it
//!   returns), the shell-state variables and the provider key variables stripped. Whether
//!   it may run at all is the policy's ([`policy_decision`]): the caller refuses a deny
//!   rule, and nothing here approves anything.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::File;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::FutureExt as _;
use serde_json::{Value, json};
use tautly_provider::{EditFormat, ProviderEvent};
pub use tautly_sandbox::Decision;
use tautly_sandbox::{LocalSandbox, Policy, PolicyConfig, SPILL_DIR, Sandbox};
use tautly_session::new_session_id;
use tautly_tools::{BashTool, PersistedEffect, ProcessGroups, Tool, ToolCx};
pub use tokio_util::sync::CancellationToken;

use crate::checkpoint::{self, CheckpointError};
use crate::lease;
use crate::record::{RecordLog, SessionRecord};
use crate::timetravel::{Boundaries, FileState, StepCounter, TtError};
use crate::worktree::{self, WtError};

#[cfg(doc)]
use crate::timetravel::state_at;

/// Longest a check may run (the `bash` tool's own limit).
pub const MAX_TIMEOUT: Duration = Duration::from_secs(600);
/// The file a bisect puts in its worktree's administrative directory
/// (`.git/worktrees/<id>/`): what tells a dead bisect's worktree from anyone else's.
pub const MARKER: &str = "tautly-bisect";
/// Longest [`BisectOutcome::text_excerpt`], in characters.
const EXCERPT_CHARS: usize = 300;
/// The name of each bisect worktree (and of its lock file) starts with this.
const PREFIX: &str = "bisect-";
/// How many lines of a run's output an endpoint mismatch reports.
const OUTPUT_LINES: usize = 10;

/// Where an endpoint of a plan came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Endpoint {
    /// `--good` / `--bad`.
    Given,
    /// 0, or the session's last step.
    Default,
    /// The session's own logged runs of the command ([`prepass`]).
    Logged,
}

impl Endpoint {
    /// How a message names it.
    pub fn describe(self) -> &'static str {
        match self {
            Endpoint::Given => "as given",
            Endpoint::Default => "the default",
            Endpoint::Logged => "from the session's own logged runs of the command",
        }
    }
}

/// What a bisect runs ([`plan`]).
#[derive(Debug, Clone)]
pub struct BisectPlan {
    pub session: String,
    /// The check, as given.
    pub test: String,
    /// The boundary that must pass.
    pub good: u32,
    /// The boundary that must fail.
    pub bad: u32,
    pub good_from: Endpoint,
    pub bad_from: Endpoint,
    /// The exact step boundaries strictly between `good` and `bad`.
    pub candidates: Vec<u32>,
    /// The boundaries between `good` and `bad` whose files are not known exactly: never
    /// run.
    pub skipped: Vec<u32>,
    log: PathBuf,
    project: String,
    /// The endpoints before the pre-pass: where a logged one falls back to.
    default_good: u32,
    default_bad: u32,
    /// The files of every exact boundary from `default_good` to `default_bad`.
    exact: BTreeMap<u32, FileState>,
    /// The boundaries in that range that are not exact.
    partial: BTreeSet<u32>,
}

impl BisectPlan {
    /// The most runs it can take: every exact boundary it may judge (both endpoints, every
    /// candidate, and those a logged endpoint's fallback widens to), each at most once.
    pub fn max_runs(&self) -> u32 {
        let lo = if self.good_from == Endpoint::Logged {
            self.default_good
        } else {
            self.good
        };
        let hi = if self.bad_from == Endpoint::Logged {
            self.default_bad
        } else {
            self.bad
        };
        count(self.exact.range(lo..=hi).count())
    }

    /// The runs it takes when none is skipped and the endpoints hold: both endpoints, then
    /// a binary search.
    pub fn expected_runs(&self) -> u32 {
        let n = count(self.candidates.len());
        2 + (u32::BITS - n.leading_zeros())
    }

    /// The exact and the partial boundaries strictly between `lo` and `hi`.
    fn between(&self, lo: u32, hi: u32) -> (Vec<u32>, Vec<u32>) {
        let inside = |k: &u32| *k > lo && *k < hi;
        (
            self.exact.keys().copied().filter(inside).collect(),
            self.partial.iter().copied().filter(inside).collect(),
        )
    }
}

fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// What one run of the check says of a state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Exit 0.
    Good,
    /// Any other exit code (a signal counts as 128 + n).
    Bad,
    /// Exit 125, a timeout, or the check could not run to its end.
    Skip,
}

impl Verdict {
    /// The verdict of an exit code (none: killed by a timeout or cancel, or never ran).
    pub fn from_exit(code: Option<i32>) -> Verdict {
        match code {
            Some(0) => Verdict::Good,
            Some(125) | None => Verdict::Skip,
            Some(_) => Verdict::Bad,
        }
    }

    fn word(self) -> &'static str {
        match self {
            Verdict::Good => "passed",
            Verdict::Bad => "failed",
            Verdict::Skip => "was skipped (exit 125, a timeout, or it could not run)",
        }
    }
}

/// Runs the check in a directory.
#[async_trait]
pub trait TestRunner: Send + Sync {
    /// Run `cmd` in `dir` for at most `timeout`.
    async fn run(&self, dir: &Path, cmd: &str, timeout: Duration) -> Verdict;

    /// Whether the bisect was cancelled: it stops (and cleans up) after the run in
    /// progress.
    fn cancelled(&self) -> bool {
        false
    }

    /// The last lines of the last run's output (or why it could not run), for a report.
    fn last_output(&self) -> Option<String> {
        None
    }
}

/// What a bisect found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BisectOutcome {
    /// The first boundary that failed (after `last_good`, which passed): the step that
    /// broke the check, unless it lies among `uncertain`.
    pub first_bad: Option<u32>,
    /// That step's user turn.
    pub turn: Option<u32>,
    /// That step's tool calls, `tool: target` each.
    pub calls: Vec<String>,
    /// At most 300 characters of that step's assistant text.
    pub text_excerpt: String,
    /// How many times the check ran.
    pub runs: u32,
    /// The boundaries never judged: partial ones, and those whose run was skipped.
    pub skipped: Vec<u32>,
    /// The last boundary that passed.
    pub last_good: u32,
    /// The skipped boundaries between `last_good` and `first_bad`: the change may be at
    /// any of them instead.
    pub uncertain: Vec<u32>,
    /// What changed on the way: an endpoint from the logged runs that did not hold and was
    /// replaced by the default one.
    pub notes: Vec<String>,
}

/// Why a bisect could not run or finish.
#[derive(Debug, thiserror::Error)]
pub enum BisectError {
    #[error("{0}")]
    Tt(#[from] TtError),
    #[error("{0}")]
    Wt(WtError),
    #[error("the files at step {step} are not known exactly ({note}); choose another endpoint")]
    Partial { step: u32, note: String },
    #[error("the good step ({good}) must come before the bad step ({bad})")]
    BadRange { good: u32, bad: u32 },
    #[error(
        "step {step} ({} {}) was to {} but it {}: the endpoints do not hold, so nothing was \
         bisected; choose other steps with --good and --bad",
        if *expected == Verdict::Good { "good step," } else { "bad step," },
        from.describe(),
        if *expected == Verdict::Good { "pass" } else { "fail" },
        got.word()
    )]
    EndpointMismatch {
        step: u32,
        from: Endpoint,
        expected: Verdict,
        got: Verdict,
        /// The last lines of that run's output, or why it could not run.
        output: Option<String>,
    },
    #[error("cancelled")]
    Cancelled,
    #[error("{0}")]
    Other(String),
}

impl From<WtError> for BisectError {
    fn from(e: WtError) -> Self {
        match e {
            WtError::InsideRepository(dir) | WtError::DefaultDirInsideRepository(dir) => {
                BisectError::Other(format!(
                    "the bisect worktree {} would be inside a worktree of the repository \
                     (the data home is); move the data home with TAUTLY_HOME",
                    dir.display()
                ))
            }
            e => BisectError::Wt(e),
        }
    }
}

impl From<CheckpointError> for BisectError {
    fn from(e: CheckpointError) -> Self {
        BisectError::Wt(WtError::Git(e))
    }
}

impl From<tautly_session::Error> for BisectError {
    fn from(e: tautly_session::Error) -> Self {
        BisectError::Tt(TtError::Session(e))
    }
}

/// `cmd` as the pre-pass compares commands: runs of whitespace collapsed to one space,
/// none at the ends.
pub fn normalize(cmd: &str) -> String {
    cmd.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The plan for bisecting `reader`'s session with `test` between `good` (default 0, or
/// the pre-pass's) and `bad` (default the last step, or the pre-pass's). Reads the log
/// once. Blocking I/O.
pub fn plan(
    reader: &RecordLog,
    test: &str,
    good: Option<u32>,
    bad: Option<u32>,
) -> Result<BisectPlan, BisectError> {
    let walked = Boundaries::new(reader)?;
    let tip = walked.last_step();
    let (lo0, hi0) = (good.unwrap_or(0), bad.unwrap_or(tip));
    for at in [lo0, hi0] {
        if at > tip {
            return Err(TtError::NoSuchStep { at, last: tip }.into());
        }
    }
    if lo0 >= hi0 {
        return Err(BisectError::BadRange {
            good: lo0,
            bad: hi0,
        });
    }
    let (mut exact, mut partial) = (BTreeMap::new(), BTreeMap::new());
    for k in lo0..=hi0 {
        // Only an in-place materialize needs what tools wrote after the checkpoint.
        match walked.state(k, false) {
            Ok(s) if s.base.is_some() && s.partial.is_none() => {
                exact.insert(k, s);
            }
            Ok(s) => {
                let note = s
                    .partial
                    .unwrap_or_else(|| "no checkpoint follows that step".to_string());
                partial.insert(k, note);
            }
            // A number no record of the log carries.
            Err(TtError::NoSuchStep { .. }) if k != lo0 && k != hi0 => {}
            Err(e) => return Err(e.into()),
        }
    }
    let given = |g: Option<u32>| {
        if g.is_some() {
            Endpoint::Given
        } else {
            Endpoint::Default
        }
    };
    let (mut lo, mut hi) = (lo0, hi0);
    let (mut good_from, mut bad_from) = (given(good), given(bad));
    if let Some((g, b)) = prepass_in(reader, walked.records(), test) {
        if good.is_none() && g > lo && g < hi && exact.contains_key(&g) {
            lo = g;
            good_from = Endpoint::Logged;
        }
        if bad.is_none() && b < hi && b > lo && exact.contains_key(&b) {
            hi = b;
            bad_from = Endpoint::Logged;
        }
    }
    for at in [lo, hi] {
        if let Some(note) = partial.get(&at) {
            return Err(BisectError::Partial {
                step: at,
                note: note.clone(),
            });
        }
    }
    let header = reader.header();
    let mut plan = BisectPlan {
        session: header.session_id.clone(),
        test: test.to_string(),
        good: lo,
        bad: hi,
        good_from,
        bad_from,
        candidates: Vec::new(),
        skipped: Vec::new(),
        log: reader.path().to_path_buf(),
        project: header.project.clone(),
        default_good: lo0,
        default_bad: hi0,
        exact,
        partial: partial.into_keys().collect(),
    };
    (plan.candidates, plan.skipped) = plan.between(lo, hi);
    Ok(plan)
}

/// What one step of the log did, for the pre-pass.
#[derive(Default)]
struct StepRuns {
    /// The step before it: the boundary its runs saw.
    before: u32,
    /// The exit codes of its runs of the command (none: killed).
    exits: Vec<Option<i32>>,
    /// Another call of the step wrote (or may have written) files.
    wrote: bool,
}

/// From the runs of `test` (normalized) the agent logged, with their exit codes: the last
/// boundary a run passed at that a later failing run follows, and the first boundary after
/// it a run failed at. A run counts for the boundary before its step only when the log's
/// write generation did not move between them (see the module docs); a boundary with runs
/// that disagree counts for neither. Blocking I/O; a record that cannot be read ends the
/// log there.
pub fn prepass(reader: &RecordLog, test: &str) -> Option<(u32, u32)> {
    let mut recs = Vec::new();
    for rec in reader.records() {
        let Ok(rec) = rec else {
            break;
        };
        recs.push(rec);
    }
    prepass_in(reader, &recs, test)
}

fn prepass_in(reader: &RecordLog, recs: &[(u64, SessionRecord)], test: &str) -> Option<(u32, u32)> {
    let test = normalize(test);
    let mut steps = StepCounter::default();
    let mut by_step: BTreeMap<u32, StepRuns> = BTreeMap::new();
    // Each call's step, and whether it is a run of the command.
    let mut calls: HashMap<&str, (u32, bool)> = HashMap::new();
    for (_, rec) in recs {
        let before = steps.last();
        let owner = steps.see(rec);
        match rec {
            SessionRecord::Request { step, .. } => {
                by_step.entry(*step).or_insert_with(|| StepRuns {
                    before,
                    ..StepRuns::default()
                });
            }
            SessionRecord::ToolCall {
                id,
                name,
                args,
                args_blob,
            } => {
                if let Some((s, _)) = owner {
                    // A command over 4 KiB is in a blob (cp5 FMT re-review N4).
                    let args = crate::record::tool_args(reader, args, args_blob.as_ref());
                    let matches = name == "bash"
                        && args
                            .get("command")
                            .and_then(Value::as_str)
                            .is_some_and(|c| normalize(c) == test);
                    calls.insert(id, (s, matches));
                }
            }
            SessionRecord::ToolResult {
                id,
                effects,
                meta,
                ran,
                ..
            } => {
                let Some(&(s, matches)) = calls.get(id.as_str()) else {
                    continue;
                };
                let Some(runs) = by_step.get_mut(&s) else {
                    continue;
                };
                if matches {
                    if *ran {
                        runs.exits.push(meta.exit_code);
                    }
                } else if effects.iter().any(writes) {
                    runs.wrote = true;
                }
            }
            SessionRecord::Edit { .. } => {
                if let Some(runs) = owner.and_then(|(s, _)| by_step.get_mut(&s)) {
                    runs.wrote = true;
                }
            }
            SessionRecord::WritesLost { .. } => {
                if let Some(runs) = by_step.get_mut(&steps.last()) {
                    runs.wrote = true;
                }
            }
            _ => {}
        }
    }
    let mut seen: BTreeMap<u32, BTreeSet<bool>> = BTreeMap::new();
    for runs in by_step.values().filter(|r| !r.wrote) {
        for exit in &runs.exits {
            match Verdict::from_exit(*exit) {
                Verdict::Good => seen.entry(runs.before).or_default().insert(true),
                Verdict::Bad => seen.entry(runs.before).or_default().insert(false),
                Verdict::Skip => false,
            };
        }
    }
    let verdicts: Vec<(u32, bool)> = seen
        .into_iter()
        .filter(|(_, v)| v.len() == 1)
        .filter_map(|(k, v)| v.first().map(|good| (k, *good)))
        .collect();
    let good = verdicts
        .iter()
        .filter(|(k, good)| *good && verdicts.iter().any(|(b, g)| !g && b > k))
        .map(|(k, _)| *k)
        .max()?;
    let bad = verdicts
        .iter()
        .filter(|(k, g)| !g && *k > good)
        .map(|(k, _)| *k)
        .min()?;
    Some((good, bad))
}

/// Whether an effect wrote (or may have written) files.
fn writes(e: &PersistedEffect) -> bool {
    matches!(
        e,
        PersistedEffect::Edited { .. }
            | PersistedEffect::Deleted { .. }
            | PersistedEffect::Exec { may_write: true }
            | PersistedEffect::Unknown
    )
}

/// What the policy `permissions` (at the work tree `root`, with no session rule: logged
/// approvals are never consent) says of running `cmd` as the `bash` tool would. The caller
/// refuses a `Deny`.
pub fn policy_decision(
    permissions: &PolicyConfig,
    root: &Path,
    cmd: &str,
) -> Result<Decision, BisectError> {
    let policy = Policy::new(permissions.clone(), root.to_path_buf())
        .map_err(|e| BisectError::Other(e.to_string()))?;
    let effect = BashTool
        .effect(&json!({ "command": cmd }))
        .map_err(|e| BisectError::Other(e.to_string()))?;
    Ok(policy.decide("bash", &effect, Some(cmd)))
}

/// Bisect `plan` in a temporary worktree of the repository the sandbox `sb` is rooted at,
/// with `runner` and `timeout` per run (see the module docs). The worktree is removed
/// before this returns, whatever happened.
pub async fn run(
    plan: &BisectPlan,
    sb: &dyn Sandbox,
    runner: &dyn TestRunner,
    timeout: Duration,
) -> Result<BisectOutcome, BisectError> {
    let repo_root = sb.root().to_path_buf();
    let base = data_home_of(&plan.log)?
        .join("worktrees")
        .join(&plan.project);
    sweep(&repo_root, &base).await;
    let name = format!("{PREFIX}{}", new_session_id());
    let lock = Held::take(&base.join(format!("{name}.lock")))
        .map_err(|e| BisectError::Other(format!("cannot lock {}: {e}", base.display())))?;
    let dir = base.join(&name);
    let outcome = AssertUnwindSafe(bisect_in(plan, &repo_root, runner, timeout, &dir))
        .catch_unwind()
        .await;
    remove_worktree(&repo_root, &dir).await;
    drop(lock);
    match outcome {
        Ok(outcome) => outcome,
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

/// `<data home>` of the session log at `log` (`<data home>/sessions/<project>/<id>.jsonl`).
fn data_home_of(log: &Path) -> Result<PathBuf, BisectError> {
    log.parent()
        .and_then(Path::parent)
        .filter(|sessions| sessions.file_name().is_some_and(|n| n == "sessions"))
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .ok_or_else(|| {
            BisectError::Other(format!(
                "{} is not in a data home's sessions directory",
                log.display()
            ))
        })
}

/// The worktree a bisect moves from state to state, the verdicts so far, and how many
/// runs it made.
struct Bench<'a> {
    plan: &'a BisectPlan,
    repo_root: &'a Path,
    runner: &'a dyn TestRunner,
    timeout: Duration,
    sb: Arc<dyn Sandbox>,
    cp: checkpoint::Checkpointer,
    /// Where it goes, then (once made) where it is.
    dir: PathBuf,
    made: bool,
    runs: u32,
    /// Each boundary judged, with its verdict and the last lines of its output.
    known: BTreeMap<u32, (Verdict, Option<String>)>,
}

impl Bench<'_> {
    /// The verdict of the check on the files of boundary `step` (run once per boundary).
    async fn judge(&mut self, step: u32) -> Result<Verdict, BisectError> {
        if let Some((v, _)) = self.known.get(&step) {
            return Ok(*v);
        }
        let state = self
            .plan
            .exact
            .get(&step)
            .ok_or_else(|| BisectError::Other(format!("no exact files for step {step}")))?;
        if self.made {
            let commit = worktree::compose_state(&self.cp, state, &self.dir).await?;
            checkpoint::worktree_move(&self.dir, &commit.commit).await?;
        } else {
            self.dir =
                worktree::create(Arc::clone(&self.sb), self.repo_root, state, &self.dir).await?;
            self.made = true;
            mark(&self.dir);
        }
        let verdict = self
            .runner
            .run(&self.dir, &self.plan.test, self.timeout)
            .await;
        self.runs += 1;
        self.known
            .insert(step, (verdict, self.runner.last_output()));
        if self.runner.cancelled() {
            return Err(BisectError::Cancelled);
        }
        Ok(verdict)
    }

    /// The endpoint `step` (from `from`) verified to be `expected`; one the pre-pass chose
    /// that is not falls back to `fallback`, and `notes` says so.
    async fn endpoint(
        &mut self,
        step: u32,
        from: Endpoint,
        expected: Verdict,
        fallback: u32,
        notes: &mut Vec<String>,
    ) -> Result<u32, BisectError> {
        let got = self.judge(step).await?;
        if got == expected {
            return Ok(step);
        }
        if from != Endpoint::Logged || fallback == step {
            return Err(self.mismatch(step, from, expected, got));
        }
        let which = if expected == Verdict::Good {
            "good"
        } else {
            "bad"
        };
        notes.push(format!(
            "the {which} step {step}, from the session's own logged runs of the command, {} \
             here; the default {which} step {fallback} was used instead",
            got.word()
        ));
        let got = self.judge(fallback).await?;
        if got == expected {
            return Ok(fallback);
        }
        Err(self.mismatch(fallback, Endpoint::Default, expected, got))
    }

    fn mismatch(&self, step: u32, from: Endpoint, expected: Verdict, got: Verdict) -> BisectError {
        BisectError::EndpointMismatch {
            step,
            from,
            expected,
            got,
            output: self.known.get(&step).and_then(|(_, o)| o.clone()),
        }
    }
}

async fn bisect_in(
    plan: &BisectPlan,
    repo_root: &Path,
    runner: &dyn TestRunner,
    timeout: Duration,
    dir: &Path,
) -> Result<BisectOutcome, BisectError> {
    let sb: Arc<dyn Sandbox> = Arc::new(
        LocalSandbox::new(repo_root.to_path_buf())
            .map_err(|e| BisectError::Other(format!("{}: {e}", repo_root.display())))?,
    );
    let cp = worktree::checkpointer(&sb, repo_root, "bisect")?;
    let mut bench = Bench {
        plan,
        repo_root,
        runner,
        timeout,
        sb,
        cp,
        dir: dir.to_path_buf(),
        made: false,
        runs: 0,
        known: BTreeMap::new(),
    };
    let mut notes = Vec::new();
    let good = bench
        .endpoint(
            plan.good,
            plan.good_from,
            Verdict::Good,
            plan.default_good,
            &mut notes,
        )
        .await?;
    let bad = bench
        .endpoint(
            plan.bad,
            plan.bad_from,
            Verdict::Bad,
            plan.default_bad,
            &mut notes,
        )
        .await?;
    let (candidates, partial) = plan.between(good, bad);
    let list: Vec<u32> = std::iter::once(good)
        .chain(candidates)
        .chain(std::iter::once(bad))
        .collect();
    let (mut lo, mut hi) = (0, list.len() - 1);
    let mut skipped = BTreeSet::new();
    loop {
        let mid = (lo + hi) / 2;
        let Some(pick) = (lo + 1..hi)
            .filter(|i| !skipped.contains(i))
            .min_by_key(|i| i.abs_diff(mid))
        else {
            break;
        };
        match bench.judge(list[pick]).await? {
            Verdict::Good => lo = pick,
            Verdict::Bad => hi = pick,
            Verdict::Skip => {
                skipped.insert(pick);
            }
        }
    }
    let (last_good, first_bad) = (list[lo], list[hi]);
    let all_skipped: BTreeSet<u32> = skipped.iter().map(|&i| list[i]).chain(partial).collect();
    let log = plan.log.clone();
    let (turn, calls, text) = tokio::task::spawn_blocking(move || step_info(&log, first_bad))
        .await
        .map_err(|e| BisectError::Other(e.to_string()))??;
    Ok(BisectOutcome {
        first_bad: Some(first_bad),
        turn,
        calls,
        text_excerpt: excerpt(&text),
        runs: bench.runs,
        uncertain: all_skipped
            .iter()
            .copied()
            .filter(|s| *s > last_good && *s < first_bad)
            .collect(),
        skipped: all_skipped.into_iter().collect(),
        last_good,
        notes,
    })
}

/// Step `step` of the log at `log`: its user turn, its calls (`tool: target`) and its
/// assistant text.
fn step_info(log: &Path, step: u32) -> Result<(Option<u32>, Vec<String>, String), BisectError> {
    let reader = RecordLog::open(log)?;
    let mut steps = StepCounter::default();
    let (mut turn, mut calls, mut text) = (None, Vec::new(), String::new());
    for rec in reader.records() {
        let (_, rec) = rec?;
        let Some((s, t)) = steps.see(&rec) else {
            continue;
        };
        if s != step {
            continue;
        }
        turn = Some(t);
        match rec {
            SessionRecord::ToolCall {
                name,
                args,
                args_blob,
                ..
            } => {
                let args = crate::record::tool_args(&reader, &args, args_blob.as_ref());
                calls.push(format!("{name}: {}", target(&name, &args)));
            }
            SessionRecord::Provider(ProviderEvent::TextDelta(t)) => text.push_str(&t),
            _ => {}
        }
    }
    Ok((turn, calls, text))
}

/// A call's target: a command's text (whitespace collapsed), else the path, pattern,
/// query, name or URL it names.
fn target(tool: &str, args: &Value) -> String {
    let arg = |k: &str| args.get(k).and_then(Value::as_str);
    match tool {
        "bash" => arg("command").map(normalize),
        _ => ["path", "pattern", "query", "name", "url"]
            .iter()
            .find_map(|k| arg(k))
            .map(str::to_string),
    }
    .unwrap_or_default()
}

/// `text` trimmed, cut to [`EXCERPT_CHARS`] characters (the last `…` when cut).
fn excerpt(text: &str) -> String {
    let text = text.trim();
    if text.chars().count() <= EXCERPT_CHARS {
        return text.to_string();
    }
    let mut cut: String = text.chars().take(EXCERPT_CHARS - 1).collect();
    cut.push('…');
    cut
}

/// The last [`OUTPUT_LINES`] lines of `text`.
fn tail(text: &str) -> String {
    let lines: Vec<&str> = text.trim_end().lines().collect();
    lines[lines.len().saturating_sub(OUTPUT_LINES)..].join("\n")
}

// ---------------------------------------------------------- locks, marks and the sweep

/// An exclusive `flock` on a bisect's lock file, held while it runs. Dropped: the file is
/// removed (while still held, so no other bisect locks a file about to go), then let go
/// with an explicit `LOCK_UN` (a child forked meanwhile may hold a copy of the descriptor).
struct Held {
    file: File,
    path: PathBuf,
    /// Remove the file when let go.
    remove: bool,
}

impl Held {
    /// Lock `path` (created owner-only, with its directory) without waiting.
    fn take(path: &Path) -> std::io::Result<Held> {
        let file = lease::open(path)?;
        lease::flock(&file, libc::LOCK_EX | libc::LOCK_NB)?;
        Ok(Held {
            file,
            path: path.to_path_buf(),
            remove: true,
        })
    }

    /// Lock the lock file at `path` without waiting, only when it exists (it is opened
    /// without creating it, and not through a symbolic link): none when it does not, or
    /// a live bisect holds it.
    fn existing(path: &Path) -> Option<Held> {
        use std::os::unix::fs::OpenOptionsExt;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .ok()?;
        lease::flock(&file, libc::LOCK_EX | libc::LOCK_NB).ok()?;
        Some(Held {
            file,
            path: path.to_path_buf(),
            remove: true,
        })
    }

    /// Let go without removing the file.
    fn keep(mut self) {
        self.remove = false;
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        if self.remove {
            let _ = std::fs::remove_file(&self.path);
        }
        let _ = lease::flock(&self.file, libc::LOCK_UN);
    }
}

/// Whether `name` is a bisect's: `bisect-` and a ULID (26 Crockford base-32 characters).
fn is_bisect_name(name: &str) -> bool {
    name.strip_prefix(PREFIX).is_some_and(|id| {
        id.len() == 26
            && id.bytes().all(|b| {
                matches!(b, b'0'..=b'9' | b'A'..=b'H' | b'J' | b'K' | b'M' | b'N' | b'P'..=b'T' | b'V'..=b'Z')
            })
    })
}

/// Put the [`MARKER`] in the administrative directory of the worktree at `dir` (best
/// effort: without it, a crash leaves the worktree for the user rather than the sweep).
fn mark(dir: &Path) {
    if let Some(dirs) = tautly_session::git_dirs_at(&dir.join(".git")) {
        let _ = std::fs::write(dirs.git_dir.join(MARKER), b"");
    }
}

/// The administrative entries of the repository at `repo_root` that are a bisect's: each
/// holds the [`MARKER`] (a regular file) and its `gitdir` names `<base>/bisect-<ULID>/.git`.
/// By name, with the entry's directory.
fn marked_entries(repo_root: &Path, base: &Path) -> BTreeMap<String, PathBuf> {
    let mut found = BTreeMap::new();
    let Some(dirs) = tautly_session::git_dirs_at(&repo_root.join(".git")) else {
        return found;
    };
    let Ok(entries) = std::fs::read_dir(dirs.common_dir.join("worktrees")) else {
        return found;
    };
    let base = worktree::resolved(base);
    for e in entries.flatten() {
        let admin = e.path();
        if !std::fs::symlink_metadata(admin.join(MARKER)).is_ok_and(|m| m.is_file()) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(admin.join("gitdir")) else {
            continue;
        };
        let Some(tree) = Path::new(text.trim()).parent() else {
            continue;
        };
        let (Some(name), Some(parent)) = (tree.file_name(), tree.parent()) else {
            continue;
        };
        let name = name.to_string_lossy().into_owned();
        if is_bisect_name(&name) && worktree::resolved(parent) == base {
            found.insert(name, admin);
        }
    }
    found
}

/// Remove what dead bisects left under `base` (see the module docs): for each
/// `bisect-<ULID>` whose lock file exists and nobody holds, its worktree and
/// administrative entry when that entry holds the marker, else (no directory there) the
/// lock file alone. A directory of that name without a marked entry is left as it is,
/// with its lock.
async fn sweep(repo_root: &Path, base: &Path) {
    let marked = marked_entries(repo_root, base);
    let mut names: BTreeSet<String> = marked.keys().cloned().collect();
    if let Ok(entries) = std::fs::read_dir(base) {
        for e in entries.flatten() {
            let file = e.file_name().to_string_lossy().into_owned();
            if let Some(name) = file.strip_suffix(".lock")
                && is_bisect_name(name)
            {
                names.insert(name.to_string());
            }
        }
    }
    for name in names {
        let Some(held) = Held::existing(&base.join(format!("{name}.lock"))) else {
            continue;
        };
        let dir = base.join(&name);
        if let Some(admin) = marked.get(&name) {
            remove_worktree(repo_root, &dir).await;
            let _ = std::fs::remove_dir_all(admin);
        } else if std::fs::symlink_metadata(&dir).is_ok() {
            held.keep();
            continue;
        }
        drop(held);
    }
}

/// Remove the worktree at `dir` of the repository at `repo_root`: `git worktree remove
/// --force` (hardened), then whatever is left of the directory and of its administrative
/// entry (`.git/worktrees/<id>` naming it). Best effort; only for a bisect's own worktree.
async fn remove_worktree(repo_root: &Path, dir: &Path) {
    if let Ok(sb) = LocalSandbox::new(repo_root.to_path_buf())
        && std::fs::symlink_metadata(dir).is_ok()
    {
        let _ = checkpoint::worktree_remove(&sb, repo_root, dir).await;
    }
    match std::fs::symlink_metadata(dir) {
        Ok(m) if m.is_dir() => {
            let _ = std::fs::remove_dir_all(dir);
        }
        Ok(_) => {
            let _ = std::fs::remove_file(dir);
        }
        Err(_) => {}
    }
    let Some(dirs) = tautly_session::git_dirs_at(&repo_root.join(".git")) else {
        return;
    };
    let ours = worktree::resolved(&dir.join(".git"));
    let Ok(entries) = std::fs::read_dir(dirs.common_dir.join("worktrees")) else {
        return;
    };
    for e in entries.flatten() {
        if let Ok(text) = std::fs::read_to_string(e.path().join("gitdir"))
            && worktree::resolved(Path::new(text.trim())) == ours
        {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

// ------------------------------------------------------------------------- the runner

/// The check as the `bash` tool runs a command (spec 7.12.7): its own process group,
/// killed on a timeout or cancel; the shell-state and the given (provider key) variables
/// stripped from its environment; output kept off the session (its last lines are kept
/// for [`TestRunner::last_output`]).
pub struct BashRunner {
    env_remove: Vec<String>,
    cancel: CancellationToken,
    groups: ProcessGroups,
    last: Mutex<Option<String>>,
}

impl BashRunner {
    /// `env_remove`: the provider key variables (`mcp::provider_key_vars`); `cancel`
    /// kills a run in progress and stops the bisect.
    pub fn new(env_remove: Vec<String>, cancel: CancellationToken) -> BashRunner {
        BashRunner {
            env_remove,
            cancel,
            groups: ProcessGroups::default(),
            last: Mutex::new(None),
        }
    }

    fn keep(&self, text: String) {
        *self.last.lock().unwrap_or_else(|e| e.into_inner()) = Some(text);
    }
}

#[async_trait]
impl TestRunner for BashRunner {
    async fn run(&self, dir: &Path, cmd: &str, timeout: Duration) -> Verdict {
        let sb = match LocalSandbox::new(dir.to_path_buf()) {
            Ok(sb) => sb,
            Err(e) => {
                self.keep(format!("could not run: {e}"));
                return Verdict::Skip;
            }
        };
        let secs = timeout
            .min(MAX_TIMEOUT)
            .as_secs()
            .saturating_add(u64::from(timeout.subsec_nanos() > 0))
            .clamp(1, MAX_TIMEOUT.as_secs());
        let cx = ToolCx::new(EditFormat::StrReplace, dir.join(SPILL_DIR))
            .with_cancel(self.cancel.clone())
            .with_env_remove(self.env_remove.clone())
            .with_process_groups(self.groups.clone());
        let args = json!({ "command": cmd, "timeout_s": secs, "raw": true });
        let result = BashTool.run(args, &sb, &cx).await;
        // A background child the check left must not run on into the next state.
        self.groups.kill_all_now();
        self.keep(tail(&result.output));
        Verdict::from_exit(result.meta.exit_code)
    }

    fn cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    fn last_output(&self) -> Option<String> {
        self.last.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_bisect_ulid_names_count() {
        assert!(is_bisect_name("bisect-01JBXZ3QK8R9T0V5W6X7Y8Z9AB"));
        assert!(!is_bisect_name("bisect-mine"));
        assert!(!is_bisect_name("bisect-fork"));
        // Crockford base 32 has no I, L, O or U, and a ULID is upper case here.
        assert!(!is_bisect_name("bisect-01JBXZ3QK8R9T0V5W6X7Y8Z9AI"));
        assert!(!is_bisect_name("bisect-01jbxz3qk8r9t0v5w6x7y8z9ab"));
        assert!(!is_bisect_name("bisect-01JBXZ3QK8R9T0V5W6X7Y8Z9A"));
        assert!(!is_bisect_name("x-01JBXZ3QK8R9T0V5W6X7Y8Z9AB"));
        assert!(is_bisect_name(&format!("{PREFIX}{}", new_session_id())));
    }

    #[test]
    fn tail_keeps_the_last_lines() {
        let text: String = (1..=15).map(|i| format!("line {i}\n")).collect();
        let t = tail(&text);
        assert!(t.starts_with("line 6\n"), "{t}");
        assert!(t.ends_with("line 15"), "{t}");
        assert_eq!(tail("one\n"), "one");
    }
}
