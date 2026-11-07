//! Compare (spec 7.12.6, Task 52): branch figures from session logs alone, counted from
//! the branches' fork point. Every log here is written record by record, with its
//! timestamps set afterwards, so wall times are exact.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use tautly_core::compare::{BranchSummary, fork_cut, fork_point, summarize, summarize_at};
use tautly_core::record::BlockHashes;
use tautly_core::{FinishReason, Ledger, SessionRecord, TestEvidence, UsageRole};
use tautly_provider::{Catalog, ModelId, ProviderEvent, RawUsage};
use tautly_session::{BlobRef, HeaderInit, RelPath, SessionReader, SessionWriter};
use tautly_tools::{PersistedEffect, ToolMeta};
use tempfile::TempDir;

// ------------------------------------------------------------------------ fixtures

struct Home {
    home: TempDir,
    work: TempDir,
}

impl Home {
    fn new() -> Home {
        Home {
            home: tempfile::tempdir().unwrap(),
            work: tempfile::tempdir().unwrap(),
        }
    }

    fn root(&self) -> PathBuf {
        self.work.path().canonicalize().unwrap()
    }

    fn data(&self) -> &Path {
        self.home.path()
    }

    fn log(&self, init: HeaderInit) -> Log {
        Log {
            w: SessionWriter::create_in(self.data(), &self.root(), init).unwrap(),
            times: Vec::new(),
            raw: Vec::new(),
        }
    }

    /// A branch of `parent` that copies its records through offset `last` (as a fork or
    /// rewind does: the header names that offset), then `Forked`.
    fn fork(&self, parent: &Done, last: u64, label: &str) -> Log {
        let mut log = self.log(HeaderInit {
            parent: Some((parent.id.clone(), last)),
            label: Some(label.into()),
            origin: tautly_session::Origin::Fork,
            ..HeaderInit::default()
        });
        let copied = log.w.copy_from(&parent.path, last + 1).unwrap();
        log.times.extend((0..copied).map(|_| None));
        log.at(0, SessionRecord::Forked);
        log
    }
}

/// A log being written: each appended record's `t` is set when it is finished.
struct Log {
    w: SessionWriter<SessionRecord>,
    /// Per record: the `t` to set (none: keep the copied one).
    times: Vec<Option<u64>>,
    /// Raw record payloads to append after the typed ones (records this version does not
    /// know).
    raw: Vec<(u64, Value)>,
}

struct Done {
    id: String,
    path: PathBuf,
}

impl Done {
    fn reader(&self) -> SessionReader<SessionRecord> {
        SessionReader::open(&self.path).unwrap()
    }
}

impl Log {
    /// Append `rec` at time `t` (ms); its offset.
    fn at(&mut self, t: u64, rec: SessionRecord) -> u64 {
        self.times.push(Some(t));
        self.w.append(&rec).unwrap()
    }

    fn blob(&self, bytes: &[u8]) -> BlobRef {
        self.w.blob(bytes).unwrap()
    }

