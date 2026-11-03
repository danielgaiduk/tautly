//! The evidence footer (spec 7.11.7): the last test run of a turn, and whether what it
//! showed still holds.
//!
//! A `bash` call is a *test run* when any component of its parsed command matches a test
//! pattern (permission-rule glob syntax). The [`EvidenceTracker`] watches the turn's tool
//! results and keeps the last test run as a [`TestEvidence`]; `TurnFinished` carries it.
//! The run is *stale* once a later step applied an `Edited` / `Deleted` effect or a later
//! `Exec { may_write: true }` that was not itself a test run. Staleness comes from counting
//! those effects (`edits_since`, `execs_since`), not from comparing write generations:
//! `write_gen` is reported for clients, never consulted here. Since `bash` always reports
//! `Exec { may_write: true }`, any later non-test `bash` call, even `git status`, makes the
//! run stale. No claim of the model is parsed: only the command, the exit code, the output
//! filter's counts and the effects.

use std::sync::Arc;

use globset::{Glob, GlobMatcher};
use serde::{Deserialize, Serialize};
use tautly_sandbox::{PolicyError, parse_commands};
use tautly_tools::{AppliedEffect, ToolResult};

/// Flags that make a test command build or list its tests without running them
/// (`cargo test --no-run`, `jest --no-execute`, `pytest --collect-only`): a component
/// carrying one is not a test run.
const COMPILE_ONLY_FLAGS: &[&str] = &["--no-run", "--no-execute", "--collect-only"];

/// Longest `TestEvidence::command`, in characters.
const MAX_COMMAND_CHARS: usize = 200;

/// Test commands recognized without configuration (spec 7.11.7).
pub const DEFAULT_TEST_COMMANDS: &[&str] = &[
    "cargo test*",
    "cargo nextest run*",
    "npm test*",
    "npm run test*",
    "pnpm test*",
    "pnpm run test*",
    "yarn test*",
    "bun test*",
    "pytest*",
    "python -m pytest*",
    "python3 -m pytest*",
    "uv run pytest*",
    "go test*",
    "jest*",
    "npx jest*",
    "vitest*",
    "npx vitest*",
    "make test*",
    "mvn test*",
    "gradle test*",
    "./gradlew test*",
    "rspec*",
    "bundle exec rspec*",
    "mix test*",
    "dotnet test*",
    "ctest*",
];

/// `[evidence]`: user and CLI layers only (a repository could otherwise label any command
/// a passing test).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceConfig {
    /// Patterns added to the defaults (permission-rule glob syntax, matched against each
    /// component of a command).
    #[serde(default)]
    pub test_commands: Vec<String>,
    /// Use only `test_commands`, not [`DEFAULT_TEST_COMMANDS`].
    #[serde(default)]
    pub replace_defaults: bool,
}

/// Decides whether a `bash` command line is a test run.
#[derive(Debug, Clone)]
pub struct TestMatcher {
    globs: Vec<GlobMatcher>,
}

impl TestMatcher {
    /// The defaults (unless `cfg.replace_defaults`) plus `cfg.test_commands`. A pattern
    /// that is empty or not a valid glob is an error naming it.
    pub fn new(cfg: &EvidenceConfig) -> Result<Self, PolicyError> {
        let defaults = DEFAULT_TEST_COMMANDS
            .iter()
            .copied()
            .filter(|_| !cfg.replace_defaults);
        let added = cfg.test_commands.iter().map(String::as_str);
        let globs = defaults
            .chain(added)
            .map(|pattern| {
                let bad = |why: String| PolicyError::MalformedRule {
                    rule: pattern.to_string(),
                    why,
                };
                if pattern.trim().is_empty() {
                    return Err(bad("a test pattern must not be empty".into()));
                }
                Glob::new(pattern)
                    .map(|g| g.compile_matcher())
                    .map_err(|e| bad(e.to_string()))
            })
            .collect::<Result<_, _>>()?;
        Ok(TestMatcher { globs })
    }

