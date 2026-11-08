//! `tautly eval` end to end on a scripted provider: each task runs `tautly -p` in a
//! temporary work tree its setup script makes, its verify script decides, and the results
//! JSON sums the event stream. `eval report` prints the benchmark table.

mod common;

use std::path::{Path, PathBuf};
use std::process::Output;

use common::{Scratch, stderr, stdout};
use serde_json::{Value, json};

const MODEL: &str = "anthropic/claude-sonnet-5-5";

/// Write task `id` under `dir`: its `task.toml` (after `id`, `prompt` and `approve =
/// "yolo"`), setup and verify scripts.
fn task(dir: &Path, id: &str, toml: &str, setup: &str, verify: &str) {
    task_with(
        dir,
        id,
        &format!("approve = \"yolo\"\n{toml}"),
        setup,
        verify,
    );
}

/// [`task`] without the `approve` line.
fn task_with(dir: &Path, id: &str, toml: &str, setup: &str, verify: &str) {
    let d = dir.join(id);
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(
        d.join("task.toml"),
        format!("id = \"{id}\"\nprompt = \"do the task\"\n{toml}"),
    )
    .unwrap();
    std::fs::write(d.join("setup.sh"), setup).unwrap();
    std::fs::write(d.join("verify.sh"), verify).unwrap();
}

const GIT_SETUP: &str = "set -e\ngit init -q\nprintf 'one\\n' > f.txt\ngit add .\n\
                         git -c user.name=t -c user.email=t@t commit -qm init\n";

fn call(id: &str, name: &str, args: Value) -> Value {
    json!({"tool_call": {"id": id, "name": name, "input_json": args.to_string()}})
}

fn usage(input: u64, output: u64, cache_read: u64) -> Value {
    json!({"usage": {"input": input, "output": output, "cache_read": cache_read, "cache_write": 0}})
}

fn turn(events: Vec<Value>, stop: &str) -> Value {
    let mut e = events;
    e.push(json!({"done": {"stop": stop}}));
    Value::Array(e)
}

fn text_turn(t: &str, u: Value) -> Value {
    turn(vec![json!({"text_delta": t}), u], "end_turn")
}

/// Run `tautly [--mock-script script] eval --tasks dir --model MODEL --out out <extra>`;
/// the output and the results JSON.
fn eval(s: &Scratch, script: Option<&str>, tasks: &Path, extra: &[&str]) -> (Output, Value) {
    let out_path = s.home.path().join("results.json");
    let _ = std::fs::remove_file(&out_path);
    let mut args: Vec<String> = Vec::new();
    if let Some(script) = script {
        args.extend(["--mock-script".into(), script.into()]);
    }
    args.extend(
        [
            "eval",
            "--tasks",
            &tasks.to_string_lossy(),
            "--model",
            MODEL,
            "--out",
            &out_path.to_string_lossy(),
        ]
        .map(String::from),
    );
    args.extend(extra.iter().map(|a| a.to_string()));
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = s.run(&refs);
    let results = std::fs::read_to_string(&out_path)
        .map(|t| serde_json::from_str(&t).unwrap())
        .unwrap_or(Value::Null);
    (out, results)
}

fn entry<'a>(results: &'a Value, id: &str) -> &'a Value {
    results["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == id)
        .unwrap_or_else(|| panic!("no task {id} in {results}"))
}

fn approx(a: &Value, b: f64) -> bool {
    a.as_f64().is_some_and(|a| (a - b).abs() < 1e-9)
}