    fn request(&mut self, t: u64, turn: u32, step: u32, model: &str) -> u64 {
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
        )
    }

    /// A response of text and its usage (an answered step).
    fn usage(&mut self, t: u64, input: u64, output: u64) -> u64 {
        self.at(
            t,
            SessionRecord::Provider(ProviderEvent::TextDelta("ok".into())),
        );
        self.at(
            t,
            SessionRecord::Provider(ProviderEvent::Usage(usage(input, output))),
        )
    }

    fn turn(&mut self, t: u64, turn: u32, text: &str) -> u64 {
        self.at(
            t,
            SessionRecord::TurnStart {
                turn,
                text: text.into(),
            },
        )
    }

    fn end(&mut self, t: u64, evidence: Option<TestEvidence>) -> u64 {
        self.at(
            t,
            SessionRecord::TurnEnd {
                reason: FinishReason::Completed,
                evidence,
            },
        )
    }

    /// A call `id` of tool `name` and its result.
    fn call(&mut self, t: u64, id: &str, name: &str, is_error: bool, blocked: bool) {
        self.at(
            t,
            SessionRecord::ToolCall {
                id: id.into(),
                name: name.into(),
                args: json!({}),
            },
        );
        let result_blob = self.blob(b"result");
        self.at(
            t,
            SessionRecord::ToolResult {
                id: id.into(),
                result_blob,
                is_error,
                effects: Vec::new(),
                meta: ToolMeta::default(),
                ran: !blocked,
                blocked,
            },
        );
    }

    /// A tool's edit of `path` at `step` from `before` to `after` (none: absent).
    fn edit(&mut self, t: u64, step: u32, path: &str, before: Option<&str>, after: Option<&str>) {
        let pre_image = before.map(|b| self.blob(b.as_bytes()));
        let post_image_blob = after.map(|a| self.blob(a.as_bytes()));
        self.at(
            t,
            SessionRecord::Edit {
                path: rel(path),
                pre_image,
                post_image_blob,
                step,
                asserted: false,
            },
        );
    }

    /// A `write` call's deletion of `path` (content `before`) at `step`: its result, then
    /// the `Edit` with no post-image.
    fn delete(&mut self, t: u64, step: u32, id: &str, path: &str, before: &str) {
        let result_blob = self.blob(b"deleted");
        self.at(
            t,
            SessionRecord::ToolResult {
                id: id.into(),
                result_blob,
                is_error: false,
                effects: vec![PersistedEffect::Deleted {
                    path: rel(path),
                    pre_image_blob: "00".into(),
                    pre_mode: 0o644,
                }],
                meta: ToolMeta::default(),
                ran: true,
                blocked: false,
            },
        );
        self.edit(t, step, path, Some(before), None);
    }

    /// A `bash` call `id` that may have written files.
    fn exec(&mut self, t: u64, id: &str) {
        let result_blob = self.blob(b"ran");
        self.at(
            t,
            SessionRecord::ToolResult {
                id: id.into(),
                result_blob,
                is_error: false,
                effects: vec![PersistedEffect::Exec { may_write: true }],
                meta: ToolMeta::default(),
                ran: true,
                blocked: false,
            },
        );
    }

    /// Append a record this version does not know, at time `t`.
    fn unknown(&mut self, t: u64) {
        self.raw
            .push((t, json!({"type": "from_the_future", "data": {"x": 1}})));
    }

    /// Flush, then set every appended record's `t`.
    fn done(mut self) -> Done {
        self.w.checkpoint().unwrap();
        let path = self.w.path().to_path_buf();
        let id = self.w.session_id().to_string();
        drop(self.w);
        let text = std::fs::read_to_string(&path).unwrap();
        let mut lines: Vec<String> = Vec::new();
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
        for (t, data) in self.raw {
            let offset = lines.len() - 1;
            lines.push(
                json!({"type": "record", "offset": offset, "t": t, "data": data}).to_string(),
            );
        }
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        Done { id, path }
    }
}

fn usage(input: u64, output: u64) -> RawUsage {
    RawUsage {
        input,
        output,
        cache_read: 0,
        cache_write: 0,
    }
}

fn rel(path: &str) -> RelPath {
    serde_json::from_value(json!(path)).unwrap()
}

/// `m/a` at $1 / $2 per million tokens, `m/small` at $0.5 / $1; `m/free` has no price.
fn catalog() -> Catalog {
    let entry = |input: f64, output: f64| {
        format!(
            "context_window = 100000\nmax_output = 4000\nparallel_tools = true\n\
             reasoning = \"none\"\ncache = \"none\"\nimages = false\n\
             edit_format = \"str_replace\"\n\
             price = {{ input = {input}, output = {output}, cache_read = 0.0, cache_write = 0.0 }}\n"
        )
    };
    Catalog::from_toml(&format!(
        "[models.\"m/a\"]\n{}\n[models.\"m/small\"]\n{}",
        entry(1.0, 2.0),
        entry(0.5, 1.0)
    ))
    .unwrap()
}

fn sum(reader: &SessionReader<SessionRecord>, from: Option<u32>) -> BranchSummary {
    summarize(reader, None, &catalog(), from).unwrap()
}

