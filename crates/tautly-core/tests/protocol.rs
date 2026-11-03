use std::path::PathBuf;
use std::time::{Duration, UNIX_EPOCH};

use serde_json::json;
use tautly_context::{Breakdown, ContextStatus, MutationReason};
use tautly_core::{
    BudgetScope, Command, DiffFile, DiffPreview, DiffStatus, EffectView, ErrorKind, Event,
    FinishReason, MemAnchor, MemoryChange, PROTOCOL_VERSION, ToolMetaView, UsageRole,
};
use tautly_memory::{MemoryEntry, MemoryKind};
use tautly_provider::Quota;
use tautly_tools::{AppliedEffect, LineRange, Shown, ToolMeta};

fn memory_change() -> MemoryChange {
    MemoryChange {
        upsert: Some(MemoryEntry {
            id: "build-cmd".into(),
            title: "How to build".into(),
            kind: MemoryKind::Command,
            anchors: vec![MemAnchor {
                path: PathBuf::from("Cargo.toml"),
                symbol: None,
                hash: "abc123".into(),
            }],
            created: UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            verified: None,
            body: "cargo build --workspace".into(),
            stale: false,
        }),
        delete: vec!["old-entry".into()],
    }
}

/// One of every event variant. The match below fails to compile when a variant is added
/// without a sample here.
fn every_event() -> Vec<Event> {
    let events = vec![
        Event::TurnStarted,
        Event::TextDelta {
            text: "Hello".into(),
        },
        Event::ReasoningDelta {
            text: "thinking".into(),
        },
        Event::ToolCallProposed {
            call_id: "call_1".into(),
            tool: "edit".into(),
            args: json!({"path": "src/lib.rs", "old": "a", "new": "b"}),
        },
        Event::ToolApprovalRequested {
            request_id: "req_1".into(),
            call_id: "call_1".into(),
            tool: "bash".into(),
            args: json!({"command": "cargo test"}),
            reason: "commands need approval in ask mode".into(),
            diff: None,
        },
        Event::ToolStarted {
            call_id: "call_1".into(),
            tool: "bash".into(),
        },
        Event::ToolOutputDelta {
            call_id: "call_1".into(),
            text: "running 3 tests\n".into(),
        },
        Event::ToolFinished {
            call_id: "call_1".into(),
            tool: "edit".into(),
            output: "edited src/lib.rs".into(),
            is_error: false,
            blocked: false,
            effects: vec![
                EffectView::Read {
                    path: "/p/src/main.rs".into(),
                    range: LineRange {
                        start: Some(1),
                        end: Some(40),
                    },
                    shown: Shown::Unchanged { since: 3 },
                },
                EffectView::Edited {
                    path: "/p/src/lib.rs".into(),
                    created: false,
                    post_hash: "a".repeat(64),
                },
                EffectView::Deleted {
                    path: "/p/old.rs".into(),
                },
                EffectView::Exec { may_write: true },
            ],
            meta: ToolMetaView {
                exit_code: Some(1),
                error_kind: Some("stale".into()),
            },
            diff: Some(DiffPreview {
                unified: "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-a\n+b\n".into(),
                lines: 5,
                truncated: false,
            }),
        },
        Event::Usage {
            role: UsageRole::Side,
            tokens_in: 1200,
            tokens_out: 300,
            cache_read: 900,
            cache_write: 0,
            cost_usd: Some(0.0042),
            quota: Some(Quota {
                remaining_requests: Some(99),
                remaining_tokens: None,
                reset_at: Some(UNIX_EPOCH + Duration::from_secs(1_700_000_060)),
            }),
        },
        Event::ContextStatus {
            used: 5000,
            budget: 200_000,
            breakdown: Breakdown {
                system: 1500,
                tools: 1000,
                history: 2500,
                ..Default::default()
            },
            warnings: vec!["compacting".into()],
        },
        Event::Compacted {
            removed_turns: 12,
            summary: "did things".into(),
            reason: MutationReason::Forced,
        },
        Event::ContextCollected {
            elided_results: 4,
            tokens_freed: 3200,
            reason: MutationReason::ColdCache,
        },
        Event::MemoryProposed {
            proposal_id: "prop_1".into(),
            change: memory_change(),
        },
        Event::SessionChanged {
            session_id: "01J0SESSION".into(),
        },
        Event::TurnFinished {
            reason: FinishReason::MaxTurns,
        },
        Event::Error {
            kind: ErrorKind::ProviderTransient,
            message: "429 from provider".into(),
            retryable: true,
            hint: Some("retrying in 2s".into()),
        },
        Event::SessionDiff {
            turn: Some(2),
            files: vec![
                DiffFile {
                    path: "src/lib.rs".into(),
                    status: DiffStatus::Modified,
                    added: 1,
                    removed: 1,
                },
                DiffFile {
                    path: "new.txt".into(),
                    status: DiffStatus::Added,
                    added: 1,
                    removed: 0,
                },
            ],
            unified: "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-a\n+b\n".into(),
            truncated: false,
        },
        Event::Warning {
            message: "non-UTF-8 path not tracked".into(),
        },
    ];
    for e in &events {
        match e {
            Event::TurnStarted
            | Event::TextDelta { .. }
            | Event::ReasoningDelta { .. }
            | Event::ToolCallProposed { .. }
            | Event::ToolApprovalRequested { .. }
            | Event::ToolStarted { .. }
            | Event::ToolOutputDelta { .. }
            | Event::ToolFinished { .. }
            | Event::Usage { .. }
            | Event::ContextStatus { .. }
            | Event::Compacted { .. }
            | Event::ContextCollected { .. }
            | Event::MemoryProposed { .. }
            | Event::SessionChanged { .. }
            | Event::TurnFinished { .. }
            | Event::Error { .. }
            | Event::SessionDiff { .. }
            | Event::Warning { .. }
            | Event::Unknown => {}
        }
    }
    events
}

