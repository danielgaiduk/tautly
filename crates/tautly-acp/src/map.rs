//! Engine events and values in ACP terms (spec 7.11.9): tool kinds, titles, locations and
//! contents, stop reasons, prompts, slash commands and the editor's MCP servers.

use std::path::{Component, Path, PathBuf};

use serde_json::{Value, json};
use tautly_core::{FinishReason, ServerSpec};

use crate::schema::{
    ContentBlock, McpServer, StopReason, ToolCallContent, ToolCallLocation, ToolKind,
};

/// Largest file text (either side) sent as `diff` content; a larger change is shown as
/// its unified diff instead.
pub const MAX_DIFF_TEXT: usize = 256 * 1024;
/// Largest tool output sent as text content.
pub const MAX_OUTPUT_TEXT: usize = 8 * 1024;
/// Longest target in a tool call's title, in characters.
const MAX_TITLE_TARGET: usize = 80;

/// The ACP kind of a tool (spec 7.11.9 table); MCP and unknown tools are `other`.
pub fn tool_kind(tool: &str) -> ToolKind {
    match tool {
        "read" | "outline" | "symbol" => ToolKind::Read,
        "grep" | "glob" | "find" | "refs" | "recall" => ToolKind::Search,
        "edit" | "write" => ToolKind::Edit,
        "bash" => ToolKind::Execute,
        "web_fetch" => ToolKind::Fetch,
        "pin" | "state" | "remember" => ToolKind::Think,
        _ => ToolKind::Other,
    }
}

/// How a turn that ended for `r` answers its `session/prompt`; None for `error`, which is
/// a JSON-RPC error instead. `budget` and `timeout` end the turn normally (the bridge
/// tells the user why first); a reason this version does not know does too.
pub fn stop_reason(r: FinishReason) -> Option<StopReason> {
    match r {
        FinishReason::Error => None,
        FinishReason::MaxTurns => Some(StopReason::MaxTurnRequests),
        FinishReason::Cancelled => Some(StopReason::Cancelled),
        FinishReason::Completed
        | FinishReason::Budget
        | FinishReason::Timeout
        | FinishReason::Unknown => Some(StopReason::EndTurn),
    }
}

/// `diff` content for a file whose text was `old` (None: created) and is `new`; None when
/// either side is over [`MAX_DIFF_TEXT`].
pub fn diff_content(path: &Path, old: Option<&str>, new: &str) -> Option<ToolCallContent> {
    if new.len() > MAX_DIFF_TEXT || old.is_some_and(|o| o.len() > MAX_DIFF_TEXT) {
        return None;
    }
    Some(ToolCallContent::Diff {
        path: path.to_path_buf(),
        old_text: old.map(str::to_string),
        new_text: new.to_string(),
    })
}

/// The argument a title names, in order of preference.
const TARGET_KEYS: &[&str] = &[
    "path", "command", "pattern", "url", "query", "name", "title", "text",
];

/// `<tool>: <target>`, the target being the call's path, command, pattern, URL or query
/// (first line, at most 80 characters); just the tool when it has none.
pub fn tool_title(tool: &str, args: &Value) -> String {
    let target = TARGET_KEYS
        .iter()
        .find_map(|k| args.get(*k).and_then(Value::as_str))
        .map(|t| {
            t.lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("")
                .trim()
        })
        .filter(|t| !t.is_empty());
    match target {
        None => tool.to_string(),
        Some(t) if t.chars().count() > MAX_TITLE_TARGET => {
            let cut: String = t.chars().take(MAX_TITLE_TARGET).collect();
            format!("{tool}: {cut}…")
        }
        Some(t) => format!("{tool}: {t}"),
    }
}

/// The files a call names (its `path` argument), absolute under `root`.
pub fn locations(root: &Path, args: &Value) -> Vec<ToolCallLocation> {
    args.get("path")
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty())
        .map(|p| ToolCallLocation {
            path: absolute(root, p),
            line: None,
        })
        .into_iter()
        .collect()
}

