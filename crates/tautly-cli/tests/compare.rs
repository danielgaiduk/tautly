//! `tautly compare` (spec 7.12.6, Task 52) over session logs written record by record in a
//! scratch `TAUTLY_HOME` and work tree, with their timestamps set afterwards so that wall
//! times (and so the golden output) are exact.

use std::path::PathBuf;
use std::process::{Command, Output};

use serde_json::{Value, json};
use tautly_core::record::BlockHashes;
use tautly_core::{FinishReason, ModelId, ProviderEvent, RawUsage, SessionRecord, TestEvidence};
use tautly_session::{BlobRef, HeaderInit, Origin, SessionWriter};
use tempfile::TempDir;

struct Scratch {
    home: TempDir,
    work: TempDir,
}

impl Scratch {
    /// A work tree, and a user configuration that prices `m/a` at $1 / $2 per million
    /// tokens and `m/small` at $0.5 / $1.
    fn new() -> Scratch {
        let s = Scratch {
            home: tempfile::tempdir().unwrap(),
            work: tempfile::tempdir().unwrap(),
        };
        let entry = |input: f64, output: f64| {
            format!(
                "context_window = 100000\nmax_output = 4000\nparallel_tools = true\n\
                 reasoning = \"none\"\ncache = \"none\"\nimages = false\n\
                 edit_format = \"str_replace\"\n\
                 price = {{ input = {input}, output = {output}, cache_read = 0.0, \
                 cache_write = 0.0 }}\n"
            )
        };
        std::fs::write(
            s.home.path().join("config.toml"),
            format!(
                "[catalog_overrides.models.\"m/a\"]\n{}\n[catalog_overrides.models.\"m/small\"]\n{}",
                entry(1.0, 2.0),
                entry(0.5, 1.0)
            ),
        )
        .unwrap();
        s
    }

    fn root(&self) -> PathBuf {
        self.work.path().canonicalize().unwrap()
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

    fn log(&self, init: HeaderInit) -> Log {
        Log {
            w: SessionWriter::create_in(self.home.path(), &self.root(), init).unwrap(),
            times: Vec::new(),
        }
    }

    /// A branch of `parent` copying its records through offset `last`, then `Forked`.
    fn fork(&self, parent: &Done, last: u64, label: &str) -> Log {
        let mut log = self.log(HeaderInit {
            parent: Some((parent.id.clone(), last)),
            label: Some(label.into()),
            origin: Origin::Rewind,
            ..HeaderInit::default()
        });
        let copied = log.w.copy_from(&parent.path, last + 1).unwrap();
        log.times.extend((0..copied).map(|_| None));
        log.at(0, SessionRecord::Forked);
        log
    }
}

struct Log {
    w: SessionWriter<SessionRecord>,
    times: Vec<Option<u64>>,
}

struct Done {
    id: String,
    path: PathBuf,
}

impl Log {
    fn at(&mut self, t: u64, rec: SessionRecord) -> u64 {
        self.times.push(Some(t));
        self.w.append(&rec).unwrap()
    }

    fn blob(&self, bytes: &[u8]) -> BlobRef {
        self.w.blob(bytes).unwrap()
    }

    fn turn(&mut self, t: u64, turn: u32, text: &str) {
        self.at(
            t,
            SessionRecord::TurnStart {
                turn,
                text: text.into(),
            },
        );
    }

    fn end(&mut self, t: u64, evidence: Option<TestEvidence>) -> u64 {
        self.at(
            t,
            SessionRecord::TurnEnd {
                reason: FinishReason::Completed,
                evidence,
                synthetic: false,
            },
        )
    }

    /// A request of `step` on `model` and its response (text, then its usage).
    fn step(&mut self, t: u64, turn: u32, step: u32, model: &str, input: u64, output: u64) {
        let hash = self.blob(format!("request {step}").as_bytes());
        self.at(
            t,
            SessionRecord::Request {
                hash,
                turn,
                step,
                model: ModelId::from(model),
                blocks: BlockHashes {
                    stable: "s".into(),
                    pinned: "p".into(),
                    history: "h".into(),
                    tail: "t".into(),
                },
            },
        );
        self.at(
            t + 5,
            SessionRecord::Provider(ProviderEvent::TextDelta("ok".into())),
        );
        self.at(
            t + 10,
            SessionRecord::Provider(ProviderEvent::Usage(RawUsage {
                input,
                output,
                cache_read: 0,
                cache_write: 0,
            })),
        );
    }

