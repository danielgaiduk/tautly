//! `tautly bisect <session> --test "<cmd>" [--good N] [--bad M] [--timeout 600] [--yes]
//! [--json]` (spec 7.12.7): the step at which a check started failing. The check runs on
//! the files of past step boundaries in one temporary git worktree (removed at the end).
//! Tautly writes nothing in the work tree of the current directory; the check itself is
//! not confined (v1 has no OS isolation) and runs the files of past states.
//!
//! - Before anything runs it prints the command, the range (and where each endpoint came
//!   from) and the most runs it can take, and asks y/N on a terminal; elsewhere it needs
//!   `--yes`. A deny rule of the policy that matches the command refuses it (exit 1).
//!   Nothing logged in a session counts as approval: the policy is the current
//!   configuration's, with no session rules.
//! - Exit 0 is good, 125 skip, any other code bad; a run past `--timeout` seconds (at most
//!   600) is killed with its process group and skipped. Ctrl-C, SIGTERM or SIGHUP kills
//!   the run in progress, removes the worktree and stops, exiting 128 + the signal (130,
//!   143, 129).
//! - The endpoints are verified first: a good step that does not pass or a bad step that
//!   does not fail exits 2, with the last lines of that run's output. An endpoint the
//!   session's own logged runs chose falls back to the default one first.
//! - It prints the first failing step, its user turn, its tool calls and up to 300
//!   characters of its assistant text (with `--json`, one object with the same on stdout;
//!   everything else then goes to stderr).

use std::io::{IsTerminal as _, Write as _};
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::Duration;

use anyhow::Context as _;
use clap::Args as ClapArgs;
use serde_json::json;
use tautly_core::bisect::{
    self, BashRunner, BisectError, BisectOutcome, BisectPlan, CancellationToken, Decision, Endpoint,
};
use tautly_core::{
    Config, SessionReader, SessionRecord, printable, printable_multiline, provider_key_vars,
};
use tokio::signal::unix::{Signal, SignalKind, signal};

use super::Env;
use crate::Cli;

#[derive(Debug, ClapArgs)]
pub struct Args {
    /// The session id, or a prefix of it.
    session: String,
    /// The check: a shell command, run at the root of each past state (exit 0 good, 125
    /// skip, anything else bad).
    #[arg(long, value_name = "CMD")]
    test: String,
    /// A step whose files pass (default 0, before the first step; or what the session's
    /// own runs of the command show).
    #[arg(long, value_name = "N")]
    good: Option<u32>,
    /// A step whose files fail (default the last step; or what the session's own runs of
    /// the command show).
    #[arg(long, value_name = "M")]
    bad: Option<u32>,
    /// Seconds each run may take before it is killed and skipped (1 to 600).
    #[arg(long, value_name = "S", default_value_t = 600,
          value_parser = clap::value_parser!(u64).range(1..=600))]
    timeout: u64,
    /// Run without asking.
    #[arg(long)]
    yes: bool,
    /// Print the result as one JSON object.
    #[arg(long)]
    json: bool,
}

/// Where what is not the result goes: stderr with `--json` (stdout is the result only),
/// else stdout.
struct Say {
    json: bool,
}

impl Say {
    fn out(&self, text: &str) -> std::io::Result<()> {
        if self.json {
            let mut err = std::io::stderr();
            err.write_all(text.as_bytes())?;
            err.flush()
        } else {
            let mut out = std::io::stdout();
            out.write_all(text.as_bytes())?;
            out.flush()
        }
    }
}

pub async fn run(args: Args, env: &Env, cli: &Cli) -> anyhow::Result<i32> {
    let say = Say { json: args.json };
    let session = env.find_session(&args.session)?;
    let (log, test, good, bad) = (session.path.clone(), args.test.clone(), args.good, args.bad);
    let plan = tokio::task::spawn_blocking(move || -> anyhow::Result<BisectPlan> {
        let reader = SessionReader::<SessionRecord>::open(&log)
            .with_context(|| format!("cannot read session {}", log.display()))?;
        Ok(bisect::plan(&reader, &test, good, bad)?)
    })
    .await??;
    let config = Config::load(env.root(), cli.overrides())?.config;
    let ask = match bisect::policy_decision(&config.permissions, env.root(), &args.test)? {
        Decision::Deny { rule } => {
            eprintln!(
                "tautly: refused: the deny rule {} matches `{}`",
                printable(&rule),
                printable(&args.test)
            );
            return Ok(1);
        }
        Decision::Ask { reason } => Some(reason),
        Decision::Allow => None,
    };
    say.out(&summary(&plan, args.timeout, ask.as_deref()))?;
    if !args.yes {
        if !std::io::stdin().is_terminal() {
            say.out("nothing was run: run it again with --yes to run the check\n")?;
            return Ok(1);
        }
        say.out("run it? [y/N] ")?;
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim(), "y" | "Y" | "yes") {
            say.out("nothing was run\n")?;
            return Ok(1);
        }
    }
    let cancel = CancellationToken::new();
    let caught = Arc::new(AtomicI32::new(0));
    // Watched from here on, so no signal can end the process with a check still running
    // and the worktree still registered.
    let signals = Signals::new()?;
    let watcher = tokio::spawn(signals.cancel(cancel.clone(), Arc::clone(&caught)));
    let runner = BashRunner::new(provider_key_vars(&config), cancel);
    let done = bisect::run(
        &plan,
        env.workspace.sandbox().as_ref(),
        &runner,
        Duration::from_secs(args.timeout),
    )
    .await;
    watcher.abort();
    match done {
        Ok(out) => {
            for note in &out.notes {
                say.out(&format!("note: {}\n", printable(note)))?;
            }
            if args.json {
                println!("{}", as_json(&plan, &out));
            } else {
                print!("{}", result(&out));
            }
            Ok(0)
        }
        Err(e @ BisectError::EndpointMismatch { .. }) => {
            eprintln!("tautly: {}", printable(&e.to_string()));
            if let BisectError::EndpointMismatch {
                output: Some(output),
                ..
            } = &e
                && !output.is_empty()
            {
                eprintln!("the last lines of that run's output:");
                eprintln!("{}", printable_multiline(output));
            }
            Ok(2)
        }
        Err(BisectError::Cancelled) => {
            eprintln!("tautly: bisect cancelled; its worktree was removed");
            Ok(128 + caught.load(Ordering::SeqCst).max(0))
        }
        Err(e) => Err(e.into()),
    }
}