/// A tool's output as text content, cut to [`MAX_OUTPUT_TEXT`] bytes (with a marker).
pub fn output_content(output: &str) -> ToolCallContent {
    if output.len() <= MAX_OUTPUT_TEXT {
        return ToolCallContent::text(output);
    }
    let mut end = MAX_OUTPUT_TEXT;
    while !output.is_char_boundary(end) {
        end -= 1;
    }
    ToolCallContent::text(format!(
        "{}\n… output cut ({} bytes in all)",
        &output[..end],
        output.len()
    ))
}

/// The text of a user message from prompt blocks: text blocks joined, embedded text
/// resources inlined as fenced text under their URI, resource links as their URI. An
/// image, audio or binary resource is refused (the engine takes text only).
pub fn prompt_text(blocks: &[ContentBlock]) -> Result<String, String> {
    let mut parts = Vec::with_capacity(blocks.len());
    for block in blocks {
        parts.push(match block {
            ContentBlock::Text { text } => text.clone(),
            ContentBlock::Resource { resource } => match &resource.text {
                Some(text) => {
                    let fence = "`".repeat(longest_backtick_run(text).max(3) + 1);
                    let nl = if text.ends_with('\n') { "" } else { "\n" };
                    format!("{}\n{fence}\n{text}{nl}{fence}", resource.uri)
                }
                None => {
                    return Err(format!(
                        "the binary resource {} cannot be sent: only text is supported",
                        resource.uri
                    ));
                }
            },
            ContentBlock::ResourceLink { uri, .. } => uri.clone(),
            ContentBlock::Image { .. } => {
                return Err("images are not supported: send text".into());
            }
            ContentBlock::Audio { .. } => {
                return Err("audio is not supported: send text".into());
            }
            ContentBlock::Unknown => {
                return Err("a prompt block of an unknown type: send text".into());
            }
        });
    }
    Ok(parts.join("\n"))
}

fn longest_backtick_run(text: &str) -> usize {
    let mut best = 0;
    let mut run = 0;
    for c in text.chars() {
        if c == '`' {
            run += 1;
            best = best.max(run);
        } else {
            run = 0;
        }
    }
    best
}

/// A prompt that is one of the advertised slash commands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Slash {
    Compact,
    Undo(Option<u32>),
    Diff(Option<u32>),
    Cost,
    Pin(String),
    /// A known command with arguments it does not take: its usage line.
    Usage(&'static str),
}

/// The advertised commands: name, description, input hint.
const COMMANDS: &[(&str, &str, Option<&str>)] = &[
    ("compact", "Compact the conversation now", None),
    (
        "undo",
        "Undo the last turn's changes (or turn N's)",
        Some("turn number (optional)"),
    ),
    (
        "diff",
        "Show what the session (or turn N) changed",
        Some("turn number (optional)"),
    ),
    ("cost", "The last turn's and the session's cost", None),
    ("pin", "Keep a note in the context", Some("text to pin")),
];

/// The slash command a prompt names, or None when it is a message.
pub fn slash(text: &str) -> Option<Slash> {
    let rest = text.trim().strip_prefix('/')?;
    let (name, arg) = match rest.split_once(char::is_whitespace) {
        Some((n, a)) => (n, a.trim()),
        None => (rest, ""),
    };
    let number = |usage: &'static str, make: fn(Option<u32>) -> Slash| {
        if arg.is_empty() {
            return make(None);
        }
        match arg.parse::<u32>() {
            Ok(n) if n > 0 => make(Some(n)),
            _ => Slash::Usage(usage),
        }
    };
    Some(match name {
        "compact" if arg.is_empty() => Slash::Compact,
        "compact" => Slash::Usage("usage: /compact"),
        "undo" => number("usage: /undo [N]", Slash::Undo),
        "diff" => number("usage: /diff [N]", Slash::Diff),
        "cost" if arg.is_empty() => Slash::Cost,
        "cost" => Slash::Usage("usage: /cost"),
        "pin" if arg.is_empty() => Slash::Usage("usage: /pin <text>"),
        "pin" => Slash::Pin(arg.to_string()),
        _ => return None,
    })
}