    /// A call `id` of `name` and its result (as the log stores them).
    fn call(&mut self, t: u64, id: &str, name: &str, is_error: bool) {
        self.at(
            t,
            SessionRecord::ToolCall {
                id: id.into(),
                name: name.into(),
                args: json!({}),
            },
        );
        let blob = self.blob(b"result");
        let result = json!({"type": "tool_result", "data": {
            "id": id, "result_blob": blob, "is_error": is_error, "effects": [],
            "meta": {}, "ran": true, "blocked": false,
        }});
        self.at(t, serde_json::from_value(result).unwrap());
    }

    fn edit(&mut self, t: u64, step: u32, path: &str, before: Option<&str>, after: &str) {
        let pre_image = before.map(|b| self.blob(b.as_bytes()));
        let post_image_blob = Some(self.blob(after.as_bytes()));
        self.at(
            t,
            SessionRecord::Edit {
                path: serde_json::from_value(json!(path)).unwrap(),
                pre_image,
                post_image_blob,
                step,
                asserted: false,
            },
        );
    }

    fn done(mut self) -> Done {
        self.w.checkpoint().unwrap();
        let path = self.w.path().to_path_buf();
        let id = self.w.session_id().to_string();
        drop(self.w);
        let text = std::fs::read_to_string(&path).unwrap();
        let mut lines = Vec::new();
        for (i, line) in text.lines().enumerate() {
            if i == 0 {
                lines.push(line.to_string());
                continue;
            }
            let mut v: Value = serde_json::from_str(line).unwrap();
            if let Some(t) = self.times[i - 1] {
                v["t"] = json!(t);
            }
            lines.push(v.to_string());
        }
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        Done { id, path }
    }
}

fn evidence(step: u32, passed: u32, failed: u32) -> TestEvidence {
    TestEvidence {
        command: "cargo test --secret-path /home/x".into(),
        exit_code: Some(i32::from(failed > 0)),
        passed: Some(passed),
        failed: Some(failed),
        step,
        write_gen: 1,
        stale: false,
        edits_since: 0,
        execs_since: 0,
    }
}

/// A parent with one turn (steps 1, 2) and two branches of it, `try-a` (one more turn,
/// tests pass) and `try-b` (two turns: tests fail, then an edit leaves them stale; its
/// label carries an escape sequence).
struct Tree {
    parent: Done,
    a: Done,
    b: Done,
}

fn tree(s: &Scratch) -> Tree {
    let mut p = s.log(HeaderInit::default());
    p.turn(1_000, 1, "fix the bug");
    p.step(1_100, 1, 2, "m/a", 1_000, 100);
    p.edit(1_300, 2, "shared.txt", None, "one\n");
    let last = p.end(3_000, None);
    let parent = p.done();

    let mut a = s.fork(&parent, last, "try-a");
    a.turn(10_000, 2, "try it this way");
    a.step(10_100, 2, 4, "m/a", 2_000, 200);
    a.call(10_200, "c1", "edit", true);
    a.call(10_300, "c2", "bash", false);
    a.edit(10_400, 4, "a.txt", None, "x\ny\n");
    a.edit(10_500, 4, "both.txt", Some("1\n2\n"), "1\nTWO\n");
    a.end(14_200, Some(evidence(4, 12, 0)));
    let a = a.done();

    let mut b = s.fork(&parent, last, "try-b\x1b[31m");
    b.turn(20_000, 2, "or that way");
    b.step(20_100, 2, 4, "m/a", 3_000, 300);
    b.step(20_200, 2, 5, "m/small", 400, 40);
    b.at(
        20_300,
        SessionRecord::Charged {
            role: tautly_core::UsageRole::Side,
            model: ModelId::from("m/small"),
            usage: RawUsage {
                input: 100,
                output: 10,
                cache_read: 0,
                cache_write: 0,
            },
            estimated: true,
        },
    );
    b.edit(20_400, 5, "both.txt", Some("1\n2\n"), "zero\n1\n2\n");
    b.end(25_000, Some(evidence(5, 10, 2)));
    b.turn(30_000, 3, "tidy up");
    b.step(30_100, 3, 7, "m/a", 500, 50);
    b.edit(30_200, 7, "b.txt", None, "b\n");
    b.end(31_000, None);
    let b = b.done();
    Tree { parent, a, b }
}

/// `text` with the session ids replaced by `<p>`, `<a>` and `<b>` (ids made in the same
/// millisecond share their first characters, so only whole ids are replaced; the columns
/// are named by label).
fn redact(text: &str, t: &Tree) -> String {
    let mut out = text.to_string();
    for (id, name) in [(&t.parent.id, "<p>"), (&t.a.id, "<a>"), (&t.b.id, "<b>")] {
        out = out.replace(id.as_str(), name);
    }
    out
}

#[test]
fn compare_two_branches_from_common_fork_point_golden() {
    let s = Scratch::new();
    let t = tree(&s);
    let out = s.ok(&["compare", &t.a.id, &t.b.id[..12]]);
    assert!(!out.contains('\x1b'), "{out:?}");
    insta::assert_snapshot!(redact(&out, &t));
}

#[test]
fn evidence_row_shows_last_test_and_stale() {
    let s = Scratch::new();
    let t = tree(&s);
    let out = s.ok(&["compare", &t.a.id, &t.b.id]);
    let row = out
        .lines()
        .find(|l| l.starts_with("last test"))
        .unwrap_or_else(|| panic!("no last test row in:\n{out}"));
    assert!(
        row.contains("exit 0 · 12 passed, 0 failed · fresh"),
        "{row}"
    );
    assert!(
        row.contains("exit 1 · 10 passed, 2 failed · stale (1 edits, 0 commands since)"),
        "{row}"
    );
    // The command may name paths: it is never shown.
    assert!(!out.contains("secret-path"), "{out}");
}

#[test]
fn more_than_five_sessions_rejected() {
    let s = Scratch::new();
    let t = tree(&s);
    let ids = [&t.parent.id, &t.a.id, &t.b.id];
    let six: Vec<&str> = ids.iter().cycle().take(6).map(|s| s.as_str()).collect();
    let mut args = vec!["compare"];
    args.extend(&six);
    let out = s.run(&args);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains('5') || err.contains("five"), "{err}");
    // One session alone is not a comparison either; the same one twice neither.
    let out = s.run(&["compare", &t.a.id]);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    let out = s.run(&["compare", &t.a.id, &t.a.id[..10]]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("more than once"),
        "{out:?}"
    );
}

