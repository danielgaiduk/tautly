use std::path::Path;

use async_trait::async_trait;
use globset::Glob;
use hx_provider::{Capabilities, ToolSpec};
use hx_sandbox::{Effect, Sandbox};
use regex::RegexBuilder;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::spec::{decode_text, display_rel, parse_args, tool_spec, walk_files};
use crate::{Tool, ToolCx, ToolError, ToolResult};

const MAX_MATCHES: usize = 500;
const MAX_BYTES: usize = 30 * 1024;

pub struct GrepTool;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    pattern: String,
    path: Option<String>,
    glob: Option<String>,
    #[serde(default)]
    case_insensitive: bool,
}

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &'static str {
        "grep"
    }

    fn spec(&self, _caps: &Capabilities) -> ToolSpec {
        tool_spec(
            "grep",
            "Search file contents with a regex. Respects .gitignore. Returns path:line:text.",
            json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "Regex to search for."},
                    "path": {"type": "string", "description": "Directory or file to search. Defaults to the project root."},
                    "glob": {"type": "string", "description": "Only search files matching this glob, e.g. *.rs."},
                    "case_insensitive": {"type": "boolean", "description": "Ignore case."}
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
        let re = match RegexBuilder::new(&a.pattern)
            .case_insensitive(a.case_insensitive)
            .build()
        {
            Ok(r) => r,
            Err(e) => return ToolResult::error(format!("invalid regex: {e}")),
        };
        let matcher = match a
            .glob
            .as_deref()
            .map(|g| Glob::new(&crate::glob::anchor(g)))
        {
            None => None,
            Some(Ok(g)) => Some(g.compile_matcher()),
            Some(Err(e)) => return ToolResult::error(format!("invalid glob: {e}")),
        };
        let base = a.path.as_deref().unwrap_or(".");
        let files = match walk_files(sb, Path::new(base)).await {
            Ok(f) => f,
            Err(e) => return ToolResult::error(e),
        };

        let mut out = String::new();
        let mut count = 0;
        let mut truncated = false;
        'files: for file in files {
            let rel = display_rel(sb, &file);
            if matcher.as_ref().is_some_and(|m| !m.is_match(&rel)) {
                continue;
            }
            let Ok(bytes) = sb.read(&file).await else {
                continue;
            };
            let Ok(text) = decode_text(&bytes) else {
                continue;
            };
            for (i, line) in text.lines().enumerate() {
                if !re.is_match(line) {
                    continue;
                }
                let entry = format!("{rel}:{}:{line}\n", i + 1);
                if count >= MAX_MATCHES || out.len() + entry.len() > MAX_BYTES {
                    truncated = true;
                    break 'files;
                }
                out.push_str(&entry);
                count += 1;
            }
        }
        if count == 0 {
            return ToolResult::ok("no matches");
        }
        out.pop();
        if truncated {
            out.push_str(&format!(
                "\n[truncated at {count} matches; narrow the pattern, path or glob]"
            ));
        }
        ToolResult::ok(out)
    }
}
