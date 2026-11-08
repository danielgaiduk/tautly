//! Live smoke tests against real providers (Task 35). Every test is `#[ignore]`: run them
//! with `cargo test -p tautly --test live_smoke -- --ignored --nocapture`.
//!
//! - A provider whose key variable is unset or empty is skipped (the test prints
//!   `SKIPPED` and passes without touching the network). Ollama is skipped when nothing
//!   answers on localhost:11434 or the model is not pulled.
//! - Keys come only from the environment. The child gets only the one key it needs, a
//!   scratch `TAUTLY_HOME`/`HOME`, and `TAUTLY_TEST_SECRET_STORE` pointing at an empty file
//!   (a build with `--features mock` then never opens the OS keychain).
//! - Models: the brief's ids, overridable with `TAUTLY_LIVE_ANTHROPIC_MODEL`,
//!   `TAUTLY_LIVE_OPENAI_MODEL`, `TAUTLY_LIVE_GOOGLE_MODEL`, `TAUTLY_LIVE_OLLAMA_MODEL`.
//!
//! How to read the continuity tests: docs/benchmark.md, "Reasoning continuity".

use std::fs::File;
use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;
use tempfile::TempDir;

/// Every provider key variable; the child inherits none but the one it needs.
const KEY_VARS: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "OPENAI_API_KEY",
    "GEMINI_API_KEY",
    "OPENROUTER_API_KEY",
];
const RUN_TIMEOUT: Duration = Duration::from_secs(600);
const OLLAMA_PORT: u16 = 11434;

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
        nonempty_env(self.model_var).unwrap_or_else(|| self.default_model.to_string())
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

    /// Print why the test is skipped and return true, or return false to run it.
    fn skipped(&self, test: &str) -> bool {
        match self.skip_reason() {
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

fn git(dir: &Path, args: &[&str]) {
    let st = Command::new("git")
        .args([
            "-c",
            "user.name=tautly-live",
            "-c",
            "user.email=live@tautly.test",
        ])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .status()
        .expect("git runs");
    assert!(st.success(), "git {args:?} failed");
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
    let store = home.path().join("secrets.json");
    std::fs::write(&store, "{}").unwrap();
    let out_path = home.path().join("events.jsonl");
    let err_path = home.path().join("stderr.txt");
    let model = p.model();

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tautly"));
    cmd.args(["-p", prompt, "--output", "json", "--model", &model])
        .args(extra)
        .current_dir(work)
        .env("TAUTLY_HOME", home.path())
        .env("HOME", home.path())
        .env("TAUTLY_TEST_SECRET_STORE", &store)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_DATA_HOME")
        .stdin(Stdio::null())
        // Files, not pipes: the poll loop below never reads, so a pipe could fill and block.
        .stdout(File::create(&out_path).unwrap())
        .stderr(File::create(&err_path).unwrap());
    for var in KEY_VARS {
        cmd.env_remove(var);
    }
    if let Some(var) = p.key_var {
        cmd.env(var, nonempty_env(var).unwrap());
    }

    let mut child = cmd.spawn().expect("tautly starts");
    let start = Instant::now();
    let code = loop {
        if let Some(st) = child.try_wait().unwrap() {
            break st.code();
        }
        if start.elapsed() > RUN_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(200));
    };
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

fn unittest_passes(dir: &Path) -> bool {
    Command::new("python3")
        .args(["-m", "unittest", "-q", "test_calc"])
        .current_dir(dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("python3 runs")
        .success()
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
        &["--approve", "auto-edit", "--budget-usd", "0.50"],
    );
    assert_eq!(run.code, Some(0), "{}", run.report());
    assert!(run.completed(), "turn did not complete: {}", run.report());
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

/// Step 1, second bullet: a three-step tool loop (each file names the next). It checks
/// that the provider accepts the replayed history (for OpenAI: reasoning items next to
/// id-less items) and prints per-step numbers; on providers with a dependable prompt cache
/// the last step must read from it, else the working-state tail is breaking the prefix.
fn reasoning_continuity(p: &Provider) {
    if p.skipped("reasoning_continuity") {
        return;
    }
    let dir = repo(&[
        ("hop1.txt", "Next file: hop2.txt\n"),
        ("hop2.txt", "Next file: hop3.txt\n"),
        ("hop3.txt", "DONE. The codeword is PERIWINKLE-42.\n"),
    ]);
    let prompt = "Read hop1.txt with the read tool. Each file names the next file to read; \
                  read them one per step, in order, until a file says DONE. Then reply with \
                  only the codeword it gives.";
    let run = run_tautly(p, dir.path(), prompt, &["--budget-usd", "0.50"]);
    assert_eq!(run.code, Some(0), "{}", run.report());
    assert!(run.completed(), "turn did not complete: {}", run.report());

    let steps = run.main_steps();
    println!("CONTINUITY [{}] {}", p.name, p.model());
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
             see docs/benchmark.md, \"Reasoning continuity\" (volatile_tail placement)"
        );
    }
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
fn anthropic_reasoning_continuity() {
    reasoning_continuity(&ANTHROPIC);
}

#[test]
#[ignore = "live: needs OPENAI_API_KEY"]
fn openai_reasoning_continuity() {
    reasoning_continuity(&OPENAI);
}

#[test]
#[ignore = "live: needs GEMINI_API_KEY"]
fn google_reasoning_continuity() {
    reasoning_continuity(&GOOGLE);
}