#[test]
fn json_shape() {
    let s = Scratch::new();
    let t = tree(&s);
    let out = s.ok(&["compare", &t.a.id, &t.b.id, &t.parent.id, "--json"]);
    let v: Value = serde_json::from_str(&out).unwrap();
    insta::assert_snapshot!(redact(&serde_json::to_string_pretty(&v).unwrap(), &t));
}

#[test]
fn unrelated_sessions_count_their_own_records() {
    // cp5 review C I4: with no shared fork point a branch counts from its own fork record,
    // as resume, tree, stats and receipts do; a root session counts whole.
    let s = Scratch::new();
    let one = tree(&s);
    let two = tree(&s);
    let out = s.ok(&["compare", &one.a.id, &two.b.id, &two.parent.id, "--json"]);
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["fork_point"], Value::Null);
    let a = &v["sessions"][0];
    assert_eq!((&a["user_turns"], &a["steps"]), (&json!(1), &json!(1)));
    assert_eq!(
        (&a["tokens_in"], &a["tokens_out"]),
        (&json!(2_000), &json!(200))
    );
    assert_eq!(a["files"], json!(["a.txt", "both.txt"]));
    assert_eq!(v["sessions"][1]["user_turns"], 2);
    assert_eq!(v["sessions"][2]["user_turns"], 1);
    assert_eq!(v["sessions"][2]["tokens_in"], 1_000);
    let text = s.ok(&["compare", &one.a.id, &two.b.id]);
    assert!(text.contains("no common fork point"), "{text}");
}

