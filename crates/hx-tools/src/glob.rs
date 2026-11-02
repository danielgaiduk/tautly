use std::path::Path;

use async_trait::async_trait;
use globset::Glob;
use hx_provider::{Capabilities, ToolSpec};
use hx_sandbox::{Effect, Sandbox};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::spec::{display_rel, parse_args, tool_spec, walk_files};
use crate::{Tool, ToolCx, ToolError, ToolResult};

const MAX_RESULTS: usize = 1000;

pub struct GlobTool;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    pattern: String,
    path: Option<String>,
}

/// A pattern without a `/` matches at any depth, like gitignore.
pub(crate) fn anchor(pattern: &str) -> String {
    if pattern.contains('/') {
        pattern.to_string()
    } else {
        format!("**/{pattern}")
    }
}

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &'static str {
        "glob"
    }

    fn spec(&self, _caps: &Capabilities) -> ToolSpec {
        tool_spec(
            "glob",
            "List files matching a glob pattern, sorted. Respects .gitignore.",
            json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "Glob such as **/*.rs."},
                    "path": {"type": "string", "description": "Directory to search. Defaults to the project root."}
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
        )
    }

    fn effect(&self, args: &Value) -> Result<Effect, ToolError> {
        parse_args::<Args>(args)?;
        Ok(Effect::ReadOnly)
    }

    async fn run(&self, args: Value, sb: &dyn Sandbox, _cx: &ToolCx) -> ToolResult {
        let a: Args = match parse_args(&args) {
            Ok(a) => a,
            Err(e) => return ToolResult::error(e.to_string()),
        };
        let matcher = match Glob::new(&anchor(&a.pattern)) {
            Ok(g) => g.compile_matcher(),
            Err(e) => return ToolResult::error(format!("invalid glob: {e}")),
        };
        let base = a.path.as_deref().unwrap_or(".");
        let base_rel = match sb.resolve(Path::new(base)) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(e.to_string()),
        };
        let files = match walk_files(sb, &base_rel).await {
            Ok(f) => f,
            Err(e) => return ToolResult::error(e),
        };
        // Patterns match relative to the search directory; output is root-relative.
        let mut hits: Vec<String> = files
            .iter()
            .filter(|f| {
                let rel = f.strip_prefix(&base_rel).unwrap_or(f);
                matcher.is_match(rel)
            })
            .map(|f| display_rel(sb, f))
            .collect();
        if hits.is_empty() {
            return ToolResult::ok("no files matched");
        }
        hits.sort();
        let extra = hits.len().saturating_sub(MAX_RESULTS);
        hits.truncate(MAX_RESULTS);
        let mut out = hits.join("\n");
        if extra > 0 {
            out.push_str(&format!("\n[{extra} more files not shown]"));
        }
        ToolResult::ok(out)
    }
}
