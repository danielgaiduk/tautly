use std::path::Path;

use async_trait::async_trait;
use globset::GlobBuilder;
use hx_provider::{Capabilities, ToolSpec};
use hx_sandbox::{Effect, Sandbox};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::cap::{OUTPUT_CAP, cap_text};
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
pub(crate) fn build_glob(pattern: &str) -> Result<globset::GlobMatcher, globset::Error> {
    let glob = GlobBuilder::new(&anchor(pattern))
        .literal_separator(true)
        .build()?;
    Ok(glob.compile_matcher())
}

fn anchor(pattern: &str) -> String {
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

    async fn run(&self, args: Value, sb: &dyn Sandbox, cx: &ToolCx) -> ToolResult {
        let a: Args = match parse_args(&args) {
            Ok(a) => a,
            Err(e) => return ToolResult::error(e.to_string()),
        };
        let matcher = match build_glob(&a.pattern) {
            Ok(m) => m,
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
        let mut hits: Vec<String> = Vec::new();
        for f in &files {
            if cx.cancel().is_cancelled() {
                return ToolResult::error("cancelled");
            }
            if matcher.is_match(f.strip_prefix(&base_rel).unwrap_or(f)) {
                hits.push(display_rel(sb, f));
            }
        }
        if hits.is_empty() {
            return ToolResult::ok("no files matched");
        }
        hits.sort();
        let extra = hits.len().saturating_sub(MAX_RESULTS);
        hits.truncate(MAX_RESULTS);
        let (mut out, truncated) = cap_text(&hits.join("\n"), OUTPUT_CAP);
        if extra > 0 {
            out.push_str(&format!("\n[{extra} more files not shown]"));
        }
        if truncated || extra > 0 {
            out.push_str("\n[output truncated; narrow the pattern]");
        }
        ToolResult::ok(out)
    }
}
