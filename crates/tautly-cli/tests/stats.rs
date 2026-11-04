//! `tautly stats` and `tautly receipt` (spec 7.11.1) over a seeded cost ledger and
//! session logs, each run in a scratch `TAUTLY_HOME` and work tree.

use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::Duration;

use serde_json::Value;
use tautly_core::{Event, Ledger, ModelId, RawUsage, Savings, SessionRecord, TestEvidence};
use tautly_session::{HeaderInit, SessionWriter};
use tempfile::TempDir;

const DAY_MS: i64 = 86_400_000;

struct Scratch {
    home: TempDir,
    work: TempDir,
}

impl Scratch {
    fn new() -> Scratch {
        Scratch {
            home: tempfile::tempdir().unwrap(),
            work: tempfile::tempdir().unwrap(),
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_tautly"))
            .args(args)
            .current_dir(self.work.path())
            .env("TAUTLY_HOME", self.home.path())
            .env("HOME", self.home.path())
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_DATA_HOME")
            .output()
            .unwrap()
    }

    /// Run `args`, which must succeed; stdout.
    fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert_eq!(
            out.status.code(),
            Some(0),
            "{args:?}: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    fn root(&self) -> PathBuf {
        self.work.path().canonicalize().unwrap()
    }

    fn ledger(&self) -> Ledger {
        Ledger::open(&self.home.path().join("ledger.sqlite")).unwrap()
    }

    /// Move every row of `session` `days` back in time.
    fn age(&self, session: &str, days: i64) {
        let conn = rusqlite::Connection::open(self.home.path().join("ledger.sqlite")).unwrap();
        conn.execute(
            "UPDATE usage SET at_ms = at_ms - ?1 WHERE session = ?2",
            rusqlite::params![days * DAY_MS, session],
        )
        .unwrap();
        conn.execute(
            "UPDATE receipts SET at = at - ?1 WHERE session = ?2",
            rusqlite::params![days * DAY_MS, session],
        )
        .unwrap();
    }

    /// A session log of this work tree with one user turn (`prompt`) ending with
    /// `evidence`; its id.
    fn session(&self, prompt: &str, evidence: Option<TestEvidence>) -> String {
        let mut w = SessionWriter::<SessionRecord>::create_in(
            self.home.path(),
            &self.root(),
            HeaderInit::default(),
        )
        .unwrap();
        w.append(&SessionRecord::TurnStart {
            turn: 1,
            text: prompt.into(),
        })
        .unwrap();
        w.append(&SessionRecord::TurnEnd {
            reason: tautly_core::FinishReason::Completed,
            evidence,
        })
        .unwrap();
        w.checkpoint().unwrap();
        // Sessions are listed by creation time.
        std::thread::sleep(Duration::from_millis(5));
        w.session_id().to_string()
    }
}

fn usage(input: u64, output: u64, cache_read: u64) -> RawUsage {
    RawUsage {
        input,
        output,
        cache_read,
        cache_write: 0,
    }
}

#[allow(clippy::too_many_arguments)]
fn receipt(
    turn: u32,
    model: &str,
    tokens_in: u64,
    tokens_out: u64,
    cost: Option<f64>,
    wall_ms: u64,
    saved: [u64; 4],
    estimated: bool,
) -> Event {
    Event::TurnReceipt {
        turn,
        model: ModelId::from(model),
        tokens_in,
        tokens_out,
        cache_read: 600,
        cache_write: 40,
        side_tokens: 100,
        cost_usd: cost,
        wall_ms,
        cache_hit_ratio: None,
        steps: 3,
        tool_calls: 4,
        saved: Savings {
            index: saved[0],
            diff_reads: saved[1],
            context_gc: saved[2],
            filters: saved[3],
            cache_read_tokens: 600,
            cache_usd: cost.map(|_| 0.0015),
            total_usd: cost.map(|_| 0.0125),
        },
        estimated,
    }
}

/// Two projects and two models: session `s1` (project `p1`, model `m/a`) today, session
/// `s2` (project `p2`, model `m/b`, plus a side model `m/small`) two days ago, and session
/// `s3` (project `p1`) forty days ago.
fn seed(s: &Scratch) {
    let l = s.ledger();
    let root = s.root();
    l.record(
        "s1",
        &ModelId::from("m/a"),
        &usage(1000, 100, 0),
        Some(0.01),
    )
    .unwrap();
    l.record(
        "s1",
        &ModelId::from("m/a"),
        &usage(500, 50, 1500),
        Some(0.005),
    )
    .unwrap();
    l.record_receipt(
        "s1",
        "p1",
        &root,
        &receipt(
            1,
            "m/a",
            1500,
            150,
            Some(0.015),
            2000,
            [100, 20, 300, 40],
            false,
        ),
    )
    .unwrap();
    l.record(
        "s2",
        &ModelId::from("m/b"),
        &usage(2000, 200, 0),
        Some(0.02),
    )
    .unwrap();
    l.record_estimated("s2", &ModelId::from("m/small"), &usage(300, 30, 0), None)
        .unwrap();
    l.record_receipt(
        "s2",
        "p2",
        &root,
        &receipt(1, "m/b", 2300, 230, None, 3000, [10, 0, 0, 0], true),
    )
    .unwrap();
    l.record_receipt(
        "s2",
        "p2",
        &root,
        &receipt(2, "m/b", 0, 0, None, 500, [0, 0, 0, 5], false),
    )
    .unwrap();
    s.age("s2", 2);
    l.record(
        "s3",
        &ModelId::from("m/a"),
        &usage(9000, 900, 0),
        Some(0.09),
    )
    .unwrap();
    l.record_receipt(
        "s3",
        "p1",
        &root,
        &receipt(1, "m/a", 9000, 900, Some(0.09), 9000, [1, 1, 1, 1], false),
    )
    .unwrap();
    s.age("s3", 40);
}

fn rows(json: &str) -> Vec<Value> {
    let v: Value = serde_json::from_str(json).unwrap();
    v["rows"].as_array().unwrap().clone()
}

fn by_key<'a>(rows: &'a [Value], key: &str) -> &'a Value {
    rows.iter()
        .find(|r| r["key"] == key)
        .unwrap_or_else(|| panic!("no row {key} in {rows:?}"))
}

