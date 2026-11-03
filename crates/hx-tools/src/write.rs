use std::path::{Path, PathBuf};

use async_trait::async_trait;
use hx_provider::{Capabilities, ToolSpec};
use hx_sandbox::{Effect, Sandbox, SandboxError};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::spec::{parse_args, tool_spec};
use crate::{AppliedEffect, Tool, ToolCx, ToolError, ToolResult};

pub struct WriteTool;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    path: String,
    content: String,
}

#[async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &'static str {
        "write"
    }

    fn spec(&self, _caps: &Capabilities) -> ToolSpec {
        tool_spec(
            "write",
            "Create or overwrite a file with the given content.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path."},
                    "content": {"type": "string", "description": "Full file content."}
                },
                "required": ["path", "content"],
                "additionalProperties": false
            }),
        )
    }

    fn effect(&self, args: &Value) -> Result<Effect, ToolError> {
        let a: Args = parse_args(args)?;
        Ok(Effect::Write {
            paths: vec![PathBuf::from(a.path)],
        })
    }

    async fn run(&self, args: Value, sb: &dyn Sandbox, cx: &ToolCx) -> ToolResult {
        let a: Args = match parse_args(&args) {
            Ok(a) => a,
            Err(e) => return ToolResult::error(e.to_string()),
        };
        let full = match sb.resolve(Path::new(&a.path)) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(e.to_string()),
        };
        let pre_image = match sb.read(&full).await {
            Ok(b) => Some(b),
            Err(SandboxError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return ToolResult::error(format!("{}: {e}", a.path)),
        };
        // The requested path, not `full`: the sandbox refuses symlinks into protected paths.
        if let Err(e) = sb
            .write_atomic(Path::new(&a.path), a.content.as_bytes(), None)
            .await
        {
            return ToolResult::error(format!("{}: {e}", a.path));
        }
        let post_hash = blake3::hash(a.content.as_bytes());
        if let Ok(meta) = sb.metadata(&full).await {
            crate::record_written_file(
                sb,
                cx,
                &full,
                &meta,
                a.content.as_bytes(),
                crate::WrittenBy::Write,
            )
            .await;
        }
        let verb = if pre_image.is_some() {
            "overwrote"
        } else {
            "created"
        };
        let mut output = format!("{verb} {} ({} bytes)", a.path, a.content.len());
        if let Some(note) = crate::edit::reindex(cx, &a.path).await {
            output.push('\n');
            output.push_str(&note);
        }
        let mut r = ToolResult::ok(output);
        r.effects.push(AppliedEffect::Edited {
            path: full,
            pre_image,
            post_hash,
        });
        r
    }
}
