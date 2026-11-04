//! The pure mappings of the ACP bridge (spec 7.11.9).

use std::path::Path;

use serde_json::json;
use tautly_acp::map::{
    self, MAX_DIFF_TEXT, MAX_OUTPUT_TEXT, Slash, diff_content, slash, stop_reason, tool_kind,
    tool_title,
};
use tautly_acp::permission::{self, Answer};
use tautly_acp::schema::{
    ContentBlock, EmbeddedResource, McpServer, StopReason, ToolCallContent, ToolKind,
};
use tautly_core::{Command, FinishReason, ServerSpec};

#[test]
fn tool_kinds_follow_the_spec_table() {
    for (tool, kind) in [
        ("read", ToolKind::Read),
        ("outline", ToolKind::Read),
        ("symbol", ToolKind::Read),
        ("grep", ToolKind::Search),
        ("glob", ToolKind::Search),
        ("find", ToolKind::Search),
        ("refs", ToolKind::Search),
        ("recall", ToolKind::Search),
        ("edit", ToolKind::Edit),
        ("write", ToolKind::Edit),
        ("bash", ToolKind::Execute),
        ("web_fetch", ToolKind::Fetch),
        ("pin", ToolKind::Think),
        ("state", ToolKind::Think),
        ("remember", ToolKind::Think),
        ("mcp__github__issues", ToolKind::Other),
        ("something_new", ToolKind::Other),
    ] {
        assert_eq!(tool_kind(tool), kind, "{tool}");
    }
}

#[test]
fn stop_reason_mapping() {
    assert_eq!(
        stop_reason(FinishReason::Completed),
        Some(StopReason::EndTurn)
    );
    assert_eq!(
        stop_reason(FinishReason::MaxTurns),
        Some(StopReason::MaxTurnRequests)
    );
    assert_eq!(
        stop_reason(FinishReason::Cancelled),
        Some(StopReason::Cancelled)
    );
    assert_eq!(stop_reason(FinishReason::Budget), Some(StopReason::EndTurn));
    assert_eq!(
        stop_reason(FinishReason::Timeout),
        Some(StopReason::EndTurn)
    );
    assert_eq!(stop_reason(FinishReason::Error), None);
    assert_eq!(
        stop_reason(FinishReason::Unknown),
        Some(StopReason::EndTurn)
    );
}

#[test]
fn diff_content_has_old_and_new_text_up_to_256_kb() {
    let p = Path::new("/r/a.rs");
    assert_eq!(
        diff_content(p, Some("a\n"), "b\n"),
        Some(ToolCallContent::Diff {
            path: p.into(),
            old_text: Some("a\n".into()),
            new_text: "b\n".into(),
        })
    );
    assert_eq!(
        diff_content(p, None, "new\n"),
        Some(ToolCallContent::Diff {
            path: p.into(),
            old_text: None,
            new_text: "new\n".into(),
        })
    );
    let big = "x".repeat(MAX_DIFF_TEXT + 1);
    let edge = "x".repeat(MAX_DIFF_TEXT);
    assert_eq!(diff_content(p, Some(&big), "b"), None);
    assert_eq!(diff_content(p, Some("a"), &big), None);
    assert!(diff_content(p, Some(&edge), &edge).is_some());
}

#[test]
fn titles_name_the_target() {
    assert_eq!(
        tool_title("read", &json!({"path": "src/a.rs"})),
        "read: src/a.rs"
    );
    assert_eq!(
        tool_title("bash", &json!({"command": "cargo test\necho done"})),
        "bash: cargo test"
    );
    assert_eq!(
        tool_title("grep", &json!({"pattern": "fn main"})),
        "grep: fn main"
    );
    assert_eq!(
        tool_title("web_fetch", &json!({"url": "https://example.com"})),
        "web_fetch: https://example.com"
    );
    assert_eq!(
        tool_title("recall", &json!({"query": "auth"})),
        "recall: auth"
    );
    assert_eq!(tool_title("state", &json!({})), "state");
    let long = tool_title("bash", &json!({"command": "x".repeat(200)}));
    assert!(long.chars().count() <= "bash: ".len() + 81, "{long}");
    assert!(long.ends_with('…'));
}

