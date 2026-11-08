//! Approval prompts (spec 7.11.8): `allow? [y]es [n]o [a]lways this session (<rule>)`.
//!
//! `y` approves once, `n` denies, `a` approves and asks the engine to allow calls the
//! exact-target rule matches for the rest of the session, and `a <rule>` sends the user's
//! own pattern instead. The engine accepts a session rule only when it matches the pending
//! call (else it approves once and warns); deny rules and protected paths still win.
//! Memory proposals and MCP server approvals are plain `y`/`n` questions.

use std::path::Path;

use serde_json::Value;
use tautly_core::printable;

/// The exact-target session rule for a call, as `a` sends it: `edit(<path>)`,
/// `write(<path>)`, `bash(<command>)`, `web_fetch(<host>)` or `mcp__<server>__<tool>`,
/// built by the engine's policy parser ([`tautly_core::exact_rule`]) so the engine accepts
/// it for this call: quoted words, write redirects and root-relative paths as rules see
/// them, glob characters escaped. `root` is where the session started. None when no single
/// rule names the call (several commands, a patch, parentheses).
pub fn exact_rule(root: &Path, tool: &str, args: &Value) -> Option<String> {
    tautly_core::exact_rule(root, tool, args)
}

/// The question under a tool call's approval request.
pub fn tool_prompt(rule: Option<&str>) -> String {
    match rule {
        Some(r) => format!(
            "allow? [y]es [n]o [a]lways this session ({}) ",
            printable(r)
        ),
        None => "allow? [y]es [n]o [a]lways this session (a <rule>) ".to_string(),
    }
}

/// How the user answered a tool call's approval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolAnswer {
    /// Approve this one call.
    Once,
    /// Refuse this call.
    Deny,
    /// Approve, and allow what this rule matches for the session.
    Always(String),
    /// Not an answer: ask again, saying why.
    Again(&'static str),
}

/// Parse an answer to [`tool_prompt`]; `rule` is the exact-target rule, if any.
pub fn tool_answer(line: &str, rule: Option<&str>) -> ToolAnswer {
    let line = line.trim();
    let (word, rest) = match line.split_once(char::is_whitespace) {
        Some((w, r)) => (w, r.trim()),
        None => (line, ""),
    };
    match (word.to_ascii_lowercase().as_str(), rest) {
        ("y" | "yes", "") => ToolAnswer::Once,
        ("n" | "no", "") => ToolAnswer::Deny,
        ("a" | "always", "") => match rule {
            Some(r) => ToolAnswer::Always(r.to_string()),
            None => ToolAnswer::Again("this call has no exact rule: type `a <rule>`"),
        },
        ("a" | "always", r) => ToolAnswer::Always(r.to_string()),
        _ => ToolAnswer::Again("answer y, n, a or `a <rule>`"),
    }
}

/// A `y`/`n` answer: Some(true) for yes, Some(false) for no, None for anything else.
pub fn yes_no(line: &str) -> Option<bool> {
    match line.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => Some(true),
        "n" | "no" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers() {
        let r = Some("bash(ls)");
        assert_eq!(tool_answer("y", r), ToolAnswer::Once);
        assert_eq!(tool_answer(" N ", r), ToolAnswer::Deny);
        assert_eq!(tool_answer("a", r), ToolAnswer::Always("bash(ls)".into()));
        assert_eq!(
            tool_answer("a  bash(ls*) ", r),
            ToolAnswer::Always("bash(ls*)".into())
        );
        assert!(matches!(tool_answer("a", None), ToolAnswer::Again(_)));
        assert!(matches!(tool_answer("maybe", r), ToolAnswer::Again(_)));
        assert_eq!(yes_no("Yes"), Some(true));
        assert_eq!(yes_no("x"), None);
        assert_eq!(
            exact_rule(
                Path::new("/"),
                "web_fetch",
                &serde_json::json!({"url": "http://[::1]:8/x"})
            )
            .as_deref(),
            Some("web_fetch(::1)")
        );
    }
}
