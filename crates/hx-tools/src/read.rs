use std::path::Path;

use async_trait::async_trait;
use hx_provider::{Capabilities, EditFormat, ToolSpec};
use hx_sandbox::{Effect, Sandbox};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::cap::{OUTPUT_CAP, cap_text};
use crate::spec::{decode_text, parse_args, tool_spec};
use crate::{AppliedEffect, Tool, ToolCx, ToolError, ToolResult};

const MAX_LINES: usize = 2000;

pub struct ReadTool;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    path: String,
    start_line: Option<usize>,
    end_line: Option<usize>,
}

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &'static str {
        "read"
    }

    fn spec(&self, _caps: &Capabilities) -> ToolSpec {
        tool_spec(
            "read",
            "Read a text file with line numbers. Use start_line/end_line (1-based, inclusive) for a range.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path."},
                    "start_line": {"type": "integer", "minimum": 1, "description": "First line to read."},
                    "end_line": {"type": "integer", "minimum": 1, "description": "Last line to read."}
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        )
    }

    fn effect(&self, args: &Value) -> Result<Effect, ToolError> {
        parse_args::<Args>(args)?;
        Ok(Effect::ReadOnly)
    }

    async fn run(&self, args: Value, sb: &dyn Sandbox, cx: &ToolCx) -> ToolResult {
        let a: Args = match parse_args(&args) {
            Ok(a) => a,
            Err(e) => return ToolResult::error(e.to_string()),
        };
        let path = Path::new(&a.path);
        let full = match sb.resolve(path) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(e.to_string()),
        };
        let (meta, bytes) = match (sb.metadata(&full).await, sb.read(&full).await) {
            (Ok(m), Ok(b)) => (m, b),
            (Err(e), _) | (_, Err(e)) => return ToolResult::error(format!("{}: {e}", a.path)),
        };
        let text = match decode_text(&bytes) {
            Ok(t) => t,
            Err(why) => return ToolResult::error(format!("{}: {why}", a.path)),
        };
        let hash = blake3::hash(&bytes);
        cx.reads().record(&full, &meta, hash);

        let lines: Vec<&str> = text.lines().collect();
        let total = lines.len();
        let start = a.start_line.unwrap_or(1);
        if start == 0 || a.end_line == Some(0) {
            return ToolResult::error("line numbers are 1-based");
        }
        let mut end = a.end_line.unwrap_or(total).min(total);
        if total > 0 && start > end {
            return ToolResult::error(format!(
                "start_line {start} is past the end ({total} lines) or after end_line"
            ));
        }
        let mut remaining = 0;
        if a.end_line.is_none() && end + 1 > start + MAX_LINES {
            end = start + MAX_LINES - 1;
            remaining = total - end;
        }
        let hashline = cx.edit_format() == EditFormat::Hashline;
        let mut out: Vec<String> = Vec::new();
        for (i, line) in lines.iter().enumerate().take(end).skip(start - 1) {
            let n = i + 1;
            if hashline {
                let tag = &blake3::hash(line.as_bytes()).to_hex()[..4];
                out.push(format!("{n}:{tag}|{line}"));
            } else {
                out.push(format!("{n}\t{line}"));
            }
        }
        let (mut output, truncated) = cap_text(&out.join("\n"), OUTPUT_CAP);
        if truncated {
            output.push_str("\n[output truncated; request a narrower line range]");
        }
        if remaining > 0 {
            output.push_str(&format!(
                "\n[{remaining} more lines; use start_line={} to continue]",
                end + 1
            ));
        }
        let mut r = ToolResult::ok(output);
        r.effects.push(AppliedEffect::Read(full, hash));
        r
    }
}
