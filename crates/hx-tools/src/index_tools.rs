//! `outline`, `symbol` and `refs`: code-index queries (spec 7.1).

use std::path::Path;

use async_trait::async_trait;
use hx_index::{Lang, SymbolKind, SymbolRow};
use hx_provider::{Capabilities, ToolSpec};
use hx_sandbox::{Effect, Sandbox};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::cap::{OUTPUT_CAP, cap_text};
use crate::spec::{decode_text, parse_args, tool_spec};
use crate::{GrepTool, Tool, ToolCx, ToolError, ToolResult};

/// Default number of `refs` results.
const DEFAULT_REFS_LIMIT: usize = 50;
const MAX_REFS_LIMIT: usize = 500;
const REFS_HEADER_NOTE: &str = "name-based, may include unrelated identifiers with the same name";

/// The line that says why results do not come from a ready index, if they do not.
pub(crate) fn fallback_header(cx: &ToolCx) -> Option<String> {
    match cx.index() {
        Some(index) if index.is_ready() => None,
        Some(index) => {
            let mut header = "(index building)".to_string();
            if let Some(e) = index.last_error() {
                header.push_str(&format!("\nlast index error: {e}"));
            }
            Some(header)
        }
        None => Some("(index unavailable)".to_string()),
    }
}

/// `header` (if any) above `body`, within the 30 KB output cap.
fn finish(header: Option<String>, body: String) -> ToolResult {
    let full = match header {
        Some(h) => format!("{h}\n{body}"),
        None => body,
    };
    ToolResult::ok(cap_text(&full, OUTPUT_CAP).0)
}

fn location(row: &SymbolRow) -> String {
    format!(
        "{}:L{}-{} {} {}",
        row.path.display(),
        row.start_line,
        row.end_line,
        row.kind.as_str(),
        row.signature
    )
}

/// Word-boundary search for `name` through the grep tool.
async fn grep_word(name: &str, path: Option<&str>, sb: &dyn Sandbox, cx: &ToolCx) -> ToolResult {
    let mut args = json!({"pattern": format!(r"\b{}\b", regex::escape(name))});
    if let Some(p) = path {
        args["path"] = json!(p);
    }
    GrepTool.run(args, sb, cx).await
}

/// Symbols of `path` parsed on demand from its current content.
async fn parse_outline(path: &str, sb: &dyn Sandbox) -> Result<Vec<SymbolRow>, String> {
    let full = sb.resolve(Path::new(path)).map_err(|e| e.to_string())?;
    let bytes = sb.read(&full).await.map_err(|e| format!("{path}: {e}"))?;
    let text = decode_text(&bytes).map_err(|e| format!("{path}: {e}"))?;
    let lang = Lang::from_path(Path::new(path))
        .filter(|l| l.has_symbols())
        .ok_or_else(|| format!("{path}: no symbol support for this file type"))?;
    Ok(hx_index::extract(lang, text).symbols)
}

pub struct OutlineTool;
pub struct SymbolTool;
pub struct RefsTool;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OutlineArgs {
    path: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SymbolArgs {
    name: String,
    kind: Option<KindArg>,
}

#[derive(Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
enum KindArg {
    Function,
    Method,
    Type,
    Constant,
    Module,
}

impl From<KindArg> for SymbolKind {
    fn from(k: KindArg) -> Self {
        match k {
            KindArg::Function => SymbolKind::Function,
            KindArg::Method => SymbolKind::Method,
            KindArg::Type => SymbolKind::Type,
            KindArg::Constant => SymbolKind::Constant,
            KindArg::Module => SymbolKind::Module,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RefsArgs {
    name: String,
    path: Option<String>,
    limit: Option<usize>,
}

#[async_trait]
impl Tool for OutlineTool {
    fn name(&self) -> &'static str {
        "outline"
    }

    fn spec(&self, _caps: &Capabilities) -> ToolSpec {
        tool_spec(
            "outline",
            "List the symbols of one file (functions, methods, types, constants) with signatures \
             and line ranges, without bodies. Cheaper than reading the whole file.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path."}
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        )
    }

    fn effect(&self, args: &Value) -> Result<Effect, ToolError> {
        parse_args::<OutlineArgs>(args)?;
        Ok(Effect::ReadOnly)
    }

    async fn run(&self, args: Value, sb: &dyn Sandbox, cx: &ToolCx) -> ToolResult {
        let a: OutlineArgs = match parse_args(&args) {
            Ok(a) => a,
            Err(e) => return ToolResult::error(e.to_string()),
        };
        let header = fallback_header(cx);
        let indexed = match cx.index().filter(|i| i.is_ready()) {
            Some(index) => match index.outline(Path::new(&a.path)) {
                Ok(rows) => rows,
                Err(e) => return ToolResult::error(format!("index error: {e}")),
            },
            None => Vec::new(),
        };
        // A ready index that has never seen the file (or an unbuilt one) is not an answer.
        let rows = if indexed.is_empty() {
            match parse_outline(&a.path, sb).await {
                Ok(rows) => rows,
                Err(e) => return ToolResult::error(e),
            }
        } else {
            indexed
        };
        if rows.is_empty() {
            return finish(header, format!("no symbols found in {}", a.path));
        }
        let body = rows
            .iter()
            .map(|r| {
                let indent = if r.parent.is_some() { "  " } else { "" };
                format!(
                    "{indent}L{}-{} {} {}",
                    r.start_line,
                    r.end_line,
                    r.kind.as_str(),
                    r.signature
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        finish(header, body)
    }
}

#[async_trait]
impl Tool for SymbolTool {
    fn name(&self) -> &'static str {
        "symbol"
    }

    fn spec(&self, _caps: &Capabilities) -> ToolSpec {
        tool_spec(
            "symbol",
            "Get the source of a definition by name, with its path and line range. If several \
             definitions share the name, only their locations are listed; narrow with `kind` or \
             `read` the file.",
            json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "Exact symbol name."},
                    "kind": {
                        "type": "string",
                        "enum": ["function", "method", "type", "constant", "module"],
                        "description": "Only definitions of this kind."
                    }
                },
                "required": ["name"],
                "additionalProperties": false
            }),
        )
    }