#[test]
fn locations_are_absolute() {
    let locs = map::locations(Path::new("/r"), &json!({"path": "src/a.rs"}));
    assert_eq!(locs.len(), 1);
    assert_eq!(locs[0].path, Path::new("/r/src/a.rs"));
    let abs = map::locations(Path::new("/r"), &json!({"path": "/r/b.rs"}));
    assert_eq!(abs[0].path, Path::new("/r/b.rs"));
    assert!(map::locations(Path::new("/r"), &json!({"command": "ls"})).is_empty());
}

#[test]
fn long_output_is_cut_with_a_marker() {
    let ToolCallContent::Content {
        content: ContentBlock::Text { text },
    } = map::output_content(&"y".repeat(MAX_OUTPUT_TEXT * 2))
    else {
        panic!("not text");
    };
    assert!(text.len() <= MAX_OUTPUT_TEXT + 64, "{}", text.len());
    assert!(text.contains("output cut"), "{text}");
}

#[test]
fn prompts_join_text_and_inline_resources() {
    let text = map::prompt_text(&[
        ContentBlock::text("look at"),
        ContentBlock::Resource {
            resource: EmbeddedResource {
                uri: "file:///r/a.rs".into(),
                text: Some("fn a() {}\n```inner```".into()),
                blob: None,
                mime_type: None,
            },
        },
        ContentBlock::ResourceLink {
            uri: "file:///r/b.rs".into(),
            name: "b.rs".into(),
        },
    ])
    .unwrap();
    assert!(text.starts_with("look at"), "{text}");
    assert!(text.contains("file:///r/a.rs\n````\nfn a() {}"), "{text}");
    assert!(text.contains("file:///r/b.rs"), "{text}");
    assert!(
        map::prompt_text(&[ContentBlock::Image {
            data: "AA==".into(),
            mime_type: "image/png".into()
        }])
        .is_err()
    );
}

#[test]
fn slash_commands() {
    assert_eq!(slash("/compact"), Some(Slash::Compact));
    assert_eq!(slash(" /undo 2 "), Some(Slash::Undo(Some(2))));
    assert_eq!(slash("/undo"), Some(Slash::Undo(None)));
    assert_eq!(slash("/diff"), Some(Slash::Diff(None)));
    assert_eq!(slash("/cost"), Some(Slash::Cost));
    assert_eq!(slash("/pin use pnpm"), Some(Slash::Pin("use pnpm".into())));
    assert!(matches!(slash("/pin"), Some(Slash::Usage(_))));
    assert!(matches!(slash("/diff x"), Some(Slash::Usage(_))));
    assert_eq!(slash("/usr/bin is odd"), None);
    assert_eq!(slash("hello"), None);
    let names: Vec<String> = map::available_commands()
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(names, ["compact", "undo", "diff", "cost", "pin"]);
}

#[test]
fn client_mcp_servers_become_specs() {
    let (specs, skipped) = map::client_mcp(vec![
        McpServer::Stdio {
            name: "fs".into(),
            command: "/bin/srv".into(),
            args: vec!["-v".into()],
            env: vec![("TOKEN".into(), "t".into())],
        },
        McpServer::Http {
            name: "web".into(),
            url: "https://h/mcp".into(),
            headers: vec![("Authorization".into(), "Bearer x".into())],
        },
        McpServer::Sse { name: "old".into() },
        McpServer::Http {
            name: "bad".into(),
            url: "ftp://h".into(),
            headers: vec![],
        },
    ]);
    assert_eq!(specs.len(), 2, "{specs:?}");
    assert!(matches!(
        &specs[0],
        (n, ServerSpec::Stdio { command, args, env, cwd: None })
            if n == "fs" && command == "/bin/srv" && args == &["-v"] && env["TOKEN"] == "t"
    ));
    assert!(matches!(&specs[1], (n, ServerSpec::Http { headers, .. })
        if n == "web" && headers["Authorization"] == "Bearer x"));
    assert_eq!(skipped.len(), 2, "{skipped:?}");
    assert!(skipped[0].contains("old"));
}