#[test]
fn stats_groups_by_day_model_project() {
    let s = Scratch::new();
    seed(&s);

    // By day (the default), the last 30 days (the default): s1 today, s2 two days ago.
    let days = rows(&s.ok(&["stats", "--json"]));
    assert_eq!(days.len(), 2, "{days:?}");
    let (older, today) = (&days[0], &days[1]);
    assert!(older["key"].as_str().unwrap() < today["key"].as_str().unwrap());
    assert_eq!(today["turns"], 1);
    assert_eq!(today["tokens"], 1000 + 100 + 500 + 50 + 1500);
    assert_eq!(today["saved_tokens"], 460);
    assert_eq!(today["wall_ms"], 2000);
    assert_eq!(today["estimated"], 0);
    assert!((today["cost_usd"].as_f64().unwrap() - 0.015).abs() < 1e-9);
    // 1500 cache read of 1500 + 1500 input.
    assert!((today["cache_hit_ratio"].as_f64().unwrap() - 0.5).abs() < 1e-9);
    assert_eq!(older["turns"], 2);
    assert_eq!(older["tokens"], 2200 + 330);
    assert_eq!(
        (older["unpriced"].clone(), older["estimated"].clone()),
        (1.into(), 1.into())
    );
    assert_eq!(older["saved_usd"], Value::Null);

    // `--since 7d` keeps the same two; `all` adds the forty-day-old session.
    assert_eq!(rows(&s.ok(&["stats", "--since", "7d", "--json"])).len(), 2);
    assert_eq!(rows(&s.ok(&["stats", "--since", "all", "--json"])).len(), 3);

    // By model: usage rows by the model billed, receipts by the turn's main model.
    let models = rows(&s.ok(&["stats", "--since", "all", "--by", "model", "--json"]));
    let keys: Vec<&str> = models.iter().map(|r| r["key"].as_str().unwrap()).collect();
    assert_eq!(keys, ["m/a", "m/b", "m/small"]);
    assert_eq!(by_key(&models, "m/a")["turns"], 2);
    assert_eq!(by_key(&models, "m/small")["turns"], 0);
    assert_eq!(by_key(&models, "m/small")["tokens"], 330);

    // By project: usage rows join their session's receipts.
    let projects = rows(&s.ok(&["stats", "--since", "all", "--by", "project", "--json"]));
    let keys: Vec<&str> = projects
        .iter()
        .map(|r| r["key"].as_str().unwrap())
        .collect();
    assert_eq!(keys, ["p1", "p2"]);
    assert_eq!(by_key(&projects, "p2")["tokens"], 2530);
    assert_eq!(by_key(&projects, "p1")["turns"], 2);
    assert_eq!(by_key(&projects, "p1")["wall_ms"], 11_000);

    // The table says what is estimated.
    let table = s.ok(&["stats", "--by", "model"]);
    assert!(table.contains("m/a") && table.contains("m/b"), "{table}");
    assert!(table.contains("est."), "{table}");
}