fn every_command() -> Vec<Command> {
    let commands = vec![
        Command::SendMessage {
            text: "fix the bug".into(),
        },
        Command::Approve {
            request_id: "req_1".into(),
        },
        Command::Deny {
            request_id: "req_1".into(),
            reason: Some("too risky".into()),
        },
        Command::Deny {
            request_id: "req_2".into(),
            reason: None,
        },
        Command::Cancel,
        Command::SetBudget {
            scope: BudgetScope::Session,
            max_usd: Some(2.5),
            max_tokens: None,
        },
        Command::SetBudget {
            scope: BudgetScope::Turn,
            max_usd: None,
            max_tokens: Some(50_000),
        },
        Command::Compact,
        Command::Uncompact,
        Command::Pin {
            text: "never touch generated/".into(),
        },
        Command::Unpin {
            text: "never touch generated/".into(),
        },
        Command::ApproveMemory {
            proposal_id: "prop_1".into(),
        },
        Command::RejectMemory {
            proposal_id: "prop_1".into(),
        },
        Command::ConsolidateMemory,
        Command::Fork,
        Command::Diff { turn: None },
        Command::Diff { turn: Some(3) },
        Command::Shutdown,
    ];
    for c in &commands {
        match c {
            Command::SendMessage { .. }
            | Command::Approve { .. }
            | Command::Deny { .. }
            | Command::Cancel
            | Command::SetBudget { .. }
            | Command::Compact
            | Command::Uncompact
            | Command::Pin { .. }
            | Command::Unpin { .. }
            | Command::ApproveMemory { .. }
            | Command::RejectMemory { .. }
            | Command::ConsolidateMemory
            | Command::Fork
            | Command::Diff { .. }
            | Command::Shutdown => {}
        }
    }
    commands
}