#[test]
fn runs_task_in_temp_git_worktree_and_cleans_up() {
    let s = Scratch::new();
    let tasks = s.home.path().join("tasks");
    let marks = s.home.path().join("marks");
    std::fs::create_dir_all(&marks).unwrap();
    // The run's directory is private (0700): setup fails otherwise.
    let setup = format!(
        "{GIT_SETUP}[ \"$(ls -ld . | cut -c1-10)\" = drwx------ ] || exit 9\n\
         pwd -P > \"$EVAL_MARKS/$(basename \"$PWD\")\"\n"
    );
    task(
        &tasks,
        "edit-one",
        "",
        &setup,
        "git rev-parse --is-inside-work-tree && grep -qx two f.txt\n",
    );
    task(&tasks, "broken-setup", "", "exit 3\n", "exit 0\n");
    let script = s.script(json!([
        turn(
            vec![call("r1", "read", json!({"path": "f.txt"}))],
            "tool_use"
        ),
        turn(
            vec![call(
                "e1",
                "edit",
                json!({"path": "f.txt", "old_text": "one", "new_text": "two"})
            )],
            "tool_use"
        ),
        text_turn("done", usage(10, 1, 0)),
    ]));
    let mut cmd = s.command(&[]);
    let out_path = s.home.path().join("results.json");
    cmd.args([
        "--mock-script",
        &script,
        "eval",
        "--tasks",
        &tasks.to_string_lossy(),
        "--model",
        MODEL,
        "--out",
        &out_path.to_string_lossy(),
    ])
    .env("EVAL_MARKS", &marks);
    let out = cmd.output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let results: Value = serde_json::from_slice(&std::fs::read(&out_path).unwrap()).unwrap();
    let ok = entry(&results, "edit-one");
    assert_eq!(ok["passed"], true, "{results}\n{}", stderr(&out));
    let broken = entry(&results, "broken-setup");
    assert_eq!(broken["passed"], false);
    assert_eq!(broken["reason"], "setup_failed");

    // The task ran in its own directory, outside the current work tree, now removed.
    let ran: Vec<PathBuf> = std::fs::read_dir(&marks)
        .unwrap()
        .map(|e| PathBuf::from(std::fs::read_to_string(e.unwrap().path()).unwrap().trim()))
        .collect();
    assert_eq!(ran.len(), 1, "{ran:?}");
    let work = s.work.path().canonicalize().unwrap();
    assert!(!ran[0].starts_with(&work), "{ran:?}");
    assert!(!ran[0].exists(), "{} was not removed", ran[0].display());
    // Nothing was written in the current work tree.
    assert_eq!(std::fs::read_dir(s.work.path()).unwrap().count(), 0);
    // The run named its session, then left no session, index or lock behind (I4).
    assert!(ok["session_id"].is_string(), "{results}");
    assert_eq!(leftovers(s.home.path()), Vec::<String>::new());
}

/// What runs left in the data home under `sessions/`, `index/`, `locks/` and `memory/`.
fn leftovers(home: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for dir in ["sessions", "index", "locks", "memory"] {
        if let Ok(entries) = std::fs::read_dir(home.join(dir)) {
            out.extend(
                entries.map(|e| format!("{dir}/{}", e.unwrap().file_name().to_string_lossy())),
            );
        }
    }
    out
}

