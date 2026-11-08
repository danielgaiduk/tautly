//! The time-travel walk and step numbering on generated record sequences: each input
//! byte picks the next record (step and turn numbers from the following bytes, so they
//! can be anything, including `u32::MAX`). Invariants:
//! - `state_at`, `step_at_offset` and `rebuild` never panic (no overflow);
//! - `step_at_offset` never goes down as the offset grows, and at the end of the log it is
//!   the last step `state_at` knows: `state_at(last)` is no `NoSuchStep`, `state_at(last
//!   + 1)` is;
//! - `rebuild` knows the same steps: `rebuild(Some(n))` is `NoSuchStep` exactly for
//!   `n > last`, when the log numbers its steps the way the engine writes them.
#![no_main]

use std::io::Write;

use libfuzzer_sys::fuzz_target;
use tautly_core::record::{BlockHashes, CheckpointPoint};
use tautly_core::resume::{ResumeError, rebuild};
use tautly_core::timetravel::{state_at, step_at_offset};
use tautly_core::{FinishReason, SessionRecord, TtError};
use tautly_provider::{ModelId, ProviderEvent, StopReason};
use tautly_session::{BlobRef, RelPath, SessionReader};

const HEADER: &str = r#"{"type":"header","schema_version":1,"session_id":"fuzz","created":{"secs_since_epoch":0,"nanos_since_epoch":0},"root":"/fuzz/root","parent":null}"#;

struct Bytes<'a>(&'a [u8]);

impl Bytes<'_> {
    fn byte(&mut self) -> Option<u8> {
        let (&b, rest) = self.0.split_first()?;
        self.0 = rest;
        Some(b)
    }

    /// A small number usually, an extreme one sometimes.
    fn num(&mut self, near: u32) -> u32 {
        match self.byte().unwrap_or(0) {
            255 => u32::MAX,
            254 => 0,
            b if b >= 128 => u32::from(b - 128),
            _ => near,
        }
    }
}

fn blob() -> BlobRef {
    BlobRef::parse(&"0".repeat(64)).unwrap()
}

fn rel(s: &str) -> RelPath {
    serde_json::from_value(serde_json::Value::String(s.into())).unwrap()
}

/// The numbers the engine writes: a user message takes the next one, and so does a
/// request, which keeps it only once its response puts something into the history
/// (text or a tool call); unanswered, the next step shares it.
#[derive(Default)]
struct Numbering {
    last: u32,
    /// The open request's step, and whether its response has content yet.
    open: Option<(u32, bool)>,
}

impl Numbering {
    fn close(&mut self) {
        if let Some((s, true)) = self.open.take() {
            self.last = s;
        }
    }
}