/// A parent with one user turn (steps 1 and 2, a tool call, 2 s of wall time).
fn parent(h: &Home) -> (Done, u64) {
    let mut p = h.log(HeaderInit::default());
    p.turn(1_000, 1, "start");
    p.request(1_100, 1, 2, "m/a");
    p.usage(1_200, 100, 10);
    p.call(1_300, "c1", "read", false, false);
    p.edit(1_400, 2, "shared.txt", None, Some("one\n"));
    let last = p.end(3_000, None);
    (p.done(), last)
}

// ------------------------------------------------------------------------ tests

#[test]
fn figures_count_from_fork_point_not_shared_prefix() {
    let h = Home::new();
    let (p, last) = parent(&h);

    // Branch a: one more turn of one step (sent twice: a retry repeats the step).
    let mut a = h.fork(&p, last, "a");
    a.turn(10_000, 2, "try a");
    a.request(10_100, 2, 4, "m/a");
    a.at(10_150, SessionRecord::ReasoningStripped);
    a.request(10_200, 2, 4, "m/a");
    a.usage(10_300, 200, 20);
    a.call(10_350, "c2", "edit", true, false);
    a.call(10_360, "c3", "bash", false, true);
    let a_end = a.end(10_500, None);
    // A message logged on arrival, before its turn starts, never counts as wall time.
    a.at(
        10_600,
        SessionRecord::Command(tautly_core::Command::SendMessage {
            text: "queued".into(),
        }),
    );
    let _ = a_end;
    let a = a.done();

    // Branch b: two steps.
    let mut b = h.fork(&p, last, "b");
    b.turn(20_000, 2, "try b");
    b.request(20_100, 2, 4, "m/a");
    b.usage(20_200, 300, 30);
    b.request(20_300, 2, 5, "m/small");
    b.usage(20_400, 400, 40);
    b.end(21_000, None);
    let b = b.done();

    let (ra, rb) = (a.reader(), b.reader());
    let fp = fork_point(ra.header(), rb.header(), h.data());
    assert_eq!(fp, Some((p.id.clone(), 2)));
    // A parent and its own branch meet at the branch's fork point.
    assert_eq!(
        fork_point(p.reader().header(), ra.header(), h.data()),
        Some((p.id.clone(), 2))
    );

    let sa = sum(&ra, Some(2));
    assert_eq!(sa.session_id, a.id);
    assert_eq!(sa.label.as_deref(), Some("a"));
    assert_eq!(sa.user_turns, 1);
    assert_eq!(sa.steps, 1, "a repeated step counts once");
    assert_eq!((sa.tokens_in, sa.tokens_out), (200, 20));
    assert_eq!(sa.wall_ms, 500);
    assert_eq!((sa.tool_calls, sa.blocked, sa.edit_failures), (1, 1, 1));
    assert!(sa.files.is_empty(), "the parent's edit is not the branch's");
    assert_eq!(sa.models, vec![ModelId::from("m/a")]);
    let cost = sa.cost_usd.unwrap();
    assert!((cost - (200.0 + 40.0) / 1e6).abs() < 1e-12, "{cost}");

    let sb = sum(&rb, Some(2));
    assert_eq!((sb.user_turns, sb.steps), (1, 2));
    assert_eq!((sb.tokens_in, sb.tokens_out), (700, 70));
    assert_eq!(sb.wall_ms, 1_000);
    assert_eq!(
        sb.models,
        vec![ModelId::from("m/a"), ModelId::from("m/small")]
    );

    // The whole branch includes the copied prefix.
    let whole = sum(&ra, None);
    assert_eq!((whole.user_turns, whole.steps), (2, 2));
    assert_eq!(whole.wall_ms, 2_500);
    assert_eq!(whole.tokens_in, 300);
    assert!(whole.files.contains("shared.txt"));

    // The parent counted from the same point has nothing of its own.
    let sp = sum(&p.reader(), Some(2));
    assert_eq!(
        (sp.user_turns, sp.steps, sp.wall_ms, sp.tokens_in),
        (0, 0, 0, 0)
    );

    // The parent's log may be gone: the branches still name it and their own logs hold
    // the copied prefix.
    std::fs::remove_file(&p.path).unwrap();
    assert_eq!(
        fork_point(ra.header(), rb.header(), h.data()),
        Some((p.id.clone(), 2))
    );
}