/// The `availableCommands` of an `available_commands_update`.
pub fn available_commands() -> Value {
    Value::Array(
        COMMANDS
            .iter()
            .map(|(name, description, hint)| {
                let mut c = json!({"name": name, "description": description});
                if let Some(hint) = hint {
                    c["input"] = json!({"hint": hint});
                }
                c
            })
            .collect(),
    )
}

/// The editor's MCP servers as engine server specs, and a line for each one that cannot
/// be used (an SSE server, an invalid URL, an empty command).
pub fn client_mcp(servers: Vec<McpServer>) -> (Vec<(String, ServerSpec)>, Vec<String>) {
    let mut specs = Vec::new();
    let mut skipped = Vec::new();
    for server in servers {
        let raw = match &server {
            McpServer::Stdio {
                command, args, env, ..
            } => json!({
                "command": command,
                "args": args,
                "env": env.iter().map(|(k, v)| (k.clone(), json!(v))).collect::<serde_json::Map<String, Value>>(),
            }),
            McpServer::Http { url, headers, .. } => json!({
                "url": url,
                "headers": headers.iter().map(|(k, v)| (k.clone(), json!(v))).collect::<serde_json::Map<String, Value>>(),
            }),
            McpServer::Sse { name } => {
                skipped.push(format!(
                    "MCP server `{name}` uses SSE, which is not supported; it is not started"
                ));
                continue;
            }
        };
        match serde_json::from_value::<ServerSpec>(raw) {
            Ok(spec) => specs.push((server.name().to_string(), spec)),
            Err(e) => skipped.push(format!(
                "MCP server `{}` from the editor is not usable ({e}); it is not started",
                server.name()
            )),
        }
    }
    (specs, skipped)
}

/// `diff` contents for an approval's preview: each file's current text under `root`
/// with the unified diff applied. None when the diff is truncated, does not apply, or
/// names a file that cannot be read; the caller then shows the unified text. Reads the
/// files (blocking).
pub fn preview_diffs(root: &Path, unified: &str) -> Option<Vec<ToolCallContent>> {
    let files = parse_unified(unified)?;
    let mut out = Vec::with_capacity(files.len());
    for file in files {
        let rel = file.new_path.as_deref().or(file.old_path.as_deref())?;
        let path = confined(root, rel)?;
        let old = match &file.old_path {
            Some(_) => Some(std::fs::read_to_string(&path).ok()?),
            None => None,
        };
        let new = apply(old.as_deref().unwrap_or(""), &file.hunks)?;
        let new = if file.new_path.is_some() {
            new
        } else {
            String::new()
        };
        out.push(diff_content(&path, old.as_deref(), &new)?);
    }
    (!out.is_empty()).then_some(out)
}

/// `path` (root-relative, or absolute) as an absolute path under `root`.
pub fn absolute(root: &Path, path: &str) -> PathBuf {
    let p = Path::new(path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        root.join(p)
    }
}

/// A root-relative path that stays below `root` (no `..`, not absolute).
fn confined(root: &Path, rel: &str) -> Option<PathBuf> {
    let p = Path::new(rel);
    p.components()
        .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
        .then(|| root.join(p))
}

/// One file of a unified diff: its paths (None: `/dev/null`) and hunks.
struct FileDiff {
    old_path: Option<String>,
    new_path: Option<String>,
    hunks: Vec<Hunk>,
}

/// `@@ -old_start,… @@` and its lines (`' '`, `'-'`, `'+'` with the text after the tag,
/// newline included; a line followed by `\ No newline at end of file` has none).
struct Hunk {
    old_start: usize,
    lines: Vec<(char, String)>,
}

