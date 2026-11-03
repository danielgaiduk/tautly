use hx_context::compact::{DEFAULT_KEEP_RECENT, DEFAULT_THRESHOLD};
use hx_context::{CompactionRecord, Context, PinSource, Pinned, Turn, UncompactError};
use hx_provider::{
    Block, CacheStyle, Capabilities, Content, EditFormat, Message, ModelId, ReasoningStyle, Role,
};
use hx_tools::{PlanStep, StepStatus};
use serde_json::json;

fn caps() -> Capabilities {
    Capabilities {
        context_window: 100_000,
        max_output: 8_000,
        parallel_tools: true,
        reasoning: ReasoningStyle::None,
        cache: CacheStyle::Explicit,
        images: false,
        edit_format: EditFormat::StrReplace,
    }
}

fn m() -> ModelId {
    "anthropic/claude-sonnet".into()
}

fn msg(role: Role, text: &str) -> Message {
    Message {
        role,
        content: vec![Content::Text(text.into())],
    }
}

/// A user prompt followed by the assistant's answer.
fn turn(i: usize) -> Turn {
    Turn {
        messages: vec![
            msg(Role::User, &format!("question {i}")),
            msg(Role::Assistant, &format!("answer {i}")),
        ],
    }
}

fn ctx_with(n: usize) -> Context {
    Context {
        history: (0..n).map(turn).collect(),
        ..Default::default()
    }
}

fn status(used: u32, budget: u32) -> hx_context::ContextStatus {
    hx_context::ContextStatus {
        used,
        budget,
        breakdown: Default::default(),
        warnings: vec![],
    }
}

fn first_text(t: &Turn) -> &str {
    match &t.messages[0].content[0] {
        Content::Text(t) => t,
        other => panic!("not text: {other:?}"),
    }
}

