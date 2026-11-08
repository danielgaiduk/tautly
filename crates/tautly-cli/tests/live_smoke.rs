//! Live smoke tests against real providers (Task 35). Every test is `#[ignore]`: run them
//! with `cargo test -p tautly --test live_smoke -- --ignored --nocapture`.
//!
//! - A provider whose key variable is unset or empty is skipped (the test prints
//!   `SKIPPED` and passes without touching the network). With `TAUTLY_LIVE_REQUIRE=1` (the
//!   nightly) a cloud provider without its key fails instead, so a missing secret cannot
//!   turn the run green. Ollama is skipped when nothing answers on localhost:11434 or the
//!   model is not pulled.
//! - Every child (tautly, git, the model-written Python) starts from an empty environment
//!   plus `PATH`, `TMPDIR`, `LANG` and a scratch `HOME`, in its own process group, killed
//!   with its children on timeout. Only tautly gets a key, and only the one it needs, with a
//!   scratch `TAUTLY_HOME` and `TAUTLY_TEST_SECRET_STORE` pointing at an empty file (a build
//!   with `--features mock` then never opens the OS keychain).
//! - Models: the brief's ids, overridable with `TAUTLY_LIVE_ANTHROPIC_MODEL`,
//!   `TAUTLY_LIVE_OPENAI_MODEL`, `TAUTLY_LIVE_GOOGLE_MODEL`, `TAUTLY_LIVE_OLLAMA_MODEL` (an
//!   Ollama tag without the `compat:ollama/` prefix gets it).
//!
//! How to read the cache continuity tests: docs/benchmark.md, "Cache continuity". They
//! check the prompt cache only; a reasoning-continuity check comes with the lane that lets
//! the engine enable reasoning (it sends `reasoning: None` today).

use std::fs::File;
use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpStream};
use std::os::unix::process::CommandExt as _;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;
use tempfile::TempDir;

const RUN_TIMEOUT: Duration = Duration::from_secs(600);
const GIT_TIMEOUT: Duration = Duration::from_secs(60);
const PYTHON_TIMEOUT: Duration = Duration::from_secs(120);
const OLLAMA_PORT: u16 = 11434;
/// The session budget each run is given, and the most it may report spending (the cap is
/// checked between steps, so one step may overshoot).
const BUDGET_USD: &str = "0.50";
const MAX_SPENT_USD: f64 = 0.75;

struct Provider {
    name: &'static str,
    /// The key variable, `None` for a local server.
    key_var: Option<&'static str>,
    model_var: &'static str,
    default_model: &'static str,
    /// The final step of a tool loop must read from the prompt cache (explicit
    /// breakpoints or a reliable automatic cache). Gemini's implicit cache is best-effort,
    /// so its numbers are printed but not asserted.
    assert_cache: bool,
}

const ANTHROPIC: Provider = Provider {
    name: "anthropic",
    key_var: Some("ANTHROPIC_API_KEY"),
    model_var: "TAUTLY_LIVE_ANTHROPIC_MODEL",
    default_model: "anthropic/claude-sonnet-5-5",
    assert_cache: true,
};
const OPENAI: Provider = Provider {
    name: "openai",
    key_var: Some("OPENAI_API_KEY"),
    model_var: "TAUTLY_LIVE_OPENAI_MODEL",
    default_model: "openai/gpt-5",
    assert_cache: true,
};
const GOOGLE: Provider = Provider {
    name: "google",
    key_var: Some("GEMINI_API_KEY"),
    model_var: "TAUTLY_LIVE_GOOGLE_MODEL",
    default_model: "google/gemini-3-pro",
    assert_cache: false,
};
const OLLAMA: Provider = Provider {
    name: "ollama",
    key_var: None,
    model_var: "TAUTLY_LIVE_OLLAMA_MODEL",
    default_model: "compat:ollama/qwen2.5-coder:7b",
    assert_cache: false,
};

fn nonempty_env(var: &str) -> Option<String> {
    std::env::var(var).ok().filter(|v| !v.trim().is_empty())
}

impl Provider {
    fn model(&self) -> String {
        let model = nonempty_env(self.model_var).unwrap_or_else(|| self.default_model.to_string());
        if self.key_var.is_none() && !model.starts_with("compat:ollama/") {
            return format!("compat:ollama/{model}");
        }
        model
    }