fn side(header: &str, prefix: &str) -> Option<Option<String>> {
    let rest = header.strip_prefix(prefix)?;
    if rest == "/dev/null" {
        return Some(None);
    }
    let path = rest.strip_prefix(if prefix == "--- " { "a/" } else { "b/" })?;
    Some(Some(path.to_string()))
}

/// `start[,count]` of a hunk header side (count 1 when left out).
fn range(s: &str) -> Option<(usize, usize)> {
    match s.split_once(',') {
        Some((a, b)) => Some((a.parse().ok()?, b.parse().ok()?)),
        None => Some((s.parse().ok()?, 1)),
    }
}

/// The files of a unified diff as Tautly's previews write it; None for anything else (a
/// truncated diff, a binary or summary-only file, text it cannot read). Hunk lines are
/// counted from their headers, so a removed line that reads like a file header is not
/// taken for one.
fn parse_unified(unified: &str) -> Option<Vec<FileDiff>> {
    let mut files: Vec<FileDiff> = Vec::new();
    // Old and new lines the current hunk still has.
    let (mut old_left, mut new_left) = (0usize, 0usize);
    let mut lines = unified.split_inclusive('\n');
    while let Some(line) = lines.next() {
        let bare = line.strip_suffix('\n').unwrap_or(line);
        if bare == "\\ No newline at end of file" {
            let hunk = files.last_mut()?.hunks.last_mut()?;
            let (_, text) = hunk.lines.last_mut()?;
            text.pop().filter(|c| *c == '\n')?;
            continue;
        }
        if old_left > 0 || new_left > 0 {
            let tag = bare.chars().next()?;
            match tag {
                ' ' if old_left > 0 && new_left > 0 => {
                    (old_left, new_left) = (old_left - 1, new_left - 1)
                }
                '-' if old_left > 0 => old_left -= 1,
                '+' if new_left > 0 => new_left -= 1,
                _ => return None,
            }
            if !line.ends_with('\n') {
                return None;
            }
            let hunk = files.last_mut()?.hunks.last_mut()?;
            hunk.lines.push((tag, line[1..].to_string()));
        } else if let Some(old) = side(bare, "--- ") {
            let next = lines.next()?;
            let new = side(next.strip_suffix('\n').unwrap_or(next), "+++ ")?;
            files.push(FileDiff {
                old_path: old,
                new_path: new,
                hunks: Vec::new(),
            });
        } else {
            // Anything else than a hunk header (a binary or summary-only file, the
            // truncation marker) is not a diff this can apply.
            let rest = bare.strip_prefix("@@ -")?;
            let (old, rest) = rest.split_once(" +")?;
            let (new, _) = rest.split_once(" @@")?;
            let (old_start, old_count) = range(old)?;
            let (_, new_count) = range(new)?;
            (old_left, new_left) = (old_count, new_count);
            files.last_mut()?.hunks.push(Hunk {
                // `-0,0` (nothing before) applies at the top.
                old_start: if old_count == 0 {
                    old_start + 1
                } else {
                    old_start
                },
                lines: Vec::new(),
            });
        }
    }
    (old_left == 0 && new_left == 0).then_some(files)
}

/// `old` with `hunks` applied; None when a context or removed line does not match.
fn apply(old: &str, hunks: &[Hunk]) -> Option<String> {
    let old_lines: Vec<&str> = old.split_inclusive('\n').collect();
    let mut out = String::with_capacity(old.len());
    let mut at = 0usize;
    for hunk in hunks {
        let start = hunk.old_start.saturating_sub(1);
        if start < at || start > old_lines.len() {
            return None;
        }
        old_lines[at..start].iter().for_each(|l| out.push_str(l));
        at = start;
        for (tag, text) in &hunk.lines {
            match tag {
                '+' => out.push_str(text),
                _ => {
                    if old_lines.get(at) != Some(&text.as_str()) {
                        return None;
                    }
                    if *tag == ' ' {
                        out.push_str(text);
                    }
                    at += 1;
                }
            }
        }
    }
    old_lines[at..].iter().for_each(|l| out.push_str(l));
    Some(out)
}