const SIGHUP: i32 = 1;
const SIGINT: i32 = 2;
const SIGTERM: i32 = 15;

/// Ctrl-C, SIGTERM and SIGHUP, watched from when this is made.
struct Signals {
    int: Signal,
    term: Signal,
    hup: Signal,
}

impl Signals {
    fn new() -> std::io::Result<Signals> {
        Ok(Signals {
            int: signal(SignalKind::interrupt())?,
            term: signal(SignalKind::terminate())?,
            hup: signal(SignalKind::hangup())?,
        })
    }

    /// On the first of them: keep its number in `caught`, then cancel `cancel`.
    async fn cancel(mut self, cancel: CancellationToken, caught: Arc<AtomicI32>) {
        let n = tokio::select! {
            Some(()) = self.int.recv() => SIGINT,
            Some(()) = self.term.recv() => SIGTERM,
            Some(()) = self.hup.recv() => SIGHUP,
            else => return,
        };
        caught.store(n, Ordering::SeqCst);
        cancel.cancel();
    }
}

/// What will run, before it does.
fn summary(plan: &BisectPlan, timeout: u64, ask: Option<&str>) -> String {
    let from = |e: Endpoint| match e {
        Endpoint::Given => "",
        Endpoint::Default => ", the default",
        Endpoint::Logged => ", from the session's logged runs of the command",
    };
    let mut s = format!(
        "bisect session {}: `{}` from step {} (good{}) to step {} (bad{}), {} step{} \
         between\n",
        printable(&plan.session),
        printable(&plan.test),
        plan.good,
        from(plan.good_from),
        plan.bad,
        from(plan.bad_from),
        plan.candidates.len(),
        if plan.candidates.len() == 1 { "" } else { "s" },
    );
    if !plan.skipped.is_empty() {
        s.push_str(&format!(
            "  not run (their files are not known exactly): {}\n",
            steps(&plan.skipped)
        ));
    }
    s.push_str(&format!(
        "  at most {} runs (about {} without skips), each at most {timeout}s, in a \
         temporary worktree\n",
        plan.max_runs(),
        plan.expected_runs()
    ));
    s.push_str(
        "  tautly writes nothing in this work tree, but the check is not confined (no OS \
         isolation): it runs the files of past states, which can write anywhere you can\n",
    );
    if let Some(reason) = ask {
        s.push_str(&format!(
            "  the policy asks before this command: {}\n",
            printable(reason)
        ));
    }
    s
}

/// The result, as text.
fn result(out: &BisectOutcome) -> String {
    let Some(step) = out.first_bad else {
        return "no failing step found\n".to_string();
    };
    let mut s = match out.turn {
        Some(turn) => format!("first failing step: {step} (user turn {turn})\n"),
        None => format!("first failing step: {step}\n"),
    };
    if !out.calls.is_empty() {
        let calls: Vec<String> = out.calls.iter().map(|c| printable(c)).collect();
        s.push_str(&format!("  calls: {}\n", calls.join("; ")));
    }
    if !out.text_excerpt.is_empty() {
        s.push_str(&format!("  text: {}\n", printable(&out.text_excerpt)));
    }
    if !out.uncertain.is_empty() {
        s.push_str(&format!(
            "  the change may instead be at step {} (not judged)\n",
            steps(&out.uncertain)
        ));
    }
    let skipped = if out.skipped.is_empty() {
        "none".to_string()
    } else {
        steps(&out.skipped)
    };
    s.push_str(&format!(
        "last passing step: {}; runs: {}; skipped: {skipped}\n",
        out.last_good, out.runs
    ));
    s
}

fn steps(list: &[u32]) -> String {
    list.iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

fn as_json(plan: &BisectPlan, out: &BisectOutcome) -> serde_json::Value {
    json!({
        "session_id": plan.session,
        "test": plan.test,
        "good": plan.good,
        "bad": plan.bad,
        "first_bad": out.first_bad,
        "turn": out.turn,
        "calls": out.calls,
        "text_excerpt": out.text_excerpt,
        "runs": out.runs,
        "skipped": out.skipped,
        "last_good": out.last_good,
        "uncertain": out.uncertain,
        "notes": out.notes,
    })
}
