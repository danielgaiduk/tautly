//! Approval prompts (spec 7.11.8): `allow? [y]es [n]o [a]lways this session (<rule>)`.
//!
//! `y` approves once, `n` denies, `a` approves and asks the engine to allow calls the
//! exact-target rule matches for the rest of the session, and `a <rule>` sends the user's
//! own pattern instead. The engine accepts a session rule only when it matches the pending
//! call (else it approves once and warns); deny rules and protected paths still win.
//! Memory proposals and MCP server approvals are plain `y`/`n` questions.

use serde_json::Value;
use tautly_core::printable;

/// The exact-target session rule for a call, as `a` sends it: `edit(<path>)`,
/// `write(<path>)`, `bash(<command>)` (whitespace collapsed), `web_fetch(<host>)` or
/// `mcp__<server>__<tool>`, with glob metacharacters escaped. None when the call has no
/// single target such a rule can name (a patch, a command with parentheses).
pub fn exact_rule(tool: &str, args: &Value) -> Option<String> {
    let arg = |k: &str| args.get(k).and_then(Value::as_str);
    let target = match tool {
        "edit" | "write" => {
            let mut path = arg("path")?;
            while let Some(rest) = path.strip_prefix("./") {
                path = rest;
            }
            path.to_string()
        }
        "bash" => arg("command")?
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" "),
        "web_fetch" => host(arg("url")?)?,
        t if t.starts_with("mcp__") => {
            let plain = t
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
            return plain.then(|| t.to_string());
        }
        _ => return None,
    };
    // A rule's pattern cannot hold parentheses, and an empty one would match anything.
    if target.is_empty() || target.contains(['(', ')']) {
        return None;
    }
    Some(format!("{tool}({})", escape_glob(&target)))
}

/// The host a `web_fetch` URL reaches, as the policy sees it: lowercase, no port, no
/// trailing dot, no IPv6 brackets.
fn host(url: &str) -> Option<String> {
    let url = url::Url::parse(url).ok()?;
    let h = url.host_str()?.to_ascii_lowercase();
    let h = h.trim_start_matches('[').trim_end_matches(']');
    Some(h.trim_end_matches('.').to_string())
}

/// `s` as a glob that matches only itself.
fn escape_glob(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '*' | '?' | '[' | ']' | '{' | '}' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
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
    Once,
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
            exact_rule("web_fetch", &serde_json::json!({"url": "http://[::1]:8/x"})).as_deref(),
            Some("web_fetch(::1)")
        );
    }
}