#[test]
fn a_branch_of_a_branch_meets_its_cousin_at_the_common_ancestor() {
    let h = Home::new();
    let (p, last) = parent(&h);
    let mut a = h.fork(&p, last, "a");
    a.turn(10_000, 2, "a");
    a.request(10_100, 2, 4, "m/a");
    a.usage(10_200, 50, 5);
    let a_last = a.end(10_300, None);
    let a = a.done();
    // c branches from a after a's own turn.
    let mut c = h.fork(&a, a_last, "c");
    c.turn(11_000, 3, "c");
    c.request(11_100, 3, 6, "m/a");
    c.usage(11_200, 70, 7);
    c.end(11_400, None);
    let c = c.done();
    let mut b = h.fork(&p, last, "b");
    b.turn(20_000, 2, "b");
    b.end(20_100, None);
    let b = b.done();

    let (rc, rb) = (c.reader(), b.reader());
    assert_eq!(
        fork_point(rc.header(), rb.header(), h.data()),
        Some((p.id.clone(), 2))
    );
    assert_eq!(
        fork_point(rc.header(), a.reader().header(), h.data()),
        Some((a.id.clone(), 4))
    );
    // From p's step 2, c counts a's turn as well as its own.
    let sc = sum(&rc, Some(2));
    assert_eq!((sc.user_turns, sc.steps, sc.tokens_in), (2, 2, 120));
    assert_eq!(sc.wall_ms, 300 + 400);
    // From a's step 4, only its own.
    let sc = sum(&rc, Some(4));
    assert_eq!((sc.user_turns, sc.steps, sc.tokens_in), (1, 1, 70));
}

#[test]
fn files_overlap_sets() {
    let h = Home::new();
    let (p, last) = parent(&h);
    let mut a = h.fork(&p, last, "a");
    a.turn(10_000, 2, "a");
    a.request(10_100, 2, 4, "m/a");
    a.edit(10_200, 4, "a.txt", None, Some("x\ny\n"));
    a.edit(10_300, 4, "b.txt", Some("1\n2\n"), Some("1\nTWO\n"));
    a.edit(10_400, 4, "b.txt", Some("1\nTWO\n"), Some("1\nthree\n"));
    // Edited and put back: touched, but no net change.
    a.edit(10_500, 4, "same.txt", Some("s\n"), Some("t\n"));
    a.edit(10_600, 4, "same.txt", Some("t\n"), Some("s\n"));
    a.end(11_000, None);
    let a = a.done();
    let mut b = h.fork(&p, last, "b");
    b.turn(20_000, 2, "b");
    b.request(20_100, 2, 4, "m/a");
    b.delete(20_200, 4, "d1", "b.txt", "1\n2\n");
    b.edit(20_300, 4, "c.txt", None, Some("c\n"));
    b.end(21_000, None);
    let b = b.done();

    let sa = sum(&a.reader(), Some(2));
    let sb = sum(&b.reader(), Some(2));
    let names = |s: &BranchSummary| s.files.iter().cloned().collect::<Vec<_>>();
    assert_eq!(names(&sa), ["a.txt", "b.txt", "same.txt"]);
    assert_eq!(names(&sb), ["b.txt", "c.txt"]);
    // a.txt +2; b.txt "2" -> "three" +1 -1; same.txt nothing.
    assert_eq!((sa.added, sa.removed), (3, 1));
    // b.txt deleted (-2), c.txt +1.
    assert_eq!((sb.added, sb.removed), (1, 2));

    let table = tautly_core::compare::files_table(&[sa, sb]);
    assert_eq!(
        table,
        vec![
            ("a.txt".to_string(), vec![0]),
            ("b.txt".to_string(), vec![0, 1]),
            ("c.txt".to_string(), vec![1]),
            ("same.txt".to_string(), vec![0]),
        ]
    );
}