#[test]
fn preview_diffs_apply_the_unified_diff() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::write(root.join("a.txt"), "one\ntwo\nthree\n").unwrap();
    let unified = "--- a/a.txt\n+++ b/a.txt\n@@ -1,3 +1,3 @@\n one\n-two\n+TWO\n three\n\
                   --- /dev/null\n+++ b/new.txt\n@@ -0,0 +1 @@\n+fresh\n\\ No newline at end of file\n";
    let diffs = map::preview_diffs(&root, unified).unwrap();
    assert_eq!(
        diffs,
        vec![
            ToolCallContent::Diff {
                path: root.join("a.txt"),
                old_text: Some("one\ntwo\nthree\n".into()),
                new_text: "one\nTWO\nthree\n".into(),
            },
            ToolCallContent::Diff {
                path: root.join("new.txt"),
                old_text: None,
                new_text: "fresh".into(),
            },
        ]
    );
    // A diff that no longer applies, or a truncated one, is not reconstructed.
    std::fs::write(root.join("a.txt"), "changed\n").unwrap();
    assert_eq!(map::preview_diffs(&root, unified), None);
    let cut = "--- a/a.txt\n+++ b/a.txt\n@@ -1,3 +1,3 @@\n one\n… 3 more lines (diff truncated)";
    assert_eq!(map::preview_diffs(&root, cut), None);
    assert_eq!(map::preview_diffs(&root, "--- a/../x\n+++ b/../x\n"), None);
}

#[test]
fn permission_answers() {
    let ids = |o: Vec<tautly_acp::schema::PermissionOption>| -> Vec<String> {
        o.into_iter().map(|o| o.option_id).collect()
    };
    assert_eq!(
        ids(permission::tool_options(Some("bash(ls)"))),
        ["allow_once", "allow_always", "reject_once"]
    );
    assert_eq!(
        ids(permission::tool_options(None)),
        ["allow_once", "reject_once"]
    );
    assert_eq!(
        ids(permission::yes_no_options()),
        ["allow_once", "reject_once"]
    );
    let sel = |id: &str| json!({"outcome": {"outcome": "selected", "optionId": id}});
    let rule = Some("bash(ls)");
    assert_eq!(
        permission::answer(Some(&sel("allow_once")), rule),
        Answer::Once
    );
    assert_eq!(
        permission::answer(Some(&sel("allow_always")), rule),
        Answer::Always("bash(ls)".into())
    );
    // Not offered (no rule, or an MCP server approval): denied, no session rule.
    assert_eq!(
        permission::answer(Some(&sel("allow_always")), None),
        Answer::Reject
    );
    assert_eq!(
        permission::answer(Some(&sel("reject_once")), rule),
        Answer::Reject
    );
    assert_eq!(
        permission::answer(Some(&json!({"outcome": {"outcome": "cancelled"}})), rule),
        Answer::Reject
    );
    assert_eq!(permission::answer(None, rule), Answer::Reject);
    assert_eq!(
        permission::approval_command("r1", Answer::Always("bash(ls)".into())),
        Command::Approve {
            request_id: "r1".into(),
            session_rule: Some("bash(ls)".into())
        }
    );
    assert!(matches!(
        permission::approval_command("r1", Answer::Reject),
        Command::Deny { request_id, .. } if request_id == "r1"
    ));
    assert_eq!(
        permission::memory_command("p1", &Answer::Once),
        Command::ApproveMemory {
            proposal_id: "p1".into()
        }
    );
}