    /// Why this provider cannot run here, or `None` when it can.
    fn skip_reason(&self) -> Option<String> {
        match self.key_var {
            Some(var) => nonempty_env(var)
                .is_none()
                .then(|| format!("{var} is not set")),
            None => {
                let model = self.model();
                let tag = model.strip_prefix("compat:ollama/").unwrap_or(&model);
                match ollama_tags() {
                    None => Some(format!("no Ollama server on localhost:{OLLAMA_PORT}")),
                    Some(tags) if !tags.contains(&format!("\"{tag}\"")) => {
                        Some(format!("Ollama has no model `{tag}` (ollama pull {tag})"))
                    }
                    Some(_) => None,
                }
            }
        }
    }

    /// Print why the test is skipped and return true, or return false to run it. Under
    /// `TAUTLY_LIVE_REQUIRE=1` a cloud provider that would be skipped fails instead.
    fn skipped(&self, test: &str) -> bool {
        match self.skip_reason() {
            Some(why)
                if self.key_var.is_some() && nonempty_env("TAUTLY_LIVE_REQUIRE").is_some() =>
            {
                panic!(
                    "{test} [{}]: {why}, and TAUTLY_LIVE_REQUIRE is set",
                    self.name
                )
            }
            Some(why) => {
                println!("SKIPPED {test} [{}]: {why}", self.name);
                true
            }
            None => false,
        }
    }
}

/// Ollama's `/api/tags` body, or `None` when nothing answers on the loopback port.
fn ollama_tags() -> Option<String> {
    let addr = SocketAddr::from(([127, 0, 0, 1], OLLAMA_PORT));
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_millis(300)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    s.write_all(b"GET /api/tags HTTP/1.0\r\nHost: localhost\r\n\r\n")
        .ok()?;
    let mut body = String::new();
    s.read_to_string(&mut body).ok()?;
    Some(body)
}

/// `program` in `dir` with an empty environment plus `PATH`, `TMPDIR`, `LANG` and
/// `HOME=home`, in a process group of its own, output discarded. No provider key, CI token
/// or runner variable reaches it.
fn isolated(program: &str, dir: &Path, home: &Path) -> Command {
    let mut cmd = Command::new(program);
    cmd.env_clear()
        .current_dir(dir)
        .env("HOME", home)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    for var in ["PATH", "TMPDIR", "LANG"] {
        if let Some(v) = std::env::var_os(var) {
            cmd.env(var, v);
        }
    }
    cmd
}