    /// Whether any parsed component of `raw_cmd` matches a test pattern, the way a
    /// permission rule sees it (the words, with and without leading `NAME=VALUE`
    /// assignments). A command that cannot be parsed is not a test run, and neither is a
    /// component that only compiles or lists tests ([`COMPILE_ONLY_FLAGS`]).
    ///
    /// It is a heuristic over the command line, not a proof: `cargo test --help` or
    /// `true || cargo test` still count, and a run's exit code and counts are what say
    /// whether anything ran.
    pub fn is_test(&self, raw_cmd: &str) -> bool {
        let Ok(commands) = parse_commands(raw_cmd) else {
            return false;
        };
        commands
            .iter()
            .filter(|c| {
                !c.argv
                    .iter()
                    .any(|w| COMPILE_ONLY_FLAGS.contains(&w.as_str()))
            })
            .any(|c| {
                let words = c.argv.join(" ");
                let with_env = c
                    .assignments
                    .iter()
                    .chain(&c.argv)
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join(" ");
                self.globs
                    .iter()
                    .any(|g| g.is_match(&words) || g.is_match(&with_env))
            })
    }
}

/// The last test run of a turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestEvidence {
    /// The command, whitespace collapsed, at most 200 characters.
    pub command: String,
    /// None when the run was killed (timeout, cancel).
    pub exit_code: Option<i32>,
    /// From the output filter's counts, when it parsed them (`failed` includes errors).
    pub passed: Option<u32>,
    pub failed: Option<u32>,
    /// The step (context turn number) that ran it.
    pub step: u32,
    /// The write generation right after the run.
    pub write_gen: u64,
    pub stale: bool,
    /// `Edited` / `Deleted` effects applied after the run.
    pub edits_since: u32,
    /// `Exec { may_write: true }` effects of later non-test commands.
    pub execs_since: u32,
}

/// Follows one turn's tool results.
#[derive(Debug, Clone)]
pub struct EvidenceTracker {
    matcher: Arc<TestMatcher>,
    last: Option<TestEvidence>,
}

impl EvidenceTracker {
    pub fn new(matcher: Arc<TestMatcher>) -> Self {
        EvidenceTracker {
            matcher,
            last: None,
        }
    }

    /// An empty tracker with the same matcher (the next turn's).
    pub fn fresh(&self) -> Self {
        EvidenceTracker::new(Arc::clone(&self.matcher))
    }

    /// A call that ran, in the order the engine settled them: `raw_cmd` is a `bash`
    /// call's command, `write_gen` the write generation after the call's effects were
    /// counted. A test run replaces the evidence (fresh, nothing since); any other call
    /// ages it by its `Edited` / `Deleted` and `Exec { may_write: true }` effects.
    pub fn on_tool(
        &mut self,
        step: u32,
        raw_cmd: Option<&str>,
        result: &ToolResult,
        write_gen: u64,
    ) {
        if let Some(cmd) = raw_cmd.filter(|c| self.matcher.is_test(c)) {
            self.last = Some(TestEvidence {
                command: collapse(cmd),
                exit_code: result.meta.exit_code,
                passed: result.meta.counts.map(|c| c.passed),
                failed: result.meta.counts.map(|c| c.failed),
                step,
                write_gen,
                stale: false,
                edits_since: 0,
                execs_since: 0,
            });
            return;
        }
        let Some(last) = self.last.as_mut() else {
            return;
        };
        for effect in &result.effects {
            match effect {
                AppliedEffect::Edited { .. } | AppliedEffect::Deleted { .. } => {
                    last.edits_since = last.edits_since.saturating_add(1);
                }
                AppliedEffect::Exec { may_write: true } => {
                    last.execs_since = last.execs_since.saturating_add(1);
                }
                AppliedEffect::Exec { may_write: false } | AppliedEffect::Read { .. } => {}
            }
        }
        last.stale = last.edits_since > 0 || last.execs_since > 0;
    }

    /// A write was abandoned half way (cancel): what it changed is unknown, so it counts
    /// like a command that may have written.
    pub fn on_lost_write(&mut self) {
        if let Some(last) = self.last.as_mut() {
            last.execs_since = last.execs_since.saturating_add(1);
            last.stale = true;
        }
    }

    /// The turn's evidence: its last test run, if it ran one.
    pub fn finish(self) -> Option<TestEvidence> {
        self.last
    }
}

/// `cmd` with runs of whitespace (newlines too) collapsed to one space, cut to
/// [`MAX_COMMAND_CHARS`] characters, the last of them `…` when it was cut.
fn collapse(cmd: &str) -> String {
    let one_line = cmd.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= MAX_COMMAND_CHARS {
        return one_line;
    }
    let mut cut: String = one_line.chars().take(MAX_COMMAND_CHARS - 1).collect();
    cut.push('…');
    cut
}