#[test]
fn evidence_is_the_last_test_run_and_goes_stale_after_later_edits() {
    let h = Home::new();
    let mut s = h.log(HeaderInit::default());
    let ev = TestEvidence {
        command: "cargo test".into(),
        exit_code: Some(0),
        passed: Some(3),
        failed: Some(0),
        step: 2,
        write_gen: 1,
        stale: false,
        edits_since: 0,
        execs_since: 0,
    };
    s.turn(1_000, 1, "one");
    s.request(1_100, 1, 2, "m/a");
    s.end(1_200, Some(ev.clone()));
    let done = s;
    let one = {
        let mut s = done;
        s.turn(2_000, 2, "two");
        s.request(2_100, 2, 4, "m/a");
        s.end(2_200, None);
        s
    }
    .done();
    let got = sum(&one.reader(), None).evidence.unwrap();
    assert_eq!(
        got, ev,
        "a later turn without a test keeps the last run, fresh"
    );

    let mut s = h.log(HeaderInit::default());
    s.turn(1_000, 1, "one");
    s.request(1_100, 1, 2, "m/a");
    s.end(1_200, Some(ev.clone()));
    s.turn(2_000, 2, "two");
    s.request(2_100, 2, 4, "m/a");
    s.edit(2_150, 4, "a.txt", None, Some("a\n"));
    s.end(2_200, None);
    let two = s.done();
    let got = sum(&two.reader(), None).evidence.unwrap();
    assert!(got.stale);
    assert_eq!(got.edits_since, 1);
}

#[test]
fn cost_from_ledger_includes_discarded_attempts_else_logged_usage() {
    let h = Home::new();
    let mut s = h.log(HeaderInit::default());
    s.turn(1_000, 1, "go");
    s.request(1_100, 1, 2, "m/a");
    s.at(
        1_150,
        SessionRecord::Provider(ProviderEvent::DiscardedUsage(usage(500, 0))),
    );
    // Usage is cumulative within a response: the last one counts.
    s.usage(1_200, 400, 50);
    s.usage(1_300, 1_000, 100);
    s.at(
        1_400,
        SessionRecord::Charged {
            role: UsageRole::Side,
            model: ModelId::from("m/small"),
            usage: usage(200, 20),
            estimated: true,
        },
    );
    s.end(2_000, None);
    let s = s.done();
    let reader = s.reader();

    // No ledger: the logged usage, each charge at its own model's price.
    let logged = summarize(&reader, None, &catalog(), None).unwrap();
    let want = (1_000.0 + 200.0 + 500.0 + 100.0 + 20.0) / 1e6;
    assert!(
        (logged.cost_usd.unwrap() - want).abs() < 1e-12,
        "{logged:?}"
    );
    assert!(logged.cost_estimated);
    assert_eq!((logged.tokens_in, logged.tokens_out), (1_700, 120));
    assert!(logged.models.contains(&ModelId::from("m/small")));

    // A ledger without this session's rows: still the log.
    let ledger = Ledger::open(&h.data().join("ledger.sqlite")).unwrap();
    let empty = summarize(&reader, Some(&ledger), &catalog(), None).unwrap();
    assert_eq!(empty.cost_usd, logged.cost_usd);

    // The ledger, discarded attempts included.
    ledger
        .record(&s.id, &ModelId::from("m/a"), &usage(1_000, 100), Some(0.05))
        .unwrap();
    ledger
        .record(&s.id, &ModelId::from("m/a"), &usage(900, 0), Some(0.01))
        .unwrap();
    ledger
        .record_estimated(&s.id, &ModelId::from("m/small"), &usage(200, 20), None)
        .unwrap();
    let led = summarize(&reader, Some(&ledger), &catalog(), None).unwrap();
    assert!((led.cost_usd.unwrap() - 0.06).abs() < 1e-12, "{led:?}");
    assert_eq!((led.tokens_in, led.tokens_out), (2_100, 120));
    assert!(led.cost_estimated);
    assert!(
        led.cost_partial,
        "an unpriced row leaves the amount partial"
    );

    // A model the catalog has no price for: the priced part, marked partial.
    let mut u = h.log(HeaderInit::default());
    u.turn(1_000, 1, "go");
    u.request(1_100, 1, 2, "m/free");
    u.usage(1_200, 10, 1);
    u.end(1_300, None);
    let u = u.done();
    let free = sum(&u.reader(), None);
    assert_eq!(free.cost_usd, None);
    assert!(free.cost_partial);
}

