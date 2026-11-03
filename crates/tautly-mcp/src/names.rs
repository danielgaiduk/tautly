//! Tool names the model sees: `mcp__<server>__<tool>` (spec 7.11.10).

use std::collections::HashMap;

/// Longest name a provider accepts for a tool.
pub const MAX_TOOL_NAME: usize = 64;
/// Characters kept of a name that gets the hash suffix.
const KEPT: usize = MAX_TOOL_NAME - 7;

/// `s` with every character outside `[A-Za-z0-9_-]` replaced by `_`.
pub fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// The name of `tool` of `server`: `mcp__<server>__<tool>`, sanitized; a name over
/// [`MAX_TOOL_NAME`] is cut to 57 characters plus `_` and 6 hex characters of the blake3
/// of the original server and tool names.
pub fn tool_name(server: &str, tool: &str) -> String {
    let name = plain(server, tool);
    if name.len() > MAX_TOOL_NAME {
        suffixed(&name, server, tool)
    } else {
        name
    }
}

/// Names for `pairs` (server, tool), all distinct: a name that two different pairs share
/// after sanitizing gets the hash suffix on every one of them, so no name depends on the
/// order of the pairs. Identical pairs keep identical names.
pub fn assign(pairs: &[(&str, &str)]) -> Vec<String> {
    let names: Vec<String> = pairs.iter().map(|(s, t)| tool_name(s, t)).collect();
    let mut owners: HashMap<&str, Vec<(&str, &str)>> = HashMap::new();
    for (name, pair) in names.iter().zip(pairs) {
        let owners = owners.entry(name).or_default();
        if !owners.contains(pair) {
            owners.push(*pair);
        }
    }
    names
        .iter()
        .zip(pairs)
        .map(|(name, (server, tool))| {
            if owners[name.as_str()].len() > 1 {
                suffixed(&plain(server, tool), server, tool)
            } else {
                name.clone()
            }
        })
        .collect()
}

fn plain(server: &str, tool: &str) -> String {
    format!("mcp__{}__{}", sanitize(server), sanitize(tool))
}

/// The first 57 characters of `name` (all ASCII after sanitizing), `_`, and 6 hex
/// characters of the hash of the unsanitized pair.
fn suffixed(name: &str, server: &str, tool: &str) -> String {
    let mut h = blake3::Hasher::new();
    h.update(server.as_bytes());
    h.update(&[0]);
    h.update(tool.as_bytes());
    let hex = h.finalize().to_hex();
    format!("{}_{}", &name[..name.len().min(KEPT)], &hex[..6])
}

/// Longest name of a repository-declared server.
pub const MAX_SERVER_NAME: usize = 32;

/// Whether `name` may name a repository-declared server: `[A-Za-z0-9]`, then
/// `[A-Za-z0-9-]` with single `_` between parts (`^[A-Za-z0-9][A-Za-z0-9-]*(_[A-Za-z0-9-]+)*$`),
/// at most [`MAX_SERVER_NAME`] characters. Such a name is its own sanitized form and has
/// no `__`, so its tools (`mcp__<name>__<tool>`) can never pass for another server's and
/// fall under that server's permission rules (`mcp__gh__*` must not cover a server named
/// `gh__x` or `gh.`).
pub fn is_valid_server_name(name: &str) -> bool {
    name.len() <= MAX_SERVER_NAME
        && name.starts_with(|c: char| c.is_ascii_alphanumeric())
        && !name.ends_with('_')
        && !name.contains("__")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}