/// Run `cmd` to its exit code; past `timeout` its whole process group is killed and the
/// result is `None` (so is death by a signal).
fn run_bounded(cmd: &mut Command, timeout: Duration) -> Option<i32> {
    let mut child = cmd.spawn().expect("child starts");
    let start = Instant::now();
    loop {
        if let Some(st) = child.try_wait().unwrap() {
            return st.code();
        }
        if start.elapsed() > timeout {
            let pgid = i32::try_from(child.id()).unwrap();
            // SAFETY: kill(2) on our child's own process group; no memory is involved.
            unsafe { libc::kill(-pgid, libc::SIGKILL) };
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn git(dir: &Path, args: &[&str]) {
    let mut cmd = isolated("git", dir, dir);
    cmd.args([
        "-c",
        "user.name=tautly-live",
        "-c",
        "user.email=live@tautly.test",
    ])
    .args(args);
    assert_eq!(
        run_bounded(&mut cmd, GIT_TIMEOUT),
        Some(0),
        "git {args:?} failed"
    );
}

/// A git repository holding `files`, committed.
fn repo(files: &[(&str, &str)]) -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (path, text) in files {
        std::fs::write(dir.path().join(path), text).unwrap();
    }
    git(dir.path(), &["init", "-q"]);
    git(dir.path(), &["add", "-A"]);
    git(dir.path(), &["commit", "-qm", "init"]);
    dir
}

struct Run {
    code: Option<i32>,
    events: Vec<Value>,
    stderr: String,
}

impl Run {
    fn completed(&self) -> bool {
        self.events
            .iter()
            .any(|e| e["type"] == "turn_finished" && e["reason"] == "completed")
    }

    fn text(&self) -> String {
        self.events
            .iter()
            .filter(|e| e["type"] == "text_delta")
            .filter_map(|e| e["text"].as_str())
            .collect()
    }

    /// The main model's `usage` events of the turn (one per provider request), in order.
    fn main_steps(&self) -> Vec<&Value> {
        self.events
            .iter()
            .take_while(|e| e["type"] != "turn_finished")
            .filter(|e| e["type"] == "usage" && e["role"] == "main")
            .collect()
    }

    /// What every `usage` event says it cost, side model and session-end episode included.
    fn spent_usd(&self) -> f64 {
        self.events
            .iter()
            .filter(|e| e["type"] == "usage")
            .filter_map(|e| e["cost_usd"].as_f64())
            .sum()
    }

    /// Print the spend and, for a priced (cloud) provider, assert `--budget-usd` held.
    fn check_budget(&self, p: &Provider) {
        let spent = self.spent_usd();
        println!("  spent ${spent:.4} of ${BUDGET_USD} [{}]", p.name);
        if p.key_var.is_some() {
            assert!(
                spent <= MAX_SPENT_USD,
                "spent ${spent:.4}: --budget-usd {BUDGET_USD} did not hold"
            );
        }
    }

    fn report(&self) -> String {
        let errors: Vec<&Value> = self
            .events
            .iter()
            .filter(|e| e["type"] == "error" || e["type"] == "warning")
            .collect();
        format!(
            "exit {:?}; errors/warnings: {errors:?}\nstderr:\n{}",
            self.code, self.stderr
        )
    }
}

/// `tautly -p PROMPT --output json --model M <extra>` in `work`, isolated from the
/// developer's home, keychain and other keys.
fn run_tautly(p: &Provider, work: &Path, prompt: &str, extra: &[&str]) -> Run {
    let home = tempfile::tempdir().unwrap();
    run_tautly_in(p, work, home.path(), prompt, extra)
}

/// [`run_tautly`] with the scratch home (`HOME` and `TAUTLY_HOME`) given, so a later run
/// can continue the session (`--continue`) and read the user configuration there.
fn run_tautly_in(p: &Provider, work: &Path, home: &Path, prompt: &str, extra: &[&str]) -> Run {
    let store = home.join("secrets.json");
    std::fs::write(&store, "{}").unwrap();
    let out_path = home.join("events.jsonl");
    let err_path = home.join("stderr.txt");
    let model = p.model();

    let mut cmd = isolated(env!("CARGO_BIN_EXE_tautly"), work, home);
    cmd.args(["-p", prompt, "--output", "json", "--model", &model])
        .args(extra)
        .env("TAUTLY_HOME", home)
        .env("TAUTLY_TEST_SECRET_STORE", &store)
        // Files, not pipes: the poll loop never reads, so a pipe could fill and block.
        .stdout(File::create(&out_path).unwrap())
        .stderr(File::create(&err_path).unwrap());
    if let Some(var) = p.key_var {
        cmd.env(var, nonempty_env(var).unwrap());
    }
    let code = run_bounded(&mut cmd, RUN_TIMEOUT);
    let events = std::fs::read_to_string(&out_path)
        .unwrap()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    Run {
        code,
        events,
        stderr: std::fs::read_to_string(&err_path).unwrap_or_default(),
    }
}

const CALC: &str = "def add(a, b):\n    return a - b\n";
const TEST_CALC: &str = "import unittest\n\nfrom calc import add\n\n\nclass AddTest(unittest.TestCase):\n    def test_add(self):\n        self.assertEqual(add(2, 3), 5)\n\n\nif __name__ == \"__main__\":\n    unittest.main()\n";

/// Runs code the model wrote: isolated (no keys) and bounded (an endless loop is killed).
fn unittest_passes(dir: &Path) -> bool {
    let mut cmd = isolated("python3", dir, dir);
    cmd.args(["-m", "unittest", "-q", "test_calc"]);
    run_bounded(&mut cmd, PYTHON_TIMEOUT) == Some(0)
}

/// Step 1, first bullet: a temp repo with a failing unit test; the model makes it pass.
fn fix_failing_test(p: &Provider) {
    if p.skipped("fix_failing_test") {
        return;
    }
    let dir = repo(&[("calc.py", CALC), ("test_calc.py", TEST_CALC)]);
    assert!(
        !unittest_passes(dir.path()),
        "the fixture test must fail first"
    );

    let run = run_tautly(
        p,
        dir.path(),
        "make the failing test pass",
        &["--approve", "auto-edit", "--budget-usd", BUDGET_USD],
    );
    assert_eq!(run.code, Some(0), "{}", run.report());
    assert!(run.completed(), "turn did not complete: {}", run.report());
    run.check_budget(p);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("test_calc.py")).unwrap(),
        TEST_CALC,
        "the model changed the test instead of the code"
    );
    assert!(
        unittest_passes(dir.path()),
        "the test still fails: {}",
        run.report()
    );
    println!("PASSED fix_failing_test [{}] with {}", p.name, p.model());
}