#[test]
fn ledger_is_not_used_for_a_range_that_is_not_the_sessions_own() {
    let h = Home::new();
    let (p, _) = parent(&h);
    let ledger = Ledger::open(&h.data().join("ledger.sqlite")).unwrap();
    ledger
        .record(&p.id, &ModelId::from("m/a"), &usage(100, 10), Some(9.0))
        .unwrap();
    let reader = p.reader();
    let whole = summarize(&reader, Some(&ledger), &catalog(), None).unwrap();
    assert_eq!(whole.cost_usd, Some(9.0));
    // From step 2 on the parent spent nothing; its ledger rows are all before.
    let after = summarize(&reader, Some(&ledger), &catalog(), Some(2)).unwrap();
    assert_eq!(after.cost_usd, Some(0.0));
}

#[test]
fn unrelated_sessions_compared_whole() {
    let h = Home::new();
    let (p, _) = parent(&h);
    let (q, _) = parent(&h);
    assert_eq!(
        fork_point(p.reader().header(), q.reader().header(), h.data()),
        None
    );
    // A header naming a parent that is no session id at all leads nowhere.
    let mut x = h.log(HeaderInit {
        parent: Some(("../../etc/passwd".into(), 0)),
        ..HeaderInit::default()
    });
    x.turn(1_000, 1, "x");
    let x = x.done();
    assert_eq!(
        fork_point(x.reader().header(), p.reader().header(), h.data()),
        None
    );
    let sp = sum(&p.reader(), None);
    assert_eq!((sp.user_turns, sp.steps, sp.wall_ms), (1, 1, 2_000));
}

#[test]
fn untrusted_paths_are_left_out_and_unknown_records_make_one_note() {
    let h = Home::new();
    let mut s = h.log(HeaderInit::default());
    s.turn(1_000, 1, "go");
    s.request(1_100, 1, 2, "m/a");
    for bad in ["../escape.txt", "/etc/passwd", "a\0b", "x/../../y"] {
        s.edit(1_200, 2, bad, None, Some("evil\n"));
    }
    s.edit(1_300, 2, "ok.txt", None, Some("fine\n"));
    s.end(1_400, None);
    s.unknown(1_500);
    s.unknown(1_600);
    let s = s.done();
    let got = sum(&s.reader(), None);
    assert_eq!(got.files.iter().collect::<Vec<_>>(), ["ok.txt"]);
    assert_eq!((got.added, got.removed), (1, 0));
    let unknown: Vec<&String> = got.notes.iter().filter(|n| n.contains("newer")).collect();
    assert_eq!(unknown.len(), 1, "{:?}", got.notes);
    assert!(unknown[0].contains('2'), "{unknown:?}");
    assert!(
        got.notes.iter().any(|n| n.contains("4 paths")),
        "{:?}",
        got.notes
    );
}

// ------------------------------------------------------------- fix round 1

fn at(reader: &SessionReader<SessionRecord>, cut: Option<u64>) -> BranchSummary {
    summarize_at(reader, None, &catalog(), cut).unwrap()
}

/// Three turns of one step each: steps (1, 2), (3, 4), (5, 6); the offsets of their
/// `TurnEnd`s.
fn three_turns(h: &Home) -> (Done, [u64; 3]) {
    let mut a = h.log(HeaderInit::default());
    let mut ends = [0; 3];
    for (i, end) in ends.iter_mut().enumerate() {
        let n = i as u32;
        let t = 1_000 * (u64::from(n) + 1);
        a.turn(t, n + 1, "go");
        a.request(t + 100, n + 1, 2 * n + 2, "m/a");
        a.usage(t + 200, 10, 1);
        *end = a.end(t + 500, None);
    }
    (a.done(), ends)
}