#[test]
fn a_payload_this_version_cannot_read_is_noted_not_fatal() {
    // cp5 review C I2: a well-formed line whose payload is no record this version reads
    // is skipped with a note, never a corrupt log.
    let s = Scratch::new();
    let t = tree(&s);
    let text = std::fs::read_to_string(&t.b.path).unwrap();
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    // A record of b's own (after the fork point), not its last line (a cut-off tail).
    let i = lines.len() - 2;
    let mut line: Value = serde_json::from_str(&lines[i]).unwrap();
    line["data"] = json!({"type": "turn_start", "data": {"turn": "bad"}});
    lines[i] = line.to_string();
    std::fs::write(&t.b.path, lines.join("\n") + "\n").unwrap();
    let out = s.ok(&["compare", &t.a.id, &t.b.id]);
    assert!(
        out.contains("1 record written by a newer version was skipped"),
        "{out}"
    );
    // A line that is not JSON still names the session.
    lines[1] = "{not json".into();
    std::fs::write(&t.b.path, lines.join("\n") + "\n").unwrap();
    let out = s.run(&["compare", &t.a.id, &t.b.id]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains(&format!("session {}", t.b.id)), "{err}");
}

#[test]
fn three_sessions_meeting_at_different_ancestors_count_from_the_earliest() {
    let s = Scratch::new();
    let t = tree(&s);
    // c branches from try-a after try-a's own turn: c and try-a meet at try-a's step 4,
    // c and try-b at the parent's step 2.
    let a_last = {
        let r = tautly_session::SessionReader::<SessionRecord>::open(&t.a.path).unwrap();
        r.records().count() as u64 - 1
    };
    let mut c = s.fork(&t.a, a_last, "c");
    c.turn(40_000, 3, "and on");
    c.step(40_100, 3, 6, "m/a", 7, 1);
    c.end(40_500, None);
    let c = c.done();
    let out = s.ok(&["compare", &c.id, &t.b.id, &t.a.id, "--json"]);
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["fork_point"]["session_id"], json!(t.parent.id));
    assert_eq!(v["fork_point"]["step"], 2);
    // From step 2, c holds try-a's turn and its own.
    assert_eq!(v["sessions"][0]["user_turns"], 2);
    assert_eq!(v["sessions"][0]["tokens_in"], 2_007);
    assert_eq!(v["sessions"][1]["user_turns"], 2);
    assert_eq!(v["sessions"][2]["user_turns"], 1);
}

#[test]
fn sessions_sharing_a_label_are_told_apart() {
    let s = Scratch::new();
    let t = tree(&s);
    let mut x = s.fork(&t.parent, 4, "try-a");
    x.turn(50_000, 2, "same name");
    x.end(50_100, None);
    let x = x.done();
    let out = s.ok(&["compare", &t.a.id, &x.id]);
    let header = out.lines().next().unwrap();
    assert_eq!(header.matches("try-a (").count(), 2, "{out}");
}

#[test]
fn the_output_says_diffs_are_tool_edits_only_and_notes_commands() {
    let s = Scratch::new();
    let t = tree(&s);
    let mut x = s.fork(&t.parent, 4, "cmds");
    x.turn(50_000, 2, "run it");
    let blob = x.blob(b"ran");
    let result = json!({"type": "tool_result", "data": {
        "id": "x1", "result_blob": blob, "is_error": false,
        "effects": [{"type": "exec", "may_write": true}],
        "meta": {}, "ran": true, "blocked": false,
    }});
    x.at(50_050, serde_json::from_value(result).unwrap());
    x.end(50_100, None);
    let x = x.done();
    let out = s.ok(&["compare", &t.a.id, &x.id]);
    assert!(out.contains("tool edits only"), "{out}");
    assert!(
        out.contains("cmds: 1 command that may have changed files"),
        "{out}"
    );
}