#[test]
fn stats_json_shape() {
    let s = Scratch::new();
    seed(&s);
    let out = s.ok(&["stats", "--since", "all", "--by", "model", "--json"]);
    let v: Value = serde_json::from_str(&out).unwrap();
    insta::assert_snapshot!("stats_json", serde_json::to_string_pretty(&v).unwrap());
}

#[test]
fn stats_without_a_ledger_says_so() {
    let s = Scratch::new();
    let out = s.ok(&["stats"]);
    assert!(out.contains("no usage"), "{out}");
}

fn evidence() -> TestEvidence {
    TestEvidence {
        command: "cargo test --manifest-path /secret/place/Cargo.toml".into(),
        exit_code: Some(0),
        passed: Some(12),
        failed: Some(0),
        step: 2,
        write_gen: 1,
        stale: false,
        edits_since: 0,
        execs_since: 0,
    }
}

#[test]
fn receipt_markdown_labels_estimates_and_has_no_paths_prompts_or_project() {
    let s = Scratch::new();
    let id = s.session("SECRET-PROMPT about the code", Some(evidence()));
    let root = s.root();
    let project = tautly_session::paths::project_id(&root);
    let l = s.ledger();
    l.record_receipt(
        &id,
        &project,
        &root,
        &receipt(
            1,
            "m/a",
            1500,
            150,
            Some(0.015),
            2000,
            [100, 20, 300, 40],
            true,
        ),
    )
    .unwrap();
    l.record_receipt(
        &id,
        &project,
        &root,
        &receipt(2, "m/a", 500, 50, Some(0.005), 1000, [0, 0, 0, 0], false),
    )
    .unwrap();

    let md = s.ok(&["receipt", "--markdown"]);
    assert!(md.contains("est."), "{md}");
    assert!(md.contains('|'), "a table: {md}");
    assert!(md.contains("2,000"), "tokens in: {md}");
    assert!(md.contains("12 passed"), "the last turn's evidence: {md}");
    let name = root.file_name().unwrap().to_string_lossy().into_owned();
    for secret in [
        root.to_string_lossy().as_ref(),
        name.as_str(),
        project.as_str(),
        "SECRET-PROMPT",
        "/secret/place",
    ] {
        assert!(!md.contains(secret), "{secret:?} in {md}");
    }
    let plain = s.ok(&["receipt"]);
    assert!(plain.contains("est."), "{plain}");
    assert!(!plain.contains(root.to_string_lossy().as_ref()), "{plain}");
    assert!(!plain.contains("SECRET-PROMPT"), "{plain}");
}

/// The `turns` figure of a plain receipt.
fn turns(out: &str) -> u32 {
    out.lines()
        .find_map(|l| l.strip_prefix("turns"))
        .unwrap_or_else(|| panic!("no turns line in {out}"))
        .trim()
        .parse()
        .unwrap()
}

#[test]
fn receipt_session_id_and_last() {
    let s = Scratch::new();
    let root = s.root();
    let project = tautly_session::paths::project_id(&root);
    let first = s.session("one", None);
    let second = s.session("two", None);
    let l = s.ledger();
    l.record_receipt(
        &first,
        &project,
        &root,
        &receipt(1, "m/a", 1, 1, None, 1, [0; 4], false),
    )
    .unwrap();
    for turn in 1..=2 {
        l.record_receipt(
            &second,
            &project,
            &root,
            &receipt(turn, "m/a", 1, 1, None, 1, [0; 4], false),
        )
        .unwrap();
    }

    assert_eq!(
        turns(&s.ok(&["receipt"])),
        2,
        "the newest session by default"
    );
    assert_eq!(turns(&s.ok(&["receipt", "--last"])), 2);
    assert_eq!(turns(&s.ok(&["receipt", "--session", &first])), 1);
    // A prefix that names one session is enough.
    assert_eq!(turns(&s.ok(&["receipt", "--session", &first[..20]])), 1);

    let out = s.run(&["receipt", "--session", "01NOSUCHSESSION"]);
    assert_eq!(out.status.code(), Some(1));
    let out = s.run(&["receipt", "--last", "--session", &first]);
    assert_eq!(out.status.code(), Some(2), "--last and --session conflict");
}

#[test]
fn receipt_without_a_session_or_receipts_says_so() {
    let s = Scratch::new();
    let out = s.run(&["receipt"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("no session"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let id = s.session("x", None);
    let out = s.run(&["receipt"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("no receipts"),
        "{id}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}