#[test]
fn a_rewind_into_the_copied_prefix_meets_at_its_own_cut() {
    let h = Home::new();
    let (a, ends) = three_turns(&h);
    // B: forked from A at its tip (step 6). C: B rewound to step 2, inside the prefix B
    // copied from A. D: forked from A at step 4.
    let mut b = h.fork(&a, ends[2], "b");
    b.turn(50_000, 4, "b");
    b.end(50_100, None);
    let b = b.done();
    let mut c = h.fork(&b, ends[0], "c");
    c.turn(60_000, 2, "c");
    c.request(60_100, 2, 4, "m/a");
    c.usage(60_200, 999, 9);
    c.end(61_000, None);
    let c = c.done();
    let mut d = h.fork(&a, ends[1], "d");
    d.turn(70_000, 3, "d");
    d.end(70_100, None);
    let d = d.done();
    let (ra, rb, rc, rd) = (a.reader(), b.reader(), c.reader(), d.reader());

    assert_eq!(
        fork_point(rc.header(), ra.header(), h.data()),
        Some((a.id.clone(), 2))
    );
    assert_eq!(
        fork_point(rc.header(), rd.header(), h.data()),
        Some((a.id.clone(), 2))
    );
    assert_eq!(
        fork_point(rc.header(), rb.header(), h.data()),
        Some((b.id.clone(), 2))
    );
    let cut = fork_cut(rc.header(), rd.header(), h.data()).unwrap();
    assert_eq!(cut.offset, ends[0]);
    let sc = at(&rc, Some(cut.offset));
    assert_eq!((sc.user_turns, sc.steps, sc.tokens_in), (1, 1, 999));
    // D from step 2: A's second turn (copied) and its own.
    let sd = at(&rd, Some(cut.offset));
    assert_eq!(
        (sd.user_turns, sd.steps, sd.tokens_in),
        (2, 1, 10),
        "{sd:?}"
    );
}

#[test]
fn a_mid_turn_fork_counts_the_rest_of_the_parents_turn() {
    let h = Home::new();
    let mut p = h.log(HeaderInit::default());
    p.turn(1_000, 1, "go");
    p.request(1_100, 1, 2, "m/a");
    let mid = p.usage(1_200, 100, 10);
    p.request(5_000, 1, 3, "m/a");
    p.usage(5_100, 500, 50);
    p.end(8_800, None);
    let p = p.done();
    // E branches after step 2, mid-turn: its copy is closed with a cancelled turn end,
    // written with `Forked` (both at 0 here).
    let mut e = h.fork(&p, mid, "e");
    e.at(
        0,
        SessionRecord::TurnEnd {
            reason: FinishReason::Cancelled,
            evidence: None,
        },
    );
    e.turn(30_000, 2, "again");
    e.request(30_100, 2, 4, "m/a");
    e.usage(30_200, 70, 7);
    e.end(31_000, None);
    let e = e.done();
    let (rp, re) = (p.reader(), e.reader());
    let cut = fork_cut(rp.header(), re.header(), h.data()).unwrap();
    assert_eq!(
        (cut.session_id.as_str(), cut.step, cut.offset),
        (p.id.as_str(), 2, mid)
    );
    let sp = at(&rp, Some(cut.offset));
    assert_eq!((sp.user_turns, sp.steps, sp.tokens_in), (0, 1, 500));
    assert_eq!(
        sp.wall_ms,
        8_800 - 5_000,
        "the open turn runs from the cut on"
    );
    let se = at(&re, Some(cut.offset));
    assert_eq!((se.user_turns, se.steps, se.tokens_in), (1, 1, 70));
    // The copy's close runs from `Forked` to its turn end (no time), then its own turn.
    assert_eq!(se.wall_ms, 1_000);
}

#[test]
fn a_branchs_ledger_rows_count_from_its_own_fork_and_the_parents_do_not() {
    let h = Home::new();
    let (p, last) = parent(&h);
    let mut b = h.fork(&p, last, "b");
    b.turn(10_000, 2, "b");
    b.request(10_100, 2, 4, "m/a");
    b.usage(10_200, 10, 1);
    b.end(10_300, None);
    let b = b.done();
    let ledger = Ledger::open(&h.data().join("ledger.sqlite")).unwrap();
    for (id, usd) in [(&p.id, 5.0), (&b.id, 0.07)] {
        ledger
            .record(id, &ModelId::from("m/a"), &usage(10, 1), Some(usd))
            .unwrap();
    }
    let cut = fork_cut(p.reader().header(), b.reader().header(), h.data()).unwrap();
    assert_eq!(cut.offset, last);
    let sb = summarize_at(&b.reader(), Some(&ledger), &catalog(), Some(cut.offset)).unwrap();
    assert_eq!(sb.cost_usd, Some(0.07));
    let sp = summarize_at(&p.reader(), Some(&ledger), &catalog(), Some(cut.offset)).unwrap();
    assert_eq!(
        sp.cost_usd,
        Some(0.0),
        "the parent's rows are before the cut"
    );
}