fn system_text(req: &hx_provider::CanonicalRequest) -> String {
    req.system
        .iter()
        .map(|b| match b {
            Block::Text(t) => t.as_str(),
            _ => "",
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn triggers_at_80_percent_not_below() {
    let c = Context::default();
    let t = Some(DEFAULT_THRESHOLD);
    assert!(c.needs_compaction(&status(80_000, 100_000), t));
    assert!(c.needs_compaction(&status(95_000, 100_000), t));
    assert!(!c.needs_compaction(&status(79_999, 100_000), t));
    assert!(!c.needs_compaction(&status(0, 100_000), t));
    // A zero window never triggers (nothing sensible to compare against).
    assert!(!c.needs_compaction(&status(10, 0), t));
    // Other thresholds work the same way.
    assert!(c.needs_compaction(&status(50_000, 100_000), Some(0.5)));
    assert!(!c.needs_compaction(&status(49_999, 100_000), Some(0.5)));
}

#[test]
fn disabled_threshold_never_triggers() {
    let c = Context::default();
    assert!(!c.needs_compaction(&status(100_000, 100_000), None));
    assert!(!c.needs_compaction(&status(500_000, 100_000), None));
    // A nonsensical threshold is treated as disabled rather than always firing.
    assert!(!c.needs_compaction(&status(100_000, 100_000), Some(0.0)));
    assert!(!c.needs_compaction(&status(100_000, 100_000), Some(f32::NAN)));
}

fn rich_ctx() -> Context {
    let mut c = ctx_with(10);
    c.pinned
        .push(Pinned::new("never touch prod", PinSource::User));
    c.pinned.push(Pinned::new("use tabs", PinSource::Config));
    c.state.task = Some("migrate the parser".into());
    c.state.plan = vec![PlanStep {
        text: "write tests".into(),
        status: StepStatus::InProgress,
    }];
    c.state.decisions.push("keep the old API".into());
    c.state.questions.push("which MSRV?".into());
    c.state
        .failed_attempts
        .push("regex approach: too slow".into());
    c.state
        .files
        .insert("src/lib.rs".into(), Default::default());
    c
}

#[test]
fn pinned_and_state_survive_verbatim() {
    let mut c = rich_ctx();
    let pinned = c.pinned.clone();
    let state = c.state.clone();
    let (before, _) = c.assemble(&caps(), &[], &m());
    let before_state_msg = before.messages.last().unwrap().clone();

    let (_, removed) = c.compaction_request(&m(), &caps(), DEFAULT_KEEP_RECENT);
    c.apply_compaction(removed, "the summary".into()).unwrap();

    assert_eq!(c.pinned, pinned);
    assert_eq!(c.state, state);
    let (after, _) = c.assemble(&caps(), &[], &m());
    // The rendered state tail is byte-identical and the pinned text still leads.
    assert_eq!(after.messages.last().unwrap(), &before_state_msg);
    let lead = &after.messages[0];
    assert_eq!(lead.role, Role::User);
    match (&lead.content[0], &before.messages[0].content[0]) {
        (Content::Text(a), Content::Text(b)) => assert_eq!(a, b),
        other => panic!("{other:?}"),
    }
    // The stable prefix is untouched.
    assert_eq!(after.system, before.system);
}

#[test]
fn keeps_recent_turns_verbatim() {
    let mut c = ctx_with(10);
    let original = c.history.clone();
    let (_, removed) = c.compaction_request(&m(), &caps(), DEFAULT_KEEP_RECENT);
    assert_eq!(DEFAULT_KEEP_RECENT, 4);
    assert_eq!(removed, 6);
    c.apply_compaction(removed, "s".into()).unwrap();
    assert_eq!(c.history.len(), 1 + 4);
    assert_eq!(&c.history[1..], &original[6..]);
}

#[test]
fn nothing_to_remove_when_history_is_short() {
    let mut c = ctx_with(4);
    let (_, removed) = c.compaction_request(&m(), &caps(), 4);
    assert_eq!(removed, 0);
    let (_, removed) = c.compaction_request(&m(), &caps(), 9);
    assert_eq!(removed, 0);
    let before = c.history.clone();
    assert!(c.apply_compaction(0, "s".into()).is_none());
    assert_eq!(c.history, before);
    assert_eq!(c.first_turn, 0);
}

#[test]
fn summary_replaces_removed_turns_as_single_block() {
    let mut c = ctx_with(10);
    let original = c.history.clone();
    let rec = c
        .apply_compaction(6, "  goal: X\n  decided: Y  ".into())
        .unwrap();
    assert_eq!(rec.removed_turns, original[..6]);
    assert_eq!(rec.summary, "  goal: X\n  decided: Y  ");

    let s = &c.history[0];
    assert_eq!(s.messages.len(), 1);
    assert_eq!(s.messages[0].role, Role::User);
    assert_eq!(s.messages[0].content.len(), 1);
    let text = first_text(s);
    assert!(text.starts_with("<summary>"), "{text}");
    assert!(text.ends_with("</summary>"), "{text}");
    assert!(text.contains("goal: X\n  decided: Y"), "{text}");
    for i in 0..6 {
        assert!(!text.contains(&format!("question {i}")));
    }
}

#[test]
fn assembled_order_is_pinned_summary_recent_state() {
    let mut c = rich_ctx();
    c.apply_compaction(6, "the summary".into()).unwrap();
    let (req, _) = c.assemble(&caps(), &[], &m());
    let first = &req.messages[0];
    assert_eq!(first.role, Role::User);
    // Pinned and the summary share the leading user message, pinned first.
    assert_eq!(first.content.len(), 2);
    assert!(matches!(&first.content[0], Content::Text(t) if t.starts_with("<pinned>")));
    assert!(matches!(&first.content[1], Content::Text(t) if t.starts_with("<summary>")));
    // Then the four recent turns, then the state tail.
    assert_eq!(req.messages.len(), 1 + 8 + 1);
    assert!(matches!(&req.messages[1].content[0], Content::Text(t) if t == "question 6"));
    assert!(matches!(&req.messages.last().unwrap().content[0],
        Content::Text(t) if t.starts_with("<working_state>")));
}

#[test]
fn uncompact_restores_when_fits() {
    let mut c = rich_ctx();
    let original = c.history.clone();
    let rec = c.apply_compaction(6, "the summary".into()).unwrap();
    assert_eq!(c.history.len(), 5);
    c.uncompact(&rec, 100_000).unwrap();
    assert_eq!(c.history, original);
    assert_eq!(c.first_turn, 0);
}

#[test]
fn uncompact_refuses_when_too_large() {
    let mut c = ctx_with(10);
    // Big removed turns make the restored context much larger than the compacted one.
    for t in &mut c.history[..6] {
        t.messages[1].content = vec![Content::Text("x".repeat(4_000))];
    }
    let rec = c.apply_compaction(6, "short".into()).unwrap();
    let compacted = c.history.clone();
    // The exact size once restored (no tools, so `used` is what uncompact estimates).
    let needed = {
        let mut restored = c.clone();
        restored.uncompact(&rec, u32::MAX).unwrap();
        restored.assemble(&caps(), &[], &m()).1.used
    };
    assert!(needed > c.assemble(&caps(), &[], &m()).1.used + 5_000);

    let err = c.uncompact(&rec, needed - 1).unwrap_err();
    assert_eq!(
        err,
        UncompactError::TooLarge {
            needed,
            budget: needed - 1
        }
    );
    // Nothing changed on refusal.
    assert_eq!(c.history, compacted);
    assert_eq!(c.first_turn, 5);
    // Exactly enough is accepted.
    c.uncompact(&rec, needed).unwrap();
    assert_eq!(c.history.len(), 10);
}

#[test]
fn uncompact_only_applies_to_the_latest_compaction() {
    let mut c = ctx_with(12);
    let rec1 = c.apply_compaction(4, "first".into()).unwrap();
    let rec2 = c.apply_compaction(4, "second".into()).unwrap();
    assert_eq!(c.uncompact(&rec1, u32::MAX), Err(UncompactError::NotLatest));
    // In stack order both undo.
    let original = ctx_with(12).history;
    c.uncompact(&rec2, u32::MAX).unwrap();
    c.uncompact(&rec1, u32::MAX).unwrap();
    assert_eq!(c.history, original);
    assert_eq!(c.first_turn, 0);
    // An undone record does not apply again.
    assert_eq!(c.uncompact(&rec1, u32::MAX), Err(UncompactError::NotLatest));
}

#[test]
fn compaction_request_instructs_to_preserve_open_questions_and_decisions() {
    let c = ctx_with(10);
    let (req, removed) = c.compaction_request(&m(), &caps(), DEFAULT_KEEP_RECENT);
    assert_eq!(removed, 6);
    let sys = system_text(&req).to_lowercase();
    for word in [
        "open questions",
        "decisions",
        "failed attempts",
        "file paths",
        "identifiers",
        "verbatim",
    ] {
        assert!(sys.contains(word), "missing {word:?} in {sys}");
    }
    // A short dedicated prompt, not the stable prefix; no tools.
    assert_eq!(req.system.len(), 1);
    assert!(req.tools.is_empty());
    assert_eq!(req.model, m());
    assert_eq!(req.max_output, 8_000);
    assert!(req.cache_breakpoints.is_empty());
    // The removed turns are the input; the kept ones are not.
    assert_eq!(req.messages.len(), 1);
    assert_eq!(req.messages[0].role, Role::User);
    let input = match &req.messages[0].content[..] {
        [Content::Text(t)] => t.clone(),
        other => panic!("{other:?}"),
    };
    for i in 0..6 {
        assert!(input.contains(&format!("question {i}")), "{input}");
        assert!(input.contains(&format!("answer {i}")), "{input}");
    }
    assert!(!input.contains("question 6"));
}

#[test]
fn compaction_request_excludes_pinned_and_state_and_shows_tool_calls() {
    let mut c = rich_ctx();
    c.history[0].messages.push(Message {
        role: Role::Assistant,
        content: vec![Content::ToolUse {
            id: "t1".into(),
            name: "read".into(),
            input: json!({"path": "src/main.rs"}),
        }],
    });
    c.history[0].messages.push(Message {
        role: Role::User,
        content: vec![Content::ToolResult {
            id: "t1".into(),
            output: "fn main() {}".into(),
            is_error: false,
        }],
    });
    let (req, _) = c.compaction_request(&m(), &caps(), 4);
    let Content::Text(input) = &req.messages[0].content[0] else {
        panic!()
    };
    assert!(!input.contains("never touch prod"));
    assert!(!input.contains("<working_state>"));
    assert!(input.contains("src/main.rs"), "{input}");
    assert!(input.contains("fn main() {}"), "{input}");
}

#[test]
fn never_splits_a_tool_use_from_its_result() {
    // Turns of one message each so the cut can fall between a call and its result.
    let mut c = Context::default();
    for i in 0..8 {
        let t = match i {
            // Cut would fall between turn 3 (tool_use) and turn 4 (tool_result) for keep 4.
            3 => Message {
                role: Role::Assistant,
                content: vec![Content::ToolUse {
                    id: "call".into(),
                    name: "read".into(),
                    input: json!({}),
                }],
            },
            4 => Message {
                role: Role::User,
                content: vec![Content::ToolResult {
                    id: "call".into(),
                    output: "ok".into(),
                    is_error: false,
                }],
            },
            i if i % 2 == 0 => msg(Role::User, &format!("u{i}")),
            i => msg(Role::Assistant, &format!("a{i}")),
        };
        c.history.push(Turn { messages: vec![t] });
    }
    let (_, removed) = c.compaction_request(&m(), &caps(), 4);
    // A naive cut removes 4 (turns 0..4); the pair forces it back to 3.
    assert_eq!(removed, 3);
    let original = c.history.clone();
    c.apply_compaction(removed, "s".into()).unwrap();
    assert_eq!(&c.history[1..], &original[3..]);

    // A cut that does not split a pair is unaffected.
    let c2 = Context {
        history: original.clone(),
        ..Default::default()
    };
    let (_, removed) = c2.compaction_request(&m(), &caps(), 3);
    assert_eq!(removed, 5);
}

#[test]
fn pair_at_the_very_start_can_leave_nothing_to_remove() {
    let mut c = Context::default();
    c.history.push(Turn {
        messages: vec![Message {
            role: Role::Assistant,
            content: vec![Content::ToolUse {
                id: "a".into(),
                name: "read".into(),
                input: json!({}),
            }],
        }],
    });
    c.history.push(Turn {
        messages: vec![Message {
            role: Role::User,
            content: vec![Content::ToolResult {
                id: "a".into(),
                output: "ok".into(),
                is_error: false,
            }],
        }],
    });
    let (_, removed) = c.compaction_request(&m(), &caps(), 1);
    assert_eq!(removed, 0);
}

#[test]
fn absolute_turn_numbers_after_two_compactions() {
    let mut c = ctx_with(12);
    assert_eq!(c.first_turn, 0);

    // Turns 0..=5 go; the summary stands in the slot of turn 5.
    let r1 = c.apply_compaction(6, "one".into()).unwrap();
    assert_eq!((r1.first_removed_turn, r1.last_removed_turn), (0, 5));
    assert_eq!(c.history.len(), 7);
    assert_eq!(c.first_turn, 5);
    // history[1] is still absolute turn 6.
    assert_eq!(first_text(&c.history[1]), "question 6");
    assert_eq!(c.first_turn + 1, 6);

    // The session goes on: two more turns, then compact the summary and 3 turns more.
    c.history.push(turn(12));
    c.history.push(turn(13));
    let r2 = c.apply_compaction(4, "two".into()).unwrap();
    // Removed: the summary slot (5) and absolute turns 6, 7, 8.
    assert_eq!((r2.first_removed_turn, r2.last_removed_turn), (5, 8));
    assert_eq!(c.first_turn, 8);
    assert_eq!(first_text(&c.history[1]), "question 9");
    assert_eq!(c.history.len(), 1 + 5);
    // The second record holds the first summary turn among its removed turns.
    assert!(first_text(&r2.removed_turns[0]).contains("one"));

    // Undo restores the numbering step by step.
    c.uncompact(&r2, u32::MAX).unwrap();
    assert_eq!(c.first_turn, 5);
    c.uncompact(&r1, u32::MAX).unwrap();
    assert_eq!(c.first_turn, 0);
}

#[test]
fn first_turn_offset_is_respected() {
    let mut c = ctx_with(8);
    c.first_turn = 100;
    let rec = c.apply_compaction(3, "s".into()).unwrap();
    assert_eq!((rec.first_removed_turn, rec.last_removed_turn), (100, 102));
    assert_eq!(c.first_turn, 102);
}

#[test]
fn record_is_comparable_and_clonable() {
    let mut c = ctx_with(6);
    let rec: CompactionRecord = c.apply_compaction(2, "s".into()).unwrap();
    assert_eq!(rec.clone(), rec);
}