#[test]
fn verify_exit_0_is_pass() {
    let s = Scratch::new();
    let tasks = s.home.path().join("tasks");
    task(&tasks, "good", "", "exit 0\n", "exit 0\n");
    task(&tasks, "bad", "", "exit 0\n", "exit 1\n");
    let script = s.script(json!([text_turn("ok", usage(10, 1, 0))]));
    let (out, results) = eval(&s, Some(&script), &tasks, &[]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(entry(&results, "good")["passed"], true, "{results}");
    assert_eq!(entry(&results, "good")["reason"], Value::Null);
    assert_eq!(entry(&results, "bad")["passed"], false);
    assert_eq!(entry(&results, "bad")["reason"], "verify_failed");
    assert!(approx(&results["summary"]["pass_rate"], 0.5), "{results}");
    assert_eq!(results["model"], MODEL);
}

#[test]
fn respects_task_budget() {
    let s = Scratch::new();
    let tasks = s.home.path().join("tasks");
    task(
        &tasks,
        "pricey",
        "budget_usd = 0.001\n",
        "exit 0\n",
        "exit 0\n",
    );
    // 100,000 input tokens cost far more than $0.001.
    let script = s.script(json!([
        turn(
            vec![
                call("b1", "bash", json!({"command": "true"})),
                usage(100_000, 10, 0)
            ],
            "tool_use"
        ),
        text_turn("done", usage(100_000, 10, 0)),
    ]));
    let (out, results) = eval(&s, Some(&script), &tasks, &[]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let t = entry(&results, "pricey");
    assert_eq!(t["passed"], false, "{results}");
    assert_eq!(t["reason"], "budget", "{results}");
    assert!(t["cost_usd"].as_f64().unwrap() > 0.001, "{results}");
}

/// The edit tool's schema in the last request a session of this home sent.
fn edit_schema(home: &Path) -> Value {
    for project in std::fs::read_dir(home.join("sessions")).unwrap() {
        for f in std::fs::read_dir(project.unwrap().path()).unwrap() {
            let p = f.unwrap().path();
            if p.to_string_lossy().ends_with(".last-request.json") {
                let req: Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
                let tools = req["tools"].as_array().expect("tools");
                let edit = tools.iter().find(|t| t["name"] == "edit").expect("edit");
                return edit.clone();
            }
        }
    }
    panic!("no request recorded");
}

#[test]
fn edit_format_override_applied() {
    for (flag, field) in [
        (None, "old_text"),
        (Some("patch"), "patch"),
        (Some("hashline"), "ops"),
    ] {
        let s = Scratch::new();
        let tasks = s.home.path().join("tasks");
        task(&tasks, "t", "", "exit 0\n", "exit 0\n");
        let script = s.script(json!([text_turn("ok", usage(10, 1, 0))]));
        let mut extra: Vec<&str> = flag.map(|f| vec!["--edit-format", f]).unwrap_or_default();
        extra.push("--keep-logs");
        let (out, results) = eval(&s, Some(&script), &tasks, &extra);
        assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
        assert_eq!(
            results["edit_format"],
            flag.map_or(Value::Null, Value::from)
        );
        let schema = edit_schema(s.home.path()).to_string();
        assert!(
            schema.contains(&format!("\"{field}\"")),
            "{flag:?}: {schema}"
        );
    }
}

#[test]
fn summary_math() {
    let s = Scratch::new();
    let tasks = s.home.path().join("tasks");
    let setup =
        "printf 'def f():\\n    if x:\\n        a()\\n        b()\\n    return 1\\n' > f.py\n";
    task(&tasks, "solved", "", setup, "grep -q 'c()' f.py\n");
    task(&tasks, "unsolved", "", setup, "exit 1\n");
    let script = s.script(json!([
        turn(
            vec![
                call("r1", "read", json!({"path": "f.py"})),
                usage(1000, 100, 3000)
            ],
            "tool_use"
        ),
        // Located by the indentation stage.
        turn(
            vec![
                call(
                    "e1",
                    "edit",
                    json!({"path": "f.py", "old_text": "if x:\n    a()\n    b()",
                                          "new_text": "if x:\n    a()\n    c()\n    b()"})
                ),
                usage(2000, 200, 0),
            ],
            "tool_use"
        ),
        // Fails: no such text.
        turn(
            vec![
                call(
                    "e2",
                    "edit",
                    json!({"path": "f.py", "old_text": "zzz", "new_text": "y"})
                ),
                usage(500, 50, 500),
            ],
            "tool_use"
        ),
        text_turn("done", usage(500, 50, 500)),
        // The session-end episode, by the side model, after the turn finished.
        text_turn("episode", usage(300, 30, 0)),
    ]));
    let (out, r) = eval(&s, Some(&script), &tasks, &[]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let t = entry(&r, "solved");
    assert_eq!(t["passed"], true, "{r}");
    assert_eq!(t["tokens_in"], 4000, "{r}");
    assert_eq!(t["tokens_out"], 400);
    assert_eq!(t["cache_read"], 4000);
    assert_eq!(t["side_tokens"], 330);
    assert_eq!(t["edit_calls"], 2);
    assert_eq!(t["edit_failures"], 1);
    assert!(t["episode_cost_usd"].as_f64().unwrap() > 0.0, "{r}");
    let sum = &r["summary"];
    assert!(approx(&sum["pass_rate"], 0.5), "{sum}");
    // Σ(in + out + side) over both tasks, per solved task.
    assert!(
        approx(
            &sum["tokens_per_solved_task"],
            2.0 * (4000.0 + 400.0 + 330.0)
        ),
        "{sum}"
    );
    assert!(
        approx(&sum["cache_hit_ratio"], 8000.0 / (8000.0 + 8000.0)),
        "{sum}"
    );
    assert!(approx(&sum["edit_failure_rate"], 0.5), "{sum}");
    assert_eq!(sum["blocked_attempts"], 0);
    assert_eq!(sum["edit_repairs_by_stage"], json!({"indentation": 2}));
    let per_task: f64 = r["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["cost_usd"].as_f64().unwrap())
        .sum();
    assert!(approx(&sum["total_cost_usd"], per_task), "{sum}");
    let episodes = 2.0 * t["episode_cost_usd"].as_f64().unwrap();
    assert!(approx(&sum["episode_cost_usd"], episodes), "{sum}");
    // The text summary names the episodes' share on a line of its own.
    assert!(
        stdout(&out)
            .lines()
            .any(|l| l.contains("session-end episode")),
        "{}",
        stdout(&out)
    );
}

#[test]
fn trials_aggregate_mean_and_stdev() {
    let s = Scratch::new();
    let tasks = s.home.path().join("tasks");
    let counter = s.home.path().join("counter");
    // Passes on odd runs: trials 1 and 3.
    let verify = format!(
        "n=$(cat '{0}' 2>/dev/null || echo 0); n=$((n+1)); echo $n > '{0}'; [ $((n % 2)) -eq 1 ]\n",
        counter.display()
    );
    task(&tasks, "flaky", "", "exit 0\n", &verify);
    let script = s.script(json!([text_turn("ok", usage(100, 10, 0))]));
    let (out, r) = eval(&s, Some(&script), &tasks, &["--trials", "3"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let entries = r["tasks"].as_array().unwrap();
    assert_eq!(entries.len(), 3, "{r}");
    let trials: Vec<&Value> = entries.iter().map(|t| &t["trial"]).collect();
    assert_eq!(trials, [&json!(1), &json!(2), &json!(3)]);
    let sum = &r["summary"];
    assert_eq!(sum["trials"], 3);
    assert!(approx(&sum["pass_rate"], 2.0 / 3.0), "{sum}");
    assert!(approx(&sum["mean"]["pass_rate"], 2.0 / 3.0), "{sum}");
    assert!(
        approx(&sum["stdev"]["pass_rate"], (1.0f64 / 3.0).sqrt()),
        "{sum}"
    );
    // Every trial solved costs the same tokens: no spread.
    assert!(
        approx(&sum["mean"]["tokens_per_solved_task"], 110.0),
        "{sum}"
    );
    assert!(
        approx(&sum["stdev"]["tokens_per_solved_task"], 0.0),
        "{sum}"
    );
}

/// An evals directory: `starter/<id>` tasks, `replay/<id>.json` scripts and a baseline.
fn replay_dir(s: &Scratch, baseline: Option<f64>) -> PathBuf {
    let evals = s.home.path().join("evals");
    task(&evals.join("starter"), "one", "", "exit 0\n", "exit 0\n");
    task(&evals.join("starter"), "two", "", "exit 0\n", "exit 0\n");
    std::fs::create_dir_all(evals.join("replay")).unwrap();
    for id in ["one", "two"] {
        let script = json!([text_turn("ok", usage(1000, 100, 0))]);
        std::fs::write(evals.join(format!("replay/{id}.json")), script.to_string()).unwrap();
    }
    if let Some(b) = baseline {
        std::fs::write(
            evals.join("baseline.json"),
            json!({"tokens_per_solved_task": b}).to_string(),
        )
        .unwrap();
    }
    evals
}

fn replay(s: &Scratch, evals: &Path, extra: &[&str]) -> Output {
    let dir = evals.join("replay");
    let mut args = vec!["eval", "--replay"];
    let dir = dir.to_string_lossy().into_owned();
    args.push(&dir);
    args.extend(extra);
    s.run(&args)
}

#[test]
fn replay_regression_over_10_percent_fails() {
    let s = Scratch::new();
    // 1,100 tokens per solved task against 990: 11% more.
    let evals = replay_dir(&s, Some(990.0));
    let out = replay(&s, &evals, &[]);
    assert_eq!(
        out.status.code(),
        Some(1),
        "{}{}",
        stdout(&out),
        stderr(&out)
    );
    let all = format!("{}{}", stdout(&out), stderr(&out));
    assert!(all.contains("1,100") && all.contains("990"), "{all}");
}

#[test]
fn replay_within_10_percent_passes() {
    let s = Scratch::new();
    let evals = replay_dir(&s, Some(1000.0));
    let out = replay(&s, &evals, &[]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}{}",
        stdout(&out),
        stderr(&out)
    );

    // --update-baseline rewrites it with the measured figure.
    let out = replay(&s, &evals, &["--update-baseline"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let b: Value =
        serde_json::from_slice(&std::fs::read(evals.join("baseline.json")).unwrap()).unwrap();
    assert!(approx(&b["tokens_per_solved_task"], 1100.0), "{b}");
}

#[test]
fn report_markdown_golden() {
    let s = Scratch::new();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../benchmarks/tokens/fixtures/sample.json")
        .canonicalize()
        .unwrap();
    let out = s.run(&["eval", "report", &fixture.to_string_lossy(), "--markdown"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    insta::assert_snapshot!(stdout(&out));
}

#[test]
fn keep_logs_keeps_an_eval_session() {
    let s = Scratch::new();
    let tasks = s.home.path().join("tasks");
    task(&tasks, "t", "", "git init -q\n", "exit 0\n");
    let script = s.script(json!([text_turn("ok", usage(10, 1, 0))]));
    let (out, r) = eval(&s, Some(&script), &tasks, &["--keep-logs"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let id = entry(&r, "t")["session_id"].as_str().unwrap().to_string();
    // The kept log is the run's, marked as an eval session; index and lock are gone.
    let left = leftovers(s.home.path());
    assert!(left.iter().all(|l| l.starts_with("sessions/")), "{left:?}");
    let log = std::fs::read_dir(s.home.path().join("sessions"))
        .unwrap()
        .map(|p| p.unwrap().path().join(format!("{id}.jsonl")))
        .find(|p| p.exists())
        .expect("the run's log");
    let header: Value = serde_json::from_str(
        std::fs::read_to_string(log)
            .unwrap()
            .lines()
            .next()
            .unwrap(),
    )
    .unwrap();
    let header = header.to_string();
    assert!(header.contains(r#""origin":"eval""#), "{header}");
    assert!(header.contains(r#""eval""#), "{header}");
}

/// Whether any process's command line contains `needle`.
fn running(needle: &str) -> bool {
    let out = std::process::Command::new("ps")
        .args(["-axww", "-o", "command="])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .any(|l| l.contains(needle) && !l.contains("ps -axww"))
}

/// A task whose run sleeps in a bash call: its setup records the run's directory.
fn sleeping_task(s: &Scratch, toml: &str, secs: u32) -> (PathBuf, PathBuf, String) {
    let tasks = s.home.path().join("tasks");
    let marks = s.home.path().join("marks");
    std::fs::create_dir_all(&marks).unwrap();
    task(
        &tasks,
        "slow",
        toml,
        &format!("git init -q\npwd -P > '{}/ran'\n", marks.display()),
        "exit 0\n",
    );
    let script = s.script(json!([
        turn(
            vec![call(
                "b1",
                "bash",
                json!({"command": format!("sleep {secs}")})
            )],
            "tool_use"
        ),
        text_turn("done", usage(10, 1, 0)),
    ]));
    (tasks, marks, script)
}

fn ran_dir(marks: &Path) -> PathBuf {
    PathBuf::from(std::fs::read_to_string(marks.join("ran")).unwrap().trim())
}

#[test]
fn timeout_stops_the_run_and_cleans_up() {
    let s = Scratch::new();
    // Long enough for setup under load; the run sleeps far longer.
    let (tasks, marks, script) = sleeping_task(&s, "timeout_s = 8\n", 47);
    let (out, r) = eval(&s, Some(&script), &tasks, &[]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(entry(&r, "slow")["reason"], "timeout", "{r}");
    assert!(!ran_dir(&marks).exists());
    assert!(!running("sleep 47"), "the run's command survived");
    assert_eq!(leftovers(s.home.path()), Vec::<String>::new());
}

#[test]
fn cancel_stops_the_run_and_cleans_up() {
    let s = Scratch::new();
    let (tasks, marks, script) = sleeping_task(&s, "", 53);
    let out_path = s.home.path().join("results.json");
    let mut child = s
        .command(&[
            "--mock-script",
            &script,
            "eval",
            "--tasks",
            &tasks.to_string_lossy(),
            "--model",
            MODEL,
            "--out",
            &out_path.to_string_lossy(),
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let start = std::time::Instant::now();
    while !running("sleep 53") {
        assert!(
            start.elapsed().as_secs() < 30,
            "the run never started its command"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    // SAFETY: a signal to our own child.
    unsafe { libc::kill(child.id() as i32, libc::SIGINT) };
    let status = child.wait().unwrap();
    assert_eq!(status.code(), Some(130));
    let r: Value = serde_json::from_slice(&std::fs::read(&out_path).unwrap()).unwrap();
    assert_eq!(entry(&r, "slow")["reason"], "cancelled", "{r}");
    assert!(!ran_dir(&marks).exists());
    assert!(!running("sleep 53"), "the run's command survived");
}

#[test]
fn without_approve_asks_are_denied() {
    let s = Scratch::new();
    let tasks = s.home.path().join("tasks");
    // Passes only when the bash call never ran.
    task_with(&tasks, "plain", "", "git init -q\n", "[ ! -e made ]\n");
    let script = s.script(json!([
        turn(
            vec![call("b1", "bash", json!({"command": "touch made"}))],
            "tool_use"
        ),
        text_turn("done", usage(10, 1, 0)),
    ]));
    let (out, r) = eval(&s, Some(&script), &tasks, &[]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(entry(&r, "plain")["passed"], true, "{r}");
}

/// A replay set of one task whose setup writes `lines` lines that the script reads, with
/// no scripted usage: the engine counts what it sends. Its tokens per solved task.
fn replayed_tokens(lines: usize) -> f64 {
    let s = Scratch::new();
    let evals = s.home.path().join("evals");
    let setup = format!(
        "git init -q\nfor i in $(seq {lines}); do echo \"line $i of the file\"; done > big.txt\n"
    );
    task(&evals.join("starter"), "big", "", &setup, "exit 0\n");
    std::fs::create_dir_all(evals.join("replay")).unwrap();
    let script = json!([
        turn(
            vec![call("r1", "read", json!({"path": "big.txt"}))],
            "tool_use"
        ),
        turn(vec![json!({"text_delta": "read it"})], "end_turn"),
    ]);
    std::fs::write(evals.join("replay/big.json"), script.to_string()).unwrap();
    let out_path = s.home.path().join("r.json");
    let out = replay(
        &s,
        &evals,
        &["--update-baseline", "--out", &out_path.to_string_lossy()],
    );
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let r: Value = serde_json::from_slice(&std::fs::read(&out_path).unwrap()).unwrap();
    r["summary"]["tokens_per_solved_task"].as_f64().unwrap()
}

#[test]
fn replay_measures_what_the_engine_sends() {
    let small = replayed_tokens(20);
    let large = replayed_tokens(400);
    assert!(large > small * 1.1, "small {small}, large {large}");
}
