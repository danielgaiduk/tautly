use std::path::PathBuf;
use std::time::{Duration, UNIX_EPOCH};

use serde_json::json;
use tautly_context::{Breakdown, ContextStatus, MutationReason};
use tautly_core::{
    BudgetScope, Command, DiffFile, DiffPreview, DiffStatus, EffectView, ErrorKind, Event,
    FinishReason, MaterializeReport, McpState, MemAnchor, MemoryChange, PROTOCOL_VERSION, Savings,
    ToolMetaView, UsageRole,
};
use tautly_memory::{MemoryEntry, MemoryKind};
use tautly_provider::{ModelId, Quota};
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
            tags: vec![],
            links: vec![],
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
                saved: None,
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
            estimated: false,
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
        Event::CompactFinished {
            applied: true,
            tokens_freed: 2900,
        },
        Event::MemoryProposed {
            proposal_id: "prop_1".into(),
            change: memory_change(),
            replaces: vec![MemoryEntry {
                id: "old-entry".into(),
                title: "Building".into(),
                kind: MemoryKind::Command,
                anchors: vec![],
                created: UNIX_EPOCH + Duration::from_secs(1_690_000_000),
                verified: Some(UNIX_EPOCH + Duration::from_secs(1_695_000_000)),
                body: "make all".into(),
                tags: vec!["build".into()],
                links: vec![],
                stale: false,
            }],
        },
        Event::SessionChanged {
            session_id: "01J0SESSION".into(),
        },
        Event::TurnReceipt {
            turn: 3,
            model: ModelId::from("anthropic/claude-opus-5-5"),
            tokens_in: 1200,
            tokens_out: 300,
            cache_read: 4800,
            cache_write: 200,
            side_tokens: 150,
            cost_usd: Some(0.0125),
            wall_ms: 4200,
            cache_hit_ratio: Some(0.8),
            steps: 2,
            tool_calls: 3,
            saved: Savings {
                index: 900,
                diff_reads: 120,
                context_gc: 3000,
                filters: 0,
                cache_read_tokens: 4800,
                cache_usd: Some(0.0126),
                total_usd: Some(0.0168),
            },
            estimated: true,
        },
        Event::TurnFinished {
            reason: FinishReason::MaxTurns,
            evidence: None,
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
        Event::ModelChanged {
            model: ModelId::from("compat:local/llama"),
            provider: "compat:local".into(),
        },
        Event::Warning {
            message: "non-UTF-8 path not tracked".into(),
        },
        Event::McpServerApprovalRequested {
            request_id: "approval-3".into(),
            server: "github".into(),
            source: "project".into(),
            launch: "stdio: npx -y @modelcontextprotocol/server-github (env: GITHUB_TOKEN)".into(),
        },
        Event::McpServerStatus {
            server: "github".into(),
            state: McpState::Ready,
            tools: 12,
            message: None,
        },
        Event::Undone {
            turn: 2,
            restored: vec!["src/lib.rs".into()],
            deleted: vec!["new.txt".into()],
            dry_run: false,
            backup_ref: Some("refs/tautly/checkpoints/01J0/undo-7".into()),
            note: Some("ignored files and submodules are not restored".into()),
        },
        Event::Rewound {
            at: 3,
            branch: "01JBRANCH".into(),
            report: MaterializeReport {
                restored: vec!["src/lib.rs".into()],
                deleted: vec!["new.txt".into()],
                conflicts: vec![],
                blocked: vec!["vendor/x".into()],
                dry_run: false,
                applied: true,
                backup: Some("git".into()),
                backup_ref: Some("refs/tautly/checkpoints/01J0/undo-8".into()),
                note: None,
            },
        },
        Event::Switched {
            branch: "01J0".into(),
            report: MaterializeReport {
                conflicts: vec!["notes.md".into()],
                dry_run: true,
                ..MaterializeReport::default()
            },
        },
        Event::Replayed {
            offset: 7,
            t_ms: 1_793_795_696_000,
            event: Box::new(Event::UserMessage {
                text: "fix the bug".into(),
            }),
        },
        Event::ReplayFinished {
            session_id: "01J0".into(),
            events: 42,
            cancelled: false,
        },
        Event::UserMessage {
            text: "fix the bug".into(),
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
            | Event::CompactFinished { .. }
            | Event::MemoryProposed { .. }
            | Event::SessionChanged { .. }
            | Event::TurnReceipt { .. }
            | Event::TurnFinished { .. }
            | Event::Error { .. }
            | Event::SessionDiff { .. }
            | Event::ModelChanged { .. }
            | Event::Warning { .. }
            | Event::McpServerApprovalRequested { .. }
            | Event::McpServerStatus { .. }
            | Event::Undone { .. }
            | Event::Rewound { .. }
            | Event::Switched { .. }
            | Event::Replayed { .. }
            | Event::ReplayFinished { .. }
            | Event::UserMessage { .. }
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
            session_rule: None,
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
        Command::Fork {
            at: None,
            label: None,
        },
        Command::Fork {
            at: Some(5),
            label: Some("try-b".into()),
        },
        Command::Diff { turn: None },
        Command::Diff { turn: Some(3) },
        Command::SetModel {
            model: ModelId::from("openai/gpt-5"),
        },
        Command::Undo {
            turn: None,
            dry_run: false,
        },
        Command::Undo {
            turn: Some(2),
            dry_run: true,
        },
        Command::Rewind {
            at: 3,
            label: Some("try-b".into()),
            dry_run: true,
            overwrite: vec![],
        },
        Command::Rewind {
            at: 2,
            label: None,
            dry_run: false,
            overwrite: vec!["notes.md".into()],
        },
        Command::Switch {
            session_id: "01J0".into(),
            dry_run: false,
            overwrite: vec![],
        },
        Command::Label {
            label: "main".into(),
        },
        Command::Replay {
            session_id: None,
            speed: 1.0,
            from_step: None,
        },
        Command::Replay {
            session_id: Some("01J0".into()),
            speed: 0.0,
            from_step: Some(4),
        },
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
            | Command::Fork { .. }
            | Command::Diff { .. }
            | Command::SetModel { .. }
            | Command::Undo { .. }
            | Command::Rewind { .. }
            | Command::Switch { .. }
            | Command::Label { .. }
            | Command::Replay { .. }
            | Command::Shutdown
            | Command::Unknown => {}
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
    // `replay_finished` names the session it replayed, not the one it is sent on.
    for e in events.iter().filter(|e| {
        !matches!(
            e,
            Event::SessionChanged { .. } | Event::ReplayFinished { .. }
        )
    }) {
        assert!(serde_json::to_value(e).unwrap().get("session_id").is_none());
    }
    insta::assert_json_snapshot!("events", events);
}

#[test]
fn turn_finished_with_evidence_has_a_stable_shape() {
    let e = Event::TurnFinished {
        reason: FinishReason::Completed,
        evidence: Some(tautly_core::TestEvidence {
            command: "cargo test -p tautly-core".into(),
            exit_code: Some(101),
            passed: Some(41),
            failed: Some(2),
            step: 3,
            write_gen: 12,
            stale: true,
            edits_since: 2,
            execs_since: 1,
        }),
    };
    insta::assert_json_snapshot!("turn_finished_with_evidence", e);
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
            request_id: "r".into(),
            session_rule: None,
        }
    );
    // "Allow for this session": on the wire, never in the session log.
    let always = Command::Approve {
        request_id: "r".into(),
        session_rule: Some("bash(cargo test*)".into()),
    };
    let v = serde_json::to_value(&always).unwrap();
    assert_eq!(
        v,
        json!({"type": "approve", "request_id": "r", "session_rule": "bash(cargo test*)"})
    );
    assert_eq!(serde_json::from_value::<Command>(v).unwrap(), always);
    assert_eq!(
        serde_json::to_value(always.for_log()).unwrap(),
        json!({"type": "approve", "request_id": "r"})
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
fn commands_accept_missing_optional_fields_and_read_unknown_types_as_unknown() {
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
    assert_eq!(
        serde_json::from_str::<Command>(r#"{"type":"explode","power":9}"#).unwrap(),
        Command::Unknown
    );
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
            error_kind: Some("not_found".into()),
            saved: None,
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
    // A new MCP server state.
    let e: Event = serde_json::from_value(json!({
        "type": "mcp_server_status", "server": "s", "state": "sleeping", "tools": 0, "message": null
    }))
    .unwrap();
    assert!(matches!(
        e,
        Event::McpServerStatus {
            state: McpState::Unknown,
            ..
        }
    ));
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
            reason: FinishReason::Unknown,
            evidence: None,
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
    // A command or budget scope the engine does not know reads as `Unknown` (the engine
    // answers it with an error, and a log holding it stays readable).
    assert_eq!(
        serde_json::from_value::<Command>(json!({"type": "explode"})).unwrap(),
        Command::Unknown
    );
    assert_eq!(
        serde_json::from_value::<Command>(
            json!({"type": "set_budget", "scope": "week", "max_usd": 1.0, "max_tokens": null})
        )
        .unwrap(),
        Command::SetBudget {
            scope: BudgetScope::Unknown,
            max_usd: Some(1.0),
            max_tokens: None,
        }
    );
    // A read effect shown in a way this version does not know.
    let e: Event = serde_json::from_value(json!({
        "type": "tool_finished", "call_id": "c", "tool": "t", "output": "", "is_error": false,
        "blocked": false, "meta": {},
        "effects": [{"type": "read", "path": "a", "range": {"start": null, "end": null},
                     "shown": {"type": "folded", "lines": 3}}]
    }))
    .unwrap();
    let Event::ToolFinished { effects, .. } = e else {
        panic!()
    };
    assert_eq!(
        effects,
        [EffectView::Read {
            path: "a".into(),
            range: LineRange::ALL,
            shown: Shown::Unknown,
        }]
    );
    // A memory kind this version does not know reads as a fact.
    let e: Event = serde_json::from_value(json!({
        "type": "memory_proposed", "proposal_id": "p",
        "change": {"upsert": {"id": "x", "title": "t", "kind": "pattern", "anchors": [],
                              "created": 0, "body": "b"}, "delete": []}
    }))
    .unwrap();
    let Event::MemoryProposed { change, .. } = e else {
        panic!()
    };
    assert_eq!(change.upsert.unwrap().kind, MemoryKind::Fact);
}

#[test]
fn timestamps_are_unix_seconds_on_the_wire() {
    let events = every_event();
    let usage = serde_json::to_value(&events[8]).unwrap();
    assert_eq!(usage["quota"]["reset_at"], 1_700_000_060_u64);
    let proposed = serde_json::to_value(&events[13]).unwrap();
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
            ..match &events[13] {
                Event::MemoryProposed { change, .. } => change.upsert.clone().unwrap(),
                _ => unreachable!(),
            }
        }),
        delete: vec![],
    };
    let v = serde_json::to_value(Event::MemoryProposed {
        proposal_id: "p".into(),
        change,
        replaces: vec![],
    })
    .unwrap();
    assert_eq!(v["change"]["upsert"]["created"], 1_700_000_000_u64);
    assert_eq!(v["change"]["upsert"]["verified"], 1_700_000_100_u64);
    // Nothing replaced: no `replaces`, and an event without it reads back as none.
    assert!(v.get("replaces").is_none(), "{v}");
    let back: Event = serde_json::from_value(v).unwrap();
    assert!(matches!(back, Event::MemoryProposed { replaces, .. } if replaces.is_empty()));
    assert_eq!(proposed["replaces"][0]["created"], 1_690_000_000_u64);
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
