//! Approvals as `session/request_permission` (spec 7.11.9).
//!
//! A tool call's approval offers `allow_once`, `allow_always` (the exact-target session
//! rule, spec 7.11.8; only when one names the call) and `reject_once`; MCP server
//! approvals and memory proposals offer `allow_once` and `reject_once`. A `cancelled`
//! outcome, an option that was not offered (an `allow_always` for an MCP server too: a
//! session rule is for tool calls only) or no answer at all denies.
//!
//! Text in these requests is the model's or the repository's. It is escaped before it
//! goes out (spec 3.4): single-line fields (titles, names, rules) with every control and
//! invisible character as a visible escape, multi-line text (commands, launch lines,
//! memory bodies) escaped and fenced, with long blank runs marked (`map::approval_text`),
//! so a client that renders markdown shows it whole and literally.

use serde_json::{Value, json};
use tautly_core::{Command, MemoryChange};

use crate::map::{approval_text, single_line};
use crate::schema::{
    PermissionOption, PermissionOptionKind, PermissionOutcome, PermissionResponse, ToolCallContent,
    ToolCallUpdate, ToolKind,
};

/// Option id of "allow once".
pub const ALLOW_ONCE: &str = "allow_once";
/// Option id of "allow for this session".
pub const ALLOW_ALWAYS: &str = "allow_always";
/// Option id of "reject".
pub const REJECT_ONCE: &str = "reject_once";

/// How the user answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// Approve this one call.
    Once,
    /// Approve, and allow what this rule matches for the rest of the session.
    Always(String),
    /// Refuse this call.
    Reject,
}

fn option(id: &str, name: String, kind: PermissionOptionKind) -> PermissionOption {
    PermissionOption {
        option_id: id.to_string(),
        name,
        kind,
    }
}

/// The options of a tool call's approval; `rule` is its exact-target session rule.
pub fn tool_options(rule: Option<&str>) -> Vec<PermissionOption> {
    let mut options = vec![option(
        ALLOW_ONCE,
        "Allow once".into(),
        PermissionOptionKind::AllowOnce,
    )];
    if let Some(rule) = rule {
        options.push(option(
            ALLOW_ALWAYS,
            format!("Allow for this session: {}", single_line(rule)),
            PermissionOptionKind::AllowAlways,
        ));
    }
    options.push(option(
        REJECT_ONCE,
        "Reject".into(),
        PermissionOptionKind::RejectOnce,
    ));
    options
}

/// The options of an MCP server approval or a memory proposal.
pub fn yes_no_options() -> Vec<PermissionOption> {
    vec![
        option(ALLOW_ONCE, "Allow".into(), PermissionOptionKind::AllowOnce),
        option(
            REJECT_ONCE,
            "Reject".into(),
            PermissionOptionKind::RejectOnce,
        ),
    ]
}

/// The answer in a `session/request_permission` result (None: no result, e.g. an error
/// or a closed connection), given the rule `allow_always` stood for (None: not offered).
pub fn answer(result: Option<&Value>, rule: Option<&str>) -> Answer {
    let Some(response) =
        result.and_then(|r| serde_json::from_value::<PermissionResponse>(r.clone()).ok())
    else {
        return Answer::Reject;
    };
    match response.outcome {
        PermissionOutcome::Selected { option_id } => match (option_id.as_str(), rule) {
            (ALLOW_ONCE, _) => Answer::Once,
            (ALLOW_ALWAYS, Some(rule)) => Answer::Always(rule.to_string()),
            _ => Answer::Reject,
        },
        PermissionOutcome::Cancelled => Answer::Reject,
    }
}

/// The command answering tool or MCP approval `request_id`.
pub fn approval_command(request_id: &str, answer: Answer) -> Command {
    let request_id = request_id.to_string();
    match answer {
        Answer::Once => Command::Approve {
            request_id,
            session_rule: None,
        },
        Answer::Always(rule) => Command::Approve {
            request_id,
            session_rule: Some(rule),
        },
        Answer::Reject => Command::Deny {
            request_id,
            reason: Some("rejected in the editor".into()),
        },
    }
}

/// The command answering memory proposal `proposal_id`.
pub fn memory_command(proposal_id: &str, answer: &Answer) -> Command {
    let proposal_id = proposal_id.to_string();
    match answer {
        Answer::Reject => Command::RejectMemory { proposal_id },
        Answer::Once | Answer::Always(_) => Command::ApproveMemory { proposal_id },
    }
}

fn params(session_id: &str, call: ToolCallUpdate, options: Vec<PermissionOption>) -> Value {
    json!({"sessionId": session_id, "toolCall": call, "options": options})
}

/// `session/request_permission` params for a tool call.
pub fn tool_request(session_id: &str, call: ToolCallUpdate, rule: Option<&str>) -> Value {
    params(session_id, call, tool_options(rule))
}

/// `session/request_permission` params for a repository MCP server that wants to start.
/// Its tool call id is `mcp:<request id>` (no tool call of the model's).
pub fn mcp_request(
    session_id: &str,
    request_id: &str,
    server: &str,
    source: &str,
    launch: &str,
) -> Value {
    let call = ToolCallUpdate {
        tool_call_id: format!("mcp:{request_id}"),
        // `server` and `launch` come escaped from the engine (`printable`).
        title: Some(format!(
            "Start MCP server `{server}` declared by the repository ({})",
            single_line(source)
        )),
        kind: Some(ToolKind::Execute),
        content: Some(vec![ToolCallContent::text(approval_text(launch))]),
        ..ToolCallUpdate::default()
    };
    params(session_id, call, yes_no_options())
}

/// `session/request_permission` params for a memory proposal; its tool call id is
/// `memory:<proposal id>`.
pub fn memory_request(session_id: &str, proposal_id: &str, change: &MemoryChange) -> Value {
    let title = match &change.upsert {
        Some(entry) => format!("Save to project memory: {}", single_line(&entry.title)),
        None => format!(
            "Delete from project memory: {}",
            single_line(&change.delete.join(", "))
        ),
    };
    let mut text = String::new();
    if let Some(entry) = &change.upsert {
        text.push_str(&format!(
            "[{}] {}\n\n{}\n",
            entry.kind.as_str(),
            entry.title,
            entry.body
        ));
    }
    if !change.delete.is_empty() {
        text.push_str(&format!("deletes: {}\n", change.delete.join(", ")));
    }
    let call = ToolCallUpdate {
        tool_call_id: format!("memory:{proposal_id}"),
        title: Some(title),
        kind: Some(ToolKind::Think),
        content: Some(vec![ToolCallContent::text(approval_text(&text))]),
        ..ToolCallUpdate::default()
    };
    params(session_id, call, yes_no_options())
}