/// Step 1, second bullet, cache half: a three-step tool loop (each file names the next,
/// under names that cannot be guessed, so the reads cannot be batched). It checks that the
/// provider accepts the replayed history (for OpenAI: reasoning items next to id-less
/// items) and prints per-step numbers; on providers with a dependable prompt cache the last
/// step must read from it, else the working-state tail is breaking the prefix. Reasoning
/// continuity itself is not checked: the engine does not enable reasoning yet.
fn cache_continuity(p: &Provider) {
    if p.skipped("cache_continuity") {
        return;
    }
    let dir = repo(&[
        ("start.txt", "Next file: q7-maple.txt\n"),
        ("q7-maple.txt", "Next file: z3-harbor.txt\n"),
        ("z3-harbor.txt", "DONE. The codeword is PERIWINKLE-42.\n"),
    ]);
    let prompt = "Read start.txt with the read tool. Each file names the next file to read; \
                  make one tool call per step, never several at once, and follow the chain \
                  until a file says DONE. Then reply with only the codeword it gives.";
    let run = run_tautly(p, dir.path(), prompt, &["--budget-usd", BUDGET_USD]);
    assert_eq!(run.code, Some(0), "{}", run.report());
    assert!(run.completed(), "turn did not complete: {}", run.report());
    run.check_budget(p);

    let steps = run.main_steps();
    println!("CACHE CONTINUITY [{}] {}", p.name, p.model());
    println!("  step  tokens_in  cache_read  tokens_out  cache_hit");
    for (i, u) in steps.iter().enumerate() {
        let tin = u["tokens_in"].as_u64().unwrap_or(0);
        let read = u["cache_read"].as_u64().unwrap_or(0);
        let ratio = read as f64 / (tin + read).max(1) as f64;
        println!(
            "  {:>4}  {tin:>9}  {read:>10}  {:>10}  {ratio:>9.2}",
            i + 1,
            u["tokens_out"].as_u64().unwrap_or(0)
        );
    }
    assert!(
        run.text().contains("PERIWINKLE-42"),
        "wrong final answer {:?}: {}",
        run.text(),
        run.report()
    );
    assert!(
        steps.len() >= 3,
        "expected a tool loop of at least 3 provider steps, got {}",
        steps.len()
    );
    if p.assert_cache {
        let last = steps.last().unwrap();
        assert!(
            last["cache_read"].as_u64().unwrap_or(0) > 0,
            "continuity lost: the last step read nothing from the prompt cache ({last}); \
             see docs/benchmark.md, \"Cache continuity\" (volatile_tail placement)"
        );
    }
}

/// The record types of the session log under `home` (one session), in order.
fn log_record_types(home: &Path) -> Vec<String> {
    let mut logs = Vec::new();
    for project in std::fs::read_dir(home.join("sessions")).unwrap() {
        for e in std::fs::read_dir(project.unwrap().path()).unwrap() {
            let path = e.unwrap().path();
            if path.extension().is_some_and(|x| x == "jsonl") {
                logs.push(path);
            }
        }
    }
    assert_eq!(logs.len(), 1, "{logs:?}");
    std::fs::read_to_string(&logs[0])
        .unwrap()
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter_map(|v| v["data"]["type"].as_str().map(str::to_string))
        .collect()
}

