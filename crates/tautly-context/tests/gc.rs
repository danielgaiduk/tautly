use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tautly_context::{Context, GcRecord, MutationReason, PinSource, Pinned, Turn};
use tautly_provider::{
    CacheStyle, Capabilities, Content, EditFormat, Message, Price, ReasoningStyle, Role,
};
use tautly_tools::{AppliedEffect, LineRange, Shown};

const TTL: Duration = Duration::from_secs(300);

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

fn p(path: &str) -> PathBuf {
    PathBuf::from(path)
}

fn read_at(path: &str, range: LineRange, shown: Shown) -> AppliedEffect {
    AppliedEffect::Read {
        path: p(path),
        range,
        file_hash: blake3::hash(path.as_bytes()),
        shown,
    }
}

fn read(path: &str) -> AppliedEffect {
    read_at(path, LineRange::ALL, Shown::Full)
}

fn edited(path: &str) -> AppliedEffect {
    AppliedEffect::Edited {
        path: p(path),
        pre_image: None,
        pre_mode: None,
        post_hash: blake3::hash(b"new"),
    }
}

/// Record one tool call as its own turn: the assistant's call, then the result. Turns are
/// numbered from the context's `next_turn`, so the nth call is turn n.
#[allow(clippy::too_many_arguments)]
fn call(
    c: &mut Context,
    id: &str,
    tool: &str,
    args: Value,
    effects: &[AppliedEffect],
    output: &str,
    is_error: bool,
) -> u32 {
    let turn = c.next_turn();
    c.history.push(Turn {
        messages: vec![
            Message {
                role: Role::Assistant,
                content: vec![Content::ToolUse {
                    id: id.into(),
                    name: tool.into(),
                    input: args.clone(),
                }],
            },
            Message {
                role: Role::User,
                content: vec![Content::ToolResult {
                    id: id.into(),
                    output: output.into(),
                    is_error,
                }],
            },
        ],
    });
    c.push_tool_result(id, tool, &args, effects, is_error, turn);
    turn
}

fn read_call(c: &mut Context, id: &str, path: &str, output: &str) -> u32 {
    call(
        c,
        id,
        "read",
        json!({"path": path}),
        &[read(path)],
        output,
        false,
    )
}

fn edit_call(c: &mut Context, id: &str, path: &str) -> u32 {
    call(
        c,
        id,
        "edit",
        json!({"path": path, "old_text": "a", "new_text": "b"}),
        &[edited(path)],
        "edited",
        false,
    )
}

fn bash_call(c: &mut Context, id: &str, cmd: &str, output: &str) -> u32 {
    call(
        c,
        id,
        "bash",
        json!({"command": cmd}),
        &[AppliedEffect::Exec { may_write: false }],
        output,
        false,
    )
}