/// The records, and whether every step number is the one the engine would write.
fn generate(data: &[u8]) -> (Vec<SessionRecord>, bool) {
    let mut b = Bytes(data);
    let mut out = Vec::new();
    let (mut turn, mut step) = (0u32, 0u32);
    let mut n = Numbering::default();
    let mut honest = true;
    while let Some(op) = b.byte() {
        let kind = op % 14;
        match kind {
            0 | 9 | 11 | 13 => n.close(),
            12 if op / 14 % 2 == 0 => n.close(), // a late lost write
            2 | 3 => {
                if let Some((_, answered)) = &mut n.open {
                    *answered = true;
                }
            }
            _ => {}
        }
        let rec = match kind {
            0 => {
                turn = b.num(turn.saturating_add(1));
                n.last = n.last.saturating_add(1);
                step = n.last;
                SessionRecord::TurnStart {
                    turn,
                    text: "hi".into(),
                }
            }
            1 => {
                n.close();
                let want = n.last.saturating_add(1);
                let s = b.num(want);
                honest &= s == want;
                n.open = Some((s, false));
                step = s;
                SessionRecord::Request {
                    hash: blob(),
                    turn,
                    step: s,
                    model: ModelId::from("mock/test"),
                    blocks: BlockHashes {
                        stable: String::new(),
                        pinned: String::new(),
                        history: String::new(),
                        tail: String::new(),
                    },
                }
            }
            2 => SessionRecord::Provider(ProviderEvent::TextDelta("t".into())),
            3 => SessionRecord::Provider(ProviderEvent::ToolCall {
                id: "c".into(),
                name: "bash".into(),
                input_json: "{}".into(),
            }),
            4 => SessionRecord::Provider(ProviderEvent::Done {
                stop: if b.byte().unwrap_or(0) % 2 == 0 {
                    StopReason::EndTurn
                } else {
                    StopReason::ToolUse
                },
            }),
            5 => SessionRecord::ToolCall {
                id: "c".into(),
                name: "bash".into(),
                args: serde_json::json!({"command": "true"}),
                args_blob: None,
            },
            6 => SessionRecord::Edit {
                path: rel(["a.txt", "b/c.txt", "../x", "/abs"][usize::from(op / 14 % 4)]),
                pre_image: None,
                post_image_blob: None,
                step: b.num(step),
                asserted: false,
            },
            7 => SessionRecord::Checkpoint {
                n: b.num(0),
                commit: "c".repeat(40),
                tree: "d".repeat(40),
                head: None,
                dirty: false,
                turn: b.num(turn),
                step: b.num(step),
                point: Some(if op / 14 % 2 == 0 {
                    CheckpointPoint::TurnEnd
                } else {
                    CheckpointPoint::BeforeExec
                }),
            },
            8 => SessionRecord::CheckpointSkipped {
                reason: "r".into(),
                turn,
                step,
                point: None,
            },
            9 => SessionRecord::TurnEnd {
                reason: FinishReason::Completed,
                evidence: None,
                synthetic: false,
            },
            10 => SessionRecord::Unreadable,
            11 => SessionRecord::Forked,
            12 => SessionRecord::WritesLost {
                paths: vec![rel("a.txt")],
                late: op / 14 % 2 == 0,
            },
            _ => SessionRecord::Compaction {
                removed: vec![blob()],
                summary: "s".into(),
            },
        };
        out.push(rec);
    }
    (out, honest)
}

fuzz_target!(|data: &[u8]| {
    if data.len() > 512 {
        return;
    }
    let (recs, honest) = generate(data);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.jsonl");
    let mut f = std::fs::File::create(&path).unwrap();
    writeln!(f, "{HEADER}").unwrap();
    for (i, rec) in recs.iter().enumerate() {
        // `Unreadable` is never written: it is what a payload of a newer shape reads as.
        let data = match rec {
            SessionRecord::Unreadable => serde_json::json!({"type": "edit", "data": {"x": 1}}),
            rec => serde_json::to_value(rec).unwrap(),
        };
        let line = serde_json::json!({"type": "record", "offset": i, "t": 0, "data": data});
        writeln!(f, "{line}").unwrap();
    }
    drop(f);
    let reader = SessionReader::<SessionRecord>::open(&path).unwrap();

    let mut prev = 0;
    for o in 0..=recs.len() as u64 {
        let s = step_at_offset(&reader, o);
        if honest {
            assert!(s >= prev, "step_at_offset went down at {o}: {prev} -> {s}");
        }
        prev = s;
    }
    let last = step_at_offset(&reader, u64::MAX);
    assert!(
        !matches!(state_at(&reader, last), Err(TtError::NoSuchStep { .. })),
        "state_at({last}) has no such step"
    );
    if let Some(next) = last.checked_add(1) {
        assert!(
            matches!(state_at(&reader, next), Err(TtError::NoSuchStep { .. })),
            "state_at({next}) is past the last step {last}"
        );
    }

    let full = rebuild(&reader, None);
    if honest {
        for n in [last, last.saturating_add(1)] {
            let past = matches!(
                rebuild(&reader, Some(n)),
                Err(ResumeError::NoSuchStep { .. })
            );
            assert_eq!(past, n > last, "rebuild({n}) vs last step {last}");
        }
    }
    drop(full);
});