/// History edits mid-session (Anthropic): the provider must accept the next request after
/// a GC stub replaced a superseded tool result, a compaction summarized the oldest turns
/// and a session pin was folded into `<pinned>` (each at a cold cache epoch). The user
/// configuration makes the epochs happen: a 1 s cache lifetime (engine-side only, never
/// sent) and a compaction threshold any context passes.
fn history_edits(p: &Provider) {
    if p.skipped("history_edits") {
        return;
    }
    let dir = repo(&[("notes.txt", "The build uses make.\n")]);
    let home = tempfile::tempdir().unwrap();
    let config = home.path().join(".config/tautly");
    std::fs::create_dir_all(&config).unwrap();
    let model = p.model();
    std::fs::write(
        config.join("config.toml"),
        format!(
            "compaction_threshold = 0.01\n\
             [catalog_overrides.models.\"{model}\"]\n\
             context_window = 1000000\nmax_output = 128000\nparallel_tools = true\n\
             reasoning = \"effort_always\"\ncache = \"explicit\"\nimages = true\n\
             edit_format = \"str_replace\"\ncache_ttl_s = 1\n\
             [catalog_overrides.models.\"{model}\".price]\n\
             input = 2.0\noutput = 10.0\ncache_read = 0.2\ncache_write = 2.5\n"
        ),
    )
    .unwrap();
    let budget = ["--budget-usd", BUDGET_USD];
    let first = run_tautly_in(
        p,
        dir.path(),
        home.path(),
        "Read notes.txt with the read tool. Then call the pin tool once with the text \
         'The codeword is LAVENDER-7.' Then read notes.txt again with the read tool. Make \
         one tool call per step. Then reply with only the word DONE.",
        &budget,
    );
    assert_eq!(first.code, Some(0), "{}", first.report());
    assert!(
        first.completed(),
        "turn 1 did not complete: {}",
        first.report()
    );
    first.check_budget(p);
    // Past the cache lifetime: the next turn starts a cold epoch.
    std::thread::sleep(Duration::from_secs(2));
    let continued = [&budget[..], &["--continue"]].concat();
    let second = run_tautly_in(
        p,
        dir.path(),
        home.path(),
        "What is the codeword? Reply with only the codeword.",
        &continued,
    );
    assert_eq!(second.code, Some(0), "{}", second.report());
    assert!(
        second.completed(),
        "turn 2 did not complete: {}",
        second.report()
    );
    second.check_budget(p);
    assert!(
        second.text().contains("LAVENDER-7"),
        "the pinned codeword was lost: {:?}: {}",
        second.text(),
        second.report()
    );
    // Each edit happened (at a step of turn 1 that started past the lifetime, or at the
    // start of turn 2 at the latest) and a request went out after it; both turns completed,
    // so the provider accepted it.
    let types = log_record_types(home.path());
    for edit in ["gc_applied", "compaction", "pins_folded"] {
        let at = types
            .iter()
            .position(|t| t == edit)
            .unwrap_or_else(|| panic!("no {edit} in the session: {types:?}"));
        assert!(
            types[at..].iter().any(|t| t == "request"),
            "no request after {edit}: {types:?}"
        );
    }
    println!("PASSED history_edits [{}] with {model}", p.name);
}

/// Not live: the isolation the live tests rely on. `cargo test` sets `CARGO` for this
/// process, so a child that still sees it inherited the environment.
#[test]
fn isolated_children_get_a_clean_environment_and_a_deadline() {
    let dir = tempfile::tempdir().unwrap();
    let mut clean = isolated("sh", dir.path(), dir.path());
    clean.args([
        "-c",
        r#"[ -z "$CARGO" ] && [ -n "$PATH" ] && [ -d "$HOME" ]"#,
    ]);
    assert_eq!(run_bounded(&mut clean, GIT_TIMEOUT), Some(0));

    let mut stuck = isolated("sh", dir.path(), dir.path());
    stuck.args(["-c", "sleep 30 & sleep 30"]);
    let start = Instant::now();
    assert_eq!(run_bounded(&mut stuck, Duration::from_millis(300)), None);
    assert!(start.elapsed() < Duration::from_secs(5));
}

#[test]
#[ignore = "live: needs ANTHROPIC_API_KEY"]
fn anthropic_fixes_failing_test() {
    fix_failing_test(&ANTHROPIC);
}

#[test]
#[ignore = "live: needs OPENAI_API_KEY"]
fn openai_fixes_failing_test() {
    fix_failing_test(&OPENAI);
}

#[test]
#[ignore = "live: needs GEMINI_API_KEY"]
fn google_fixes_failing_test() {
    fix_failing_test(&GOOGLE);
}

#[test]
#[ignore = "live: needs a local Ollama with the model pulled"]
fn ollama_fixes_failing_test() {
    fix_failing_test(&OLLAMA);
}

#[test]
#[ignore = "live: needs ANTHROPIC_API_KEY"]
fn anthropic_cache_continuity() {
    cache_continuity(&ANTHROPIC);
}

#[test]
#[ignore = "live: needs OPENAI_API_KEY"]
fn openai_cache_continuity() {
    cache_continuity(&OPENAI);
}

#[test]
#[ignore = "live: needs GEMINI_API_KEY"]
fn google_cache_continuity() {
    cache_continuity(&GOOGLE);
}

#[test]
#[ignore = "live: needs ANTHROPIC_API_KEY"]
fn anthropic_history_edits() {
    history_edits(&ANTHROPIC);
}