fn output_of<'a>(c: &'a Context, id: &str) -> &'a str {
    c.history
        .iter()
        .flat_map(|t| &t.messages)
        .flat_map(|m| &m.content)
        .find_map(|x| match x {
            Content::ToolResult { id: i, output, .. } if i == id => Some(output.as_str()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no result {id}"))
}

fn elided_ids(rec: &GcRecord) -> Vec<&str> {
    rec.elided.iter().map(|e| e.call_id.as_str()).collect()
}

fn stub(n: u32) -> String {
    format!("[elided: superseded at turn {n}]")
}

/// A reason as the scheduler would give it; these tests are about what gets collected.
const NOW: Option<MutationReason> = Some(MutationReason::ColdCache);

fn big(label: &str) -> String {
    format!("{label}\n{}", "line of file content\n".repeat(300))
}

#[test]
fn read_superseded_by_later_edit_of_same_path() {
    let mut c = Context::default();
    read_call(&mut c, "r1", "/w/a.rs", "old a");
    read_call(&mut c, "r2", "/w/b.rs", "b");
    let t = edit_call(&mut c, "e1", "/w/a.rs");
    let rec = c.collect(NOW).unwrap();
    assert_eq!(elided_ids(&rec), ["r1"]);
    assert_eq!(output_of(&c, "r1"), stub(t));
    assert_eq!(output_of(&c, "r2"), "b");
    assert_eq!(output_of(&c, "e1"), "edited");
}

#[test]
fn read_superseded_by_later_reread() {
    let mut c = Context::default();
    read_call(&mut c, "r1", "/w/a.rs", "first");
    let t = read_call(&mut c, "r2", "/w/a.rs", "second");
    let rec = c.collect(NOW).unwrap();
    assert_eq!(elided_ids(&rec), ["r1"]);
    assert_eq!(output_of(&c, "r1"), stub(t));
    assert_eq!(output_of(&c, "r2"), "second");
}

#[test]
fn bash_superseded_by_same_command_string() {
    let mut c = Context::default();
    bash_call(&mut c, "b1", "cargo test", "failed");
    bash_call(&mut c, "b2", "cargo test -q", "other");
    bash_call(&mut c, "b3", "cargo test ", "trailing space");
    let t = bash_call(&mut c, "b4", "cargo test", "passed");
    let rec = c.collect(NOW).unwrap();
    // Only the exact string counts.
    assert_eq!(elided_ids(&rec), ["b1"]);
    assert_eq!(output_of(&c, "b1"), stub(t));
    for id in ["b2", "b3", "b4"] {
        assert_ne!(output_of(&c, id), stub(t), "{id}");
    }
    assert_eq!(output_of(&c, "b4"), "passed");
}

#[test]
fn failed_edit_superseded_by_later_successful_edit() {
    let mut c = Context {
        root: Some(p("/w")),
        ..Default::default()
    };
    // The failed call names the path as the model wrote it, relative to the root.
    call(
        &mut c,
        "f1",
        "edit",
        json!({"path": "src/a.rs", "old_text": "x", "new_text": "y"}),
        &[],
        "no match",
        true,
    );
    // A failure on another path, and a failure that is still the latest on its path.
    call(
        &mut c,
        "f2",
        "edit",
        json!({"path": "src/b.rs", "old_text": "x", "new_text": "y"}),
        &[],
        "no match",
        true,
    );
    let t = edit_call(&mut c, "e1", "/w/src/a.rs");
    let rec = c.collect(NOW).unwrap();
    assert_eq!(elided_ids(&rec), ["f1"]);
    assert_eq!(output_of(&c, "f1"), stub(t));
    assert_eq!(output_of(&c, "f2"), "no match");
    // The successful edit is never a failed one's victim.
    assert_eq!(output_of(&c, "e1"), "edited");
}

#[test]
fn failed_patch_covers_every_file_it_names() {
    let mut c = Context {
        root: Some(p("/w")),
        ..Default::default()
    };
    let patch = "*** Begin Patch\n*** Update File: a.rs\n@@\n-x\n+y\n*** Update File: b.rs\n@@\n-x\n+y\n*** End Patch";
    call(
        &mut c,
        "f1",
        "edit",
        json!({ "patch": patch }),
        &[],
        "no match",
        true,
    );
    edit_call(&mut c, "e1", "/w/a.rs");
    assert!(c.collect(NOW).is_none(), "b.rs still has no good edit");
    let t = edit_call(&mut c, "e2", "/w/b.rs");
    let rec = c.collect(NOW).unwrap();
    assert_eq!(elided_ids(&rec), ["f1"]);
    assert_eq!(output_of(&c, "f1"), stub(t));
}

#[test]
fn latest_result_never_elided() {
    let mut c = Context::default();
    bash_call(&mut c, "b1", "ls", "one");
    read_call(&mut c, "r1", "/w/a.rs", "a");
    let rec = c.collect(NOW);
    assert!(rec.is_none(), "nothing is superseded yet");
    assert_eq!(output_of(&c, "b1"), "one");
    // Three results of one key: only the last survives, however many rounds.
    bash_call(&mut c, "b2", "ls", "two");
    bash_call(&mut c, "b3", "ls", "three");
    let rec = c.collect(NOW).unwrap();
    assert_eq!(elided_ids(&rec), ["b1", "b2"]);
    assert_eq!(output_of(&c, "b3"), "three");
    assert!(c.collect(NOW).is_none());
    assert_eq!(output_of(&c, "b3"), "three");
    assert_eq!(output_of(&c, "r1"), "a");
}

#[test]
fn stub_text_and_call_id_preserved() {
    let mut c = Context::default();
    read_call(&mut c, "r1", "/w/a.rs", "old");
    read_call(&mut c, "mid", "/w/b.rs", "b");
    let t = read_call(&mut c, "r2", "/w/a.rs", "new");
    assert_eq!(t, 3);
    let before = c.history.clone();
    c.collect(NOW).unwrap();
    // Only the one result's output changed; call ids, roles, order and error flags did not.
    assert_eq!(c.history.len(), before.len());
    for (a, b) in c.history.iter().zip(&before) {
        assert_eq!(a.messages.len(), b.messages.len());
        for (ma, mb) in a.messages.iter().zip(&b.messages) {
            assert_eq!(ma.role, mb.role);
            for (ca, cb) in ma.content.iter().zip(&mb.content) {
                match (ca, cb) {
                    (
                        Content::ToolResult {
                            id: ia,
                            output: oa,
                            is_error: ea,
                        },
                        Content::ToolResult {
                            id: ib,
                            output: ob,
                            is_error: eb,
                        },
                    ) => {
                        assert_eq!(ia, ib);
                        assert_eq!(ea, eb);
                        if ia == "r1" {
                            assert_eq!(oa, "[elided: superseded at turn 3]");
                        } else {
                            assert_eq!(oa, ob);
                        }
                    }
                    _ => assert_eq!(ca, cb),
                }
            }
        }
    }
    // Every tool_use still has its tool_result.
    let (req, _) = c.assemble(&caps(), &[], &"anthropic/m".into());
    let mut uses: Vec<&str> = Vec::new();
    let mut results: Vec<&str> = Vec::new();
    for x in req.messages.iter().flat_map(|m| &m.content) {
        match x {
            Content::ToolUse { id, .. } => uses.push(id),
            Content::ToolResult { id, .. } => results.push(id),
            _ => {}
        }
    }
    assert_eq!(uses, results);
    assert_eq!(uses, ["r1", "mid", "r2"]);
}

#[test]
fn elided_error_result_keeps_its_error_flag() {
    let mut c = Context::default();
    call(
        &mut c,
        "b1",
        "bash",
        json!({"command": "false"}),
        &[],
        "exit 1",
        true,
    );
    bash_call(&mut c, "b2", "false", "exit 0");
    c.collect(NOW).unwrap();
    assert!(matches!(
        &c.history[0].messages[1].content[0],
        Content::ToolResult { is_error: true, output, .. } if output == "[elided: superseded at turn 2]"
    ));
}

#[test]
fn below_threshold_returns_none() {
    let now = Instant::now();
    let mut c = Context::default();
    c.note_request(now - Duration::from_secs(10), 50_000);
    // Elidable: one 1000-token read.
    read_call(&mut c, "r1", "/w/a.rs", &"x".repeat(4_000));
    read_call(&mut c, "r2", "/w/a.rs", "small");
    let freed = c.pending_gc();
    assert!((900..=1_100).contains(&freed), "{freed}");
    // Warm, no price: max(2000, 5% of the window) = 5000 for a 100k window.
    assert_eq!(
        c.gc_decision(now, Some(TTL), None, 40_000, 100_000),
        None,
        "1000 < 5000"
    );
    let before = c.history.clone();
    assert!(c.collect(None).is_none());
    assert_eq!(c.history, before, "no decision, no change");
    // Cold: any amount.
    assert_eq!(
        c.gc_decision(now + TTL, Some(TTL), None, 40_000, 100_000),
        Some(MutationReason::ColdCache)
    );
    // The 2000 floor: the same 1000 tokens against a 10k window are still too few ...
    assert_eq!(c.gc_decision(now, Some(TTL), None, 4_000, 10_000), None);
    // ... but 2000 of them in a 10k window are not.
    let mut c2 = Context::default();
    c2.note_request(now - Duration::from_secs(10), 5_000);
    read_call(&mut c2, "r1", "/w/a.rs", &"x".repeat(8_400));
    read_call(&mut c2, "r2", "/w/a.rs", "small");
    assert_eq!(
        c2.gc_decision(now, Some(TTL), None, 4_000, 10_000),
        Some(MutationReason::CostBenefit)
    );
}

#[test]
fn above_threshold_elides_all_superseded() {
    let now = Instant::now();
    let mut c = Context::default();
    c.note_request(now - Duration::from_secs(10), 50_000);
    // Three superseded results of 5k+ tokens in total, and live ones.
    for (i, f) in ["a", "b", "c"].iter().enumerate() {
        read_call(&mut c, &format!("old{i}"), &format!("/w/{f}.rs"), &big(f));
    }
    bash_call(&mut c, "old_b", "make", &big("make"));
    read_call(&mut c, "live_x", "/w/x.rs", "x");
    edit_call(&mut c, "e_a", "/w/a.rs");
    read_call(&mut c, "re_b", "/w/b.rs", "b2");
    read_call(&mut c, "re_c", "/w/c.rs", "c2");
    bash_call(&mut c, "new_b", "make", "ok");
    let originals: Vec<String> = ["old0", "old1", "old2", "old_b"]
        .iter()
        .map(|id| output_of(&c, id).to_string())
        .collect();

    let freed = c.pending_gc();
    assert!(freed >= 4 * 1_000, "{freed}");
    let reason = c.gc_decision(now, Some(TTL), None, 40_000, 100_000);
    assert_eq!(reason, Some(MutationReason::CostBenefit));
    let rec = c.collect(reason).unwrap();
    assert_eq!(elided_ids(&rec), ["old0", "old1", "old2", "old_b"]);
    assert_eq!(rec.reason, MutationReason::CostBenefit);
    assert_eq!(rec.tokens_freed, freed);
    for (e, want) in rec.elided.iter().zip(&originals) {
        let original = &e.original;
        assert_eq!(original, want);
    }
    for id in ["live_x", "e_a", "re_b", "re_c", "new_b"] {
        assert!(!output_of(&c, id).starts_with("[elided"), "{id}");
    }
    // Nothing is left to collect.
    assert_eq!(c.pending_gc(), 0);
    assert!(c.collect(NOW).is_none());
}

#[test]
fn restore_gc_restores_original_outputs() {
    let mut c = Context::default();
    let odd = "tab\there\r\nunicode \u{1F600} \u{e9}\n\n  trailing  \n";
    read_call(&mut c, "r1", "/w/a.rs", odd);
    bash_call(&mut c, "b1", "ls", "");
    read_call(&mut c, "r2", "/w/a.rs", "new");
    bash_call(&mut c, "b2", "ls", "x");
    let before = c.history.clone();
    let rec = c.collect(NOW).unwrap();
    assert_ne!(c.history, before);
    c.restore_gc(&rec);
    assert_eq!(c.history, before, "byte-identical");
    assert_eq!(output_of(&c, "r1"), odd);
    // Elided again on the next collection.
    let rec2 = c.collect(NOW).unwrap();
    assert_eq!(elided_ids(&rec2), ["r1", "b1"]);
}

#[test]
fn restore_skips_results_that_are_gone_or_changed() {
    let mut c = Context::default();
    read_call(&mut c, "r1", "/w/a.rs", "old");
    read_call(&mut c, "r2", "/w/a.rs", "new");
    let rec = c.collect(NOW).unwrap();
    // The elided turn was compacted away.
    c.apply_compaction(1, "s".into()).unwrap();
    let after = c.history.clone();
    c.restore_gc(&rec);
    assert_eq!(c.history, after);
}

#[test]
fn uncompact_then_restore_gc_brings_everything_back() {
    let mut c = Context::default();
    read_call(&mut c, "r1", "/w/a.rs", "old");
    read_call(&mut c, "r2", "/w/a.rs", "new");
    for i in 0..4 {
        c.history.push(Turn {
            messages: vec![Message {
                role: Role::User,
                content: vec![Content::Text(format!("t{i}"))],
            }],
        });
    }
    let before = c.history.clone();
    let rec = c.collect(NOW).unwrap();
    let comp = c.apply_compaction(2, "s".into()).unwrap();
    c.uncompact(&comp, u32::MAX).unwrap();
    // The turns come back as they were compacted: stubbed ...
    assert_eq!(output_of(&c, "r1"), stub(2));
    c.restore_gc(&rec);
    assert_eq!(c.history, before);
    // ... and the result is collectable again afterwards.
    assert_eq!(elided_ids(&c.collect(NOW).unwrap()), ["r1"]);
}

#[test]
fn compacted_away_results_do_not_count() {
    let mut c = Context::default();
    read_call(&mut c, "r1", "/w/a.rs", "old");
    read_call(&mut c, "r2", "/w/a.rs", "mid");
    c.history.push(Turn {
        messages: vec![Message {
            role: Role::User,
            content: vec![Content::Text("t".into())],
        }],
    });
    // r1 and r2 are summarized away; the later read is the only one left in view.
    c.apply_compaction(2, "s".into()).unwrap();
    let t = read_call(&mut c, "r3", "/w/a.rs", "newest");
    assert!(c.collect(NOW).is_none(), "r1/r2 are not in the history");
    assert_eq!(output_of(&c, "r3"), "newest");
    assert_eq!(t, 4);
}

#[test]
fn batched_read_is_superseded_only_when_all_its_paths_are() {
    let mut c = Context::default();
    call(
        &mut c,
        "batch",
        "read",
        json!({"paths": ["/w/a.rs", "/w/b.rs"]}),
        &[read("/w/a.rs"), read("/w/b.rs")],
        "A and B",
        false,
    );
    read_call(&mut c, "ra", "/w/a.rs", "a again");
    assert!(
        c.collect(NOW).is_none(),
        "b.rs is only in the batch, so the batch stays"
    );
    let t = edit_call(&mut c, "eb", "/w/b.rs");
    let rec = c.collect(NOW).unwrap();
    assert_eq!(elided_ids(&rec), ["batch"]);
    assert_eq!(output_of(&c, "batch"), stub(t));
    // A batch also supersedes several single reads at once.
    let mut c = Context::default();
    read_call(&mut c, "a", "/w/a.rs", "a");
    read_call(&mut c, "b", "/w/b.rs", "b");
    let t = call(
        &mut c,
        "batch",
        "read",
        json!({"paths": ["/w/a.rs", "/w/b.rs"]}),
        &[read("/w/a.rs"), read("/w/b.rs")],
        "A and B",
        false,
    );
    let rec = c.collect(NOW).unwrap();
    assert_eq!(elided_ids(&rec), ["a", "b"]);
    assert_eq!(output_of(&c, "a"), stub(t));
}

#[test]
fn unchanged_and_diff_reads_never_supersede_their_base() {
    let mut c = Context::default();
    read_call(&mut c, "base", "/w/a.rs", "full file");
    let one = |shown| read_at("/w/a.rs", LineRange::ALL, shown);
    call(
        &mut c,
        "same",
        "read",
        json!({"path": "/w/a.rs"}),
        &[one(Shown::Unchanged { since: 1 })],
        "unchanged since turn 1",
        false,
    );
    call(
        &mut c,
        "diff",
        "read",
        json!({"path": "/w/a.rs"}),
        &[one(Shown::Diff { since: 1 })],
        "@@ -1 +1 @@",
        false,
    );
    assert!(c.collect(NOW).is_none(), "the base is still needed");
    for id in ["base", "same", "diff"] {
        assert!(!output_of(&c, id).starts_with("[elided"), "{id}");
    }
    // A full re-read supersedes the base and everything that leaned on it.
    let t = read_call(&mut c, "full", "/w/a.rs", "full again");
    let rec = c.collect(NOW).unwrap();
    assert_eq!(elided_ids(&rec), ["base", "same", "diff"]);
    assert_eq!(output_of(&c, "diff"), stub(t));
    // An edit does too.
    let mut c = Context::default();
    read_call(&mut c, "base", "/w/a.rs", "full file");
    call(
        &mut c,
        "same",
        "read",
        json!({"path": "/w/a.rs"}),
        &[one(Shown::Unchanged { since: 1 })],
        "unchanged since turn 1",
        false,
    );
    let t = edit_call(&mut c, "e", "/w/a.rs");
    let rec = c.collect(NOW).unwrap();
    assert_eq!(elided_ids(&rec), ["base", "same"]);
    assert_eq!(output_of(&c, "same"), stub(t));
}

#[test]
fn a_cut_short_read_never_replaces_a_full_one() {
    let partial = |path: &str| read_at(path, LineRange::ALL, Shown::Partial);
    // Full, then the same lines cut short (the file grew): the full output stays.
    let mut c = Context::default();
    read_call(&mut c, "full", "/w/a.rs", "whole");
    call(
        &mut c,
        "part",
        "read",
        json!({"path": "/w/a.rs"}),
        &[partial("/w/a.rs")],
        "first 2000 lines",
        false,
    );
    assert!(c.collect(NOW).is_none());
    assert_eq!(output_of(&c, "full"), "whole");

    // Two cut-short reads of the same lines: the older is redundant.
    let mut c = Context::default();
    let one = |c: &mut Context, id: &str| {
        call(
            c,
            id,
            "read",
            json!({"path": "/w/a.rs"}),
            &[partial("/w/a.rs")],
            "first 2000 lines",
            false,
        )
    };
    one(&mut c, "p1");
    let t = one(&mut c, "p2");
    let rec = c.collect(NOW).unwrap();
    assert_eq!(elided_ids(&rec), ["p1"]);
    assert_eq!(output_of(&c, "p1"), stub(t));

    // A full read replaces a cut-short one.
    let t = read_call(&mut c, "full", "/w/a.rs", "whole");
    let rec = c.collect(NOW).unwrap();
    assert_eq!(elided_ids(&rec), ["p2"]);
    assert_eq!(output_of(&c, "p2"), stub(t));
}

#[test]
fn failed_edit_without_a_root_matches_only_the_exact_path() {
    let mut c = Context::default();
    assert!(c.root.is_none());
    call(
        &mut c,
        "f1",
        "edit",
        json!({"path": "a.rs", "old_text": "x", "new_text": "y"}),
        &[],
        "no match",
        true,
    );
    call(
        &mut c,
        "f2",
        "edit",
        json!({"path": "/x/y/b.rs", "old_text": "x", "new_text": "y"}),
        &[],
        "no match",
        true,
    );
    // A relative name cannot be resolved without a root: another file with that tail is not it.
    edit_call(&mut c, "e1", "/x/y/a.rs");
    assert!(c.collect(NOW).is_none());
    // An absolute path still matches exactly.
    let t = edit_call(&mut c, "e2", "/x/y/b.rs");
    let rec = c.collect(NOW).unwrap();
    assert_eq!(elided_ids(&rec), ["f2"]);
    assert_eq!(output_of(&c, "f2"), stub(t));
}

#[test]
fn reads_of_other_line_ranges_are_not_superseded_by_a_narrower_reread() {
    let r = |s, e| LineRange {
        start: Some(s),
        end: Some(e),
    };
    let mut c = Context::default();
    call(
        &mut c,
        "top",
        "read",
        json!({"path": "/w/a.rs", "start": 1, "end": 50}),
        &[read_at("/w/a.rs", r(1, 50), Shown::Full)],
        "lines 1-50",
        false,
    );
    call(
        &mut c,
        "bottom",
        "read",
        json!({"path": "/w/a.rs", "start": 100, "end": 150}),
        &[read_at("/w/a.rs", r(100, 150), Shown::Full)],
        "lines 100-150",
        false,
    );
    assert!(
        c.collect(NOW).is_none(),
        "different lines, both still needed"
    );
    // A read that includes the earlier range supersedes it.
    let t = call(
        &mut c,
        "whole",
        "read",
        json!({"path": "/w/a.rs"}),
        &[read("/w/a.rs")],
        "everything",
        false,
    );
    let rec = c.collect(NOW).unwrap();
    assert_eq!(elided_ids(&rec), ["top", "bottom"]);
    assert_eq!(output_of(&c, "top"), stub(t));
    // An edit supersedes reads of any range.
    let mut c = Context::default();
    call(
        &mut c,
        "top",
        "read",
        json!({"path": "/w/a.rs"}),
        &[read_at("/w/a.rs", r(1, 50), Shown::Full)],
        "lines 1-50",
        false,
    );
    edit_call(&mut c, "e", "/w/a.rs");
    assert_eq!(elided_ids(&c.collect(NOW).unwrap()), ["top"]);
}

#[test]
fn forgotten_paths_lists_every_path_an_elided_read_covered() {
    let mut c = Context::default();
    call(
        &mut c,
        "batch",
        "read",
        json!({"paths": ["/w/a.rs", "/w/b.rs"]}),
        &[read("/w/a.rs"), read("/w/b.rs")],
        "A and B",
        false,
    );
    read_call(&mut c, "c", "/w/c.rs", "c");
    bash_call(&mut c, "ls1", "ls", "1");
    // Dependent reads count too.
    call(
        &mut c,
        "same",
        "read",
        json!({"path": "/w/d.rs"}),
        &[read_at(
            "/w/d.rs",
            LineRange::ALL,
            Shown::Unchanged { since: 1 },
        )],
        "unchanged",
        false,
    );
    read_call(&mut c, "a2", "/w/a.rs", "a");
    edit_call(&mut c, "eb", "/w/b.rs");
    edit_call(&mut c, "ed", "/w/d.rs");
    bash_call(&mut c, "ls2", "ls", "2");
    let rec = c.collect(NOW).unwrap();
    assert_eq!(elided_ids(&rec), ["batch", "ls1", "same"]);
    // Not /w/c.rs (its read stays) nor anything bash covers.
    assert_eq!(
        rec.forgotten_paths,
        [p("/w/a.rs"), p("/w/b.rs"), p("/w/d.rs")]
    );
}

#[test]
fn collecting_changes_history_only() {
    let mut c = Context {
        agents_md: Some("rules".into()),
        memory_index: Some("- [fact] x".into()),
        ..Default::default()
    };
    c.pinned.push(Pinned::new("keep me", PinSource::User));
    read_call(&mut c, "r1", "/w/a.rs", &big("a"));
    read_call(&mut c, "r2", "/w/a.rs", "a");
    let tools: Vec<tautly_provider::ToolSpec> = Vec::new();
    let model = "anthropic/m".into();
    let (before, _) = c.assemble(&caps(), &tools, &model);
    let pinned = c.pinned.clone();
    let state = c.state.clone();
    let rec = c.collect(NOW).unwrap();
    let (after, status) = c.assemble(&caps(), &tools, &model);
    assert_eq!(after.system, before.system, "stable prefix");
    assert_eq!(after.tools, before.tools);
    assert_eq!(c.pinned, pinned);
    assert_eq!(c.state, state);
    assert_eq!(after.messages.len(), before.messages.len());
    assert!(rec.tokens_freed > 1_000);
    assert!(status.breakdown.history < 1_000);
}

#[test]
fn collect_with_a_forced_or_manual_reason_reports_it() {
    let mut c = Context::default();
    bash_call(&mut c, "b1", "ls", "1");
    bash_call(&mut c, "b2", "ls", "2");
    let rec = c.collect(Some(MutationReason::Manual)).unwrap();
    assert_eq!(rec.reason, MutationReason::Manual);
    // The price-based path of the scheduler is reachable from here too.
    let now = Instant::now();
    let mut c = Context::default();
    c.note_request(now - Duration::from_secs(5), 1_000);
    bash_call(&mut c, "b1", "ls", &"x".repeat(40_000));
    bash_call(&mut c, "b2", "ls", "2");
    let price = Price {
        input: 3.0,
        output: 15.0,
        cache_read: 0.3,
        cache_write: 3.75,
    };
    assert_eq!(
        c.gc_decision(now, Some(TTL), Some(&price), 20_000, 100_000),
        Some(MutationReason::CostBenefit)
    );
    // Forced when the context is nearly full, however little is pending.
    let mut c = Context::default();
    c.note_request(now - Duration::from_secs(5), 100_000);
    bash_call(&mut c, "b1", "ls", &"x".repeat(400));
    bash_call(&mut c, "b2", "ls", "2");
    assert_eq!(
        c.gc_decision(now, Some(TTL), Some(&price), 95_000, 100_000),
        Some(MutationReason::Forced)
    );
}

// ---- resume: per-entry stub turns and reapply_gc (D1) ----

/// A context with superseded reads, built the same way every time.
fn replayable() -> Context {
    let mut c = Context::default();
    read_call(&mut c, "r1", "/p/a.rs", &big("a v1"));
    read_call(&mut c, "r2", "/p/b.rs", &big("b v1"));
    edit_call(&mut c, "e1", "/p/a.rs");
    read_call(&mut c, "r3", "/p/b.rs", &big("b v2"));
    bash_call(&mut c, "b1", "cargo test", "ok");
    c
}

#[test]
fn record_carries_the_superseding_turn_of_each_entry() {
    let mut c = replayable();
    let rec = c.collect(NOW).unwrap();
    let got: Vec<(&str, u32)> = rec
        .elided
        .iter()
        .map(|e| (e.call_id.as_str(), e.by_turn))
        .collect();
    // r1 is superseded by the edit at turn 3, r2 by the re-read at turn 4.
    assert_eq!(got, [("r1", 3), ("r2", 4)]);
    for e in &rec.elided {
        assert_eq!(output_of(&c, &e.call_id), stub(e.by_turn));
    }
}

#[test]
fn reapply_on_a_fresh_context_reproduces_the_history_byte_for_byte() {
    let mut collected = replayable();
    let rec = collected.collect(NOW).unwrap();
    let persisted: Vec<(String, u32)> = rec
        .elided
        .iter()
        .map(|e| (e.call_id.clone(), e.by_turn))
        .collect();

    // Resume: the same history and results, rebuilt from the log, then the record.
    let mut resumed = replayable();
    assert_eq!(resumed.reapply_gc(&persisted), 0);
    assert!(
        serde_json::to_vec(&resumed.history).unwrap()
            == serde_json::to_vec(&collected.history).unwrap()
    );
    // The collector knows those results are elided: nothing is pending again.
    assert_eq!(resumed.pending_gc(), 0);
    assert_eq!(resumed.collect(NOW), None);
    // Idempotent.
    assert_eq!(resumed.reapply_gc(&persisted), 0);
    assert_eq!(resumed.history, collected.history);
}

#[test]
fn reapply_ignores_and_counts_unknown_ids() {
    let mut c = replayable();
    let before = c.history.clone();
    let unknown = vec![("nope".to_string(), 9), ("gone".to_string(), 2)];
    assert_eq!(c.reapply_gc(&unknown), 2);
    assert_eq!(c.history, before);
    // Known ids are applied even when listed among unknown ones.
    let mixed = vec![("nope".to_string(), 9), ("r1".to_string(), 3)];
    assert_eq!(c.reapply_gc(&mixed), 1);
    assert_eq!(output_of(&c, "r1"), stub(3));
}