#[test]
fn an_undo_is_in_the_diff_stat_and_makes_the_last_test_stale() {
    let h = Home::new();
    let mut s = h.log(HeaderInit::default());
    let ev = TestEvidence {
        command: "cargo test".into(),
        exit_code: Some(0),
        passed: Some(1),
        failed: Some(0),
        step: 2,
        write_gen: 1,
        stale: false,
        edits_since: 0,
        execs_since: 0,
    };
    s.turn(1_000, 1, "one");
    s.request(1_100, 1, 2, "m/a");
    s.edit(1_150, 2, "u.txt", None, Some("a\nb\n"));
    let end = s.end(1_200, Some(ev));
    let from_blob = Some(s.blob(b"a\nb\n"));
    s.at(
        2_000,
        SessionRecord::Undone {
            turn: 1,
            files: vec![tautly_core::record::UndoneFile {
                path: rel("u.txt"),
                from_blob,
                from_mode: Some(0o644),
                to: None,
                to_mode: None,
            }],
            backup_ref: None,
        },
    );
    let s = s.done();
    let got = sum(&s.reader(), None);
    assert!(got.files.contains("u.txt"));
    // Created, then undone: no net change.
    assert_eq!((got.added, got.removed), (0, 0));
    assert!(
        got.evidence.unwrap().stale,
        "an undo changes files after the test"
    );

    // From the turn's end on, only the undo counts: the file goes away.
    let only = at(&s.reader(), Some(end));
    assert_eq!((only.added, only.removed), (0, 2), "{only:?}");
}

#[test]
fn write_failures_count_and_commands_after_the_cut_are_noted() {
    let h = Home::new();
    let mut s = h.log(HeaderInit::default());
    s.turn(1_000, 1, "go");
    s.request(1_100, 1, 2, "m/a");
    s.call(1_200, "w1", "write", true, false);
    s.call(1_300, "e1", "edit", true, false);
    s.exec(1_400, "x1");
    s.exec(1_500, "x2");
    s.end(1_600, None);
    let s = s.done();
    let got = sum(&s.reader(), None);
    assert_eq!(got.edit_failures, 2);
    assert!(
        got.notes
            .iter()
            .any(|n| n.contains("2 commands") && n.contains("not counted")),
        "{:?}",
        got.notes
    );
}

#[test]
fn a_hostile_step_number_does_not_panic() {
    let h = Home::new();
    let mut s = h.log(HeaderInit::default());
    s.turn(1_000, 1, "go");
    s.request(1_100, 1, u32::MAX, "m/a");
    s.turn(1_200, 2, "again");
    s.end(1_300, None);
    let s = s.done();
    let got = sum(&s.reader(), Some(1));
    assert_eq!(got.user_turns, 1);
}

#[test]
fn an_unreadable_ancestor_log_falls_back_to_the_branch_copy() {
    let h = Home::new();
    let (a, ends) = three_turns(&h);
    let b = h.fork(&a, ends[1], "b").done();
    let d = h.fork(&a, ends[1], "d").done();
    // A's first record no longer reads.
    let text = std::fs::read_to_string(&a.path).unwrap();
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    lines[1] = json!({"type": "record", "offset": 0, "t": 1,
        "data": {"type": "turn_start", "data": {"turn": "bad"}}})
    .to_string();
    std::fs::write(&a.path, lines.join("\n") + "\n").unwrap();
    assert_eq!(
        fork_point(b.reader().header(), d.reader().header(), h.data()),
        Some((a.id.clone(), 4))
    );
}