#[test]
fn event_json_shape_is_stable() {
    let events = every_event();
    // Every event has a distinct snake_case `type`; only `session_changed` names a session
    // (the transport adds the session id to the others).
    let types: Vec<String> = events
        .iter()
        .map(|e| {
            serde_json::to_value(e).unwrap()["type"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    let mut unique = types.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), events.len(), "{types:?}");
    for e in events
        .iter()
        .filter(|e| !matches!(e, Event::SessionChanged { .. }))
    {
        assert!(serde_json::to_value(e).unwrap().get("session_id").is_none());
    }
    insta::assert_json_snapshot!("events", events);
}

#[test]
fn approval_events_and_commands_carry_request_ids() {
    let v = serde_json::to_value(&every_event()[4]).unwrap();
    assert_eq!(v["type"], "tool_approval_requested");
    assert_eq!(v["request_id"], "req_1");
    assert_eq!(v["call_id"], "call_1");

    let v = serde_json::to_value(Command::Deny {
        request_id: "r".into(),
        reason: Some("no".into()),
    })
    .unwrap();
    assert_eq!(
        v,
        json!({"type": "deny", "request_id": "r", "reason": "no"})
    );
    let approve: Command =
        serde_json::from_value(json!({"type": "approve", "request_id": "r"})).unwrap();
    assert_eq!(
        approve,
        Command::Approve {
            request_id: "r".into()
        }
    );
}

#[test]
fn events_roundtrip() {
    for e in every_event() {
        let text = serde_json::to_string(&e).unwrap();
        let back: Event = serde_json::from_str(&text).unwrap();
        assert_eq!(back, e, "{text}");
    }
}

#[test]
fn command_roundtrip() {
    for c in every_command() {
        let text = serde_json::to_string(&c).unwrap();
        let back: Command = serde_json::from_str(&text).unwrap();
        assert_eq!(back, c, "{text}");
    }
}

#[test]
fn command_json_shape_is_stable() {
    insta::assert_json_snapshot!("commands", every_command());
}

#[test]
fn commands_accept_missing_optional_fields_and_reject_unknown_types() {
    let c: Command = serde_json::from_str(r#"{"type":"deny","request_id":"r"}"#).unwrap();
    assert_eq!(
        c,
        Command::Deny {
            request_id: "r".into(),
            reason: None
        }
    );
    let c: Command =
        serde_json::from_str(r#"{"type":"set_budget","scope":"turn","max_usd":1.5}"#).unwrap();
    assert_eq!(
        c,
        Command::SetBudget {
            scope: BudgetScope::Turn,
            max_usd: Some(1.5),
            max_tokens: None
        }
    );
    assert!(serde_json::from_str::<Command>(r#"{"type":"explode"}"#).is_err());
    assert!(
        serde_json::from_str::<Command>(r#"{"type":"approve"}"#).is_err(),
        "request_id is required"
    );
}

#[test]
fn finish_reasons_and_error_kinds_use_snake_case() {
    let reasons = [
        (FinishReason::Completed, "completed"),
        (FinishReason::Cancelled, "cancelled"),
        (FinishReason::Budget, "budget"),
        (FinishReason::MaxTurns, "max_turns"),
        (FinishReason::Timeout, "timeout"),
        (FinishReason::Error, "error"),
    ];
    for (r, s) in reasons {
        assert_eq!(serde_json::to_value(r).unwrap(), s);
    }
    let kinds = [
        (ErrorKind::ProviderTransient, "provider_transient"),
        (ErrorKind::ProviderFatal, "provider_fatal"),
        (ErrorKind::ContextOverflow, "context_overflow"),
        (ErrorKind::Tool, "tool"),
        (ErrorKind::Policy, "policy"),
        (ErrorKind::Internal, "internal"),
    ];
    for (k, s) in kinds {
        assert_eq!(serde_json::to_value(k).unwrap(), s);
    }
    assert_eq!(serde_json::to_value(UsageRole::Main).unwrap(), "main");
}

#[test]
fn protocol_version_is_1_0() {
    assert_eq!(PROTOCOL_VERSION, (1, 0));
}

#[cfg(unix)]
#[test]
fn non_utf8_path_serializes_lossily_instead_of_failing() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let path = PathBuf::from(OsStr::from_bytes(b"/tmp/caf\xe9/\xff.txt"));
    assert!(path.to_str().is_none());
    // `PathBuf`'s own serde impl fails on this path, which is why events do not use it.
    assert!(serde_json::to_string(&path).is_err());

    let event = Event::ToolFinished {
        call_id: "c".into(),
        tool: "write".into(),
        output: String::new(),
        is_error: false,
        blocked: false,
        effects: vec![
            EffectView::Edited {
                path: path.clone(),
                created: true,
                post_hash: "b".repeat(64),
            },
            EffectView::Read {
                path: path.clone(),
                range: LineRange::ALL,
                shown: Shown::Full,
            },
            EffectView::Deleted { path },
        ],
        meta: ToolMetaView::default(),
        diff: None,
    };
    let v = serde_json::to_value(&event).unwrap();
    assert_eq!(v["effects"][0]["path"], "/tmp/caf\u{fffd}/\u{fffd}.txt");
    assert_eq!(v["effects"][1]["path"], "/tmp/caf\u{fffd}/\u{fffd}.txt");
    assert_eq!(v["effects"][2]["path"], "/tmp/caf\u{fffd}/\u{fffd}.txt");
    // It reads back as the lossy text, not as the original bytes.
    let back: Event = serde_json::from_value(v).unwrap();
    let Event::ToolFinished { effects, .. } = back else {
        panic!()
    };
    assert_eq!(
        effects[0],
        EffectView::Edited {
            path: PathBuf::from("/tmp/caf\u{fffd}/\u{fffd}.txt"),
            created: true,
            post_hash: "b".repeat(64),
        }
    );
}

#[test]
fn effect_views_drop_file_contents() {
    let hash = blake3::hash(b"x");
    let edited = AppliedEffect::Edited {
        path: "/p/a.rs".into(),
        pre_image: Some(b"secret contents".to_vec()),
        pre_mode: Some(0o644),
        post_hash: hash,
    };
    let created = AppliedEffect::Edited {
        path: "/p/new.rs".into(),
        pre_image: None,
        pre_mode: None,
        post_hash: hash,
    };
    let deleted = AppliedEffect::Deleted {
        path: "/p/gone.rs".into(),
        pre_image: b"more secret".to_vec(),
        pre_mode: 0o600,
    };
    let read = AppliedEffect::Read {
        path: "/p/r.rs".into(),
        range: LineRange {
            start: Some(2),
            end: None,
        },
        file_hash: hash,
        shown: Shown::Diff { since: 4 },
    };
    let exec = AppliedEffect::Exec { may_write: false };

    assert_eq!(
        EffectView::from(&edited),
        EffectView::Edited {
            path: "/p/a.rs".into(),
            created: false,
            post_hash: hash.to_hex().to_string(),
        }
    );
    assert_eq!(
        EffectView::from(&created),
        EffectView::Edited {
            path: "/p/new.rs".into(),
            created: true,
            post_hash: hash.to_hex().to_string(),
        }
    );
    assert_eq!(
        EffectView::from(&deleted),
        EffectView::Deleted {
            path: "/p/gone.rs".into()
        }
    );
    assert_eq!(
        EffectView::from(&read),
        EffectView::Read {
            path: "/p/r.rs".into(),
            range: LineRange {
                start: Some(2),
                end: None
            },
            shown: Shown::Diff { since: 4 },
        }
    );
    assert_eq!(
        EffectView::from(&exec),
        EffectView::Exec { may_write: false }
    );

    let text =
        serde_json::to_string(&[EffectView::from(&edited), EffectView::from(&deleted)]).unwrap();
    assert!(
        !text.contains("secret") && !text.contains("pre_image"),
        "{text}"
    );
}

#[test]
fn tool_meta_view_copies_meta() {
    let mut meta = ToolMeta::default();
    meta.exit_code = Some(2);
    meta.error_kind = Some("not_found");
    assert_eq!(
        ToolMetaView::from(&meta),
        ToolMetaView {
            exit_code: Some(2),
            error_kind: Some("not_found".into())
        }
    );
    assert_eq!(
        ToolMetaView::from(&ToolMeta::default()),
        ToolMetaView::default()
    );
}

#[test]
fn context_status_event_from_status() {
    let status = ContextStatus {
        used: 10,
        budget: 100,
        breakdown: Breakdown {
            history: 10,
            ..Default::default()
        },
        warnings: vec!["pinned items exceed 10% of the window".into()],
        flat_tokens: 3,
        cached_prefix: 7,
    };
    assert_eq!(
        Event::context_status(&status),
        Event::ContextStatus {
            used: 10,
            budget: 100,
            breakdown: status.breakdown,
            warnings: status.warnings.clone(),
        }
    );
}

#[test]
fn unknown_events_and_values_read_as_unknown_not_as_errors() {
    use serde_json::json;
    // A newer engine's event type.
    let e: Event = serde_json::from_value(json!({"type": "hologram", "x": 1})).unwrap();
    assert_eq!(e, Event::Unknown);
    // Extra fields on a known event are ignored.
    let e: Event =
        serde_json::from_value(json!({"type": "text_delta", "text": "hi", "lang": "en"})).unwrap();
    assert_eq!(e, Event::TextDelta { text: "hi".into() });
    // New values of the wire enums.
    let e: Event = serde_json::from_value(json!({
        "type": "error", "kind": "quota_exhausted", "message": "m", "retryable": false, "hint": null
    }))
    .unwrap();
    assert!(matches!(
        e,
        Event::Error {
            kind: ErrorKind::Unknown,
            ..
        }
    ));
    let e: Event =
        serde_json::from_value(json!({"type": "turn_finished", "reason": "refused"})).unwrap();
    assert_eq!(
        e,
        Event::TurnFinished {
            reason: FinishReason::Unknown
        }
    );
    let e: Event = serde_json::from_value(json!({
        "type": "usage", "role": "judge", "tokens_in": 1, "tokens_out": 2,
        "cache_read": 0, "cache_write": 0, "cost_usd": null, "quota": null
    }))
    .unwrap();
    assert!(matches!(
        e,
        Event::Usage {
            role: UsageRole::Unknown,
            ..
        }
    ));
    let e: Event = serde_json::from_value(json!({
        "type": "tool_finished", "call_id": "c", "tool": "t", "output": "", "is_error": false,
        "blocked": false, "meta": {}, "effects": [{"type": "moved", "from": "a", "to": "b"}, {"type": "exec", "may_write": true}]
    }))
    .unwrap();
    let Event::ToolFinished { effects, .. } = e else {
        panic!()
    };
    assert_eq!(
        effects,
        [EffectView::Unknown, EffectView::Exec { may_write: true }]
    );
    // A command the engine does not know is still an error: it cannot be ignored.
    assert!(serde_json::from_value::<Command>(json!({"type": "explode"})).is_err());
}

#[test]
fn timestamps_are_unix_seconds_on_the_wire() {
    let events = every_event();
    let usage = serde_json::to_value(&events[8]).unwrap();
    assert_eq!(usage["quota"]["reset_at"], 1_700_000_060_u64);
    let proposed = serde_json::to_value(&events[12]).unwrap();
    assert_eq!(proposed["change"]["upsert"]["created"], 1_700_000_000_u64);
    assert_eq!(
        proposed["change"]["upsert"]["verified"],
        serde_json::Value::Null
    );
    assert!(proposed["change"]["upsert"].get("stale").is_none());

    // They read back, and sub-second parts do not survive the wire.
    for e in &events {
        let back: Event = serde_json::from_value(serde_json::to_value(e).unwrap()).unwrap();
        assert_eq!(&back, e);
    }
    let change = MemoryChange {
        upsert: Some(MemoryEntry {
            created: UNIX_EPOCH + Duration::from_millis(1_700_000_000_999),
            verified: Some(UNIX_EPOCH + Duration::from_secs(1_700_000_100)),
            ..match &events[12] {
                Event::MemoryProposed { change, .. } => change.upsert.clone().unwrap(),
                _ => unreachable!(),
            }
        }),
        delete: vec![],
    };
    let v = serde_json::to_value(Event::MemoryProposed {
        proposal_id: "p".into(),
        change,
    })
    .unwrap();
    assert_eq!(v["change"]["upsert"]["created"], 1_700_000_000_u64);
    assert_eq!(v["change"]["upsert"]["verified"], 1_700_000_100_u64);
    // A client that omits optional timestamps is understood.
    let e: Event = serde_json::from_value(json!({
        "type": "usage", "role": "main", "tokens_in": 1, "tokens_out": 1,
        "cache_read": 0, "cache_write": 0
    }))
    .unwrap();
    assert!(matches!(
        e,
        Event::Usage {
            quota: None,
            cost_usd: None,
            ..
        }
    ));
}