    fn effect(&self, args: &Value) -> Result<Effect, ToolError> {
        parse_args::<SymbolArgs>(args)?;
        Ok(Effect::ReadOnly)
    }

    async fn run(&self, args: Value, sb: &dyn Sandbox, cx: &ToolCx) -> ToolResult {
        let a: SymbolArgs = match parse_args(&args) {
            Ok(a) => a,
            Err(e) => return ToolResult::error(e.to_string()),
        };
        if a.name.is_empty() {
            return ToolResult::error("name must not be empty");
        }
        let Some(index) = cx.index().filter(|i| i.is_ready()) else {
            let found = grep_word(&a.name, None, sb, cx).await;
            let header = fallback_header(cx).unwrap_or_default();
            return ToolResult {
                output: format!("{header}\n{}", found.output),
                ..found
            };
        };
        let rows = match index.find_symbol(&a.name, a.kind.map(SymbolKind::from)) {
            Ok(r) => r,
            Err(e) => return ToolResult::error(format!("index error: {e}")),
        };
        match rows.as_slice() {
            [] => ToolResult::ok(format!("no definition of `{}` found", a.name)),
            [row] => {
                let path = row.path.display().to_string();
                let bytes = match sb.read(Path::new(&path)).await {
                    Ok(b) => b,
                    Err(e) => return ToolResult::error(format!("{path}: {e}")),
                };
                let text = match decode_text(&bytes) {
                    Ok(t) => t,
                    Err(e) => return ToolResult::error(format!("{path}: {e}")),
                };
                let lines: Vec<&str> = text.lines().collect();
                let (start, end) = (row.start_line as usize, row.end_line as usize);
                if start == 0 || end < start || end > lines.len() {
                    return ToolResult::error(format!(
                        "{path}: the index is out of date for this file; use `read`"
                    ));
                }
                let source = lines[start - 1..end].join("\n");
                finish(None, format!("{}\n{source}", location(row)))
            }
            many => {
                let mut body = format!(
                    "{} definitions of `{}`; narrow with `kind` or `read` the one you need:",
                    many.len(),
                    a.name
                );
                for row in many {
                    body.push('\n');
                    body.push_str(&location(row));
                    if let Some(parent) = &row.parent {
                        body.push_str(&format!(" (in {parent})"));
                    }
                }
                finish(None, body)
            }
        }
    }
}

#[async_trait]
impl Tool for RefsTool {
    fn name(&self) -> &'static str {
        "refs"
    }

    fn spec(&self, _caps: &Capabilities) -> ToolSpec {
        tool_spec(
            "refs",
            "Find uses of an identifier: path:line: context (name-based, may include unrelated \
             identifiers with the same name). Definitions are not listed.",
            json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "Identifier to look for."},
                    "path": {"type": "string", "description": "Only results under this file or directory."},
                    "limit": {"type": "integer", "minimum": 1, "description": "Most results to return (default 50)."}
                },
                "required": ["name"],
                "additionalProperties": false
            }),
        )
    }

    fn effect(&self, args: &Value) -> Result<Effect, ToolError> {
        parse_args::<RefsArgs>(args)?;
        Ok(Effect::ReadOnly)
    }

    async fn run(&self, args: Value, sb: &dyn Sandbox, cx: &ToolCx) -> ToolResult {
        let a: RefsArgs = match parse_args(&args) {
            Ok(a) => a,
            Err(e) => return ToolResult::error(e.to_string()),
        };
        if a.name.is_empty() {
            return ToolResult::error("name must not be empty");
        }
        let heading = format!("refs to `{}` ({REFS_HEADER_NOTE}):", a.name);
        let Some(index) = cx.index().filter(|i| i.is_ready()) else {
            let found = grep_word(&a.name, a.path.as_deref(), sb, cx).await;
            let header = fallback_header(cx).unwrap_or_default();
            return ToolResult {
                output: format!("{header}\n{heading}\n{}", found.output),
                ..found
            };
        };
        let limit = a
            .limit
            .unwrap_or(DEFAULT_REFS_LIMIT)
            .clamp(1, MAX_REFS_LIMIT);
        // The index reports no totals: asking for one more row than wanted detects truncation.
        let prefix = a.path.as_deref().map(Path::new);
        let mut rows = match index.refs_in(&a.name, prefix, limit + 1) {
            Ok(r) => r,
            Err(e) => return ToolResult::error(format!("index error: {e}")),
        };
        if rows.is_empty() {
            return ToolResult::ok(format!("no references to `{}` found", a.name));
        }
        let truncated = rows.len() > limit;
        rows.truncate(limit);
        let mut body = heading;
        for r in &rows {
            body.push_str(&format!("\n{}:{}: {}", r.path.display(), r.line, r.context));
        }
        if truncated {
            body.push_str("\n… more results truncated; narrow with path");
        }
        finish(None, body)
    }
}
