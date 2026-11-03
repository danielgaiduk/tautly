use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tautly_provider::{Capabilities, ToolSpec};
use tautly_sandbox::{Effect, Sandbox, SandboxError};

use crate::preview::{
    DiffPreview, Preview, check_approved, display_diff, explain_failure, shown_path,
};
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
        let (full, pre_image, pre_mode) = match load(&a, sb).await {
            Ok(loaded) => loaded,
            Err(r) => return explain_failure(sb, cx, r).await,
        };
        let base = (full.clone(), pre_image.as_deref().map(blake3::hash));
        if let Err(r) = check_approved(sb, cx, &[base]) {
            return r;
        }
        // The commit point: a cancel seen before it changes nothing; after it, the write
        // is reported in full (the engine waits for it rather than abandon it).
        if cx.cancel().is_cancelled() {
            return ToolResult::error(crate::CANCELLED_BEFORE_WRITE);
        }
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
        let shown = shown_path(sb, &full, &a.path);
        let diff = display_diff(&[(shown, pre_image.as_deref(), Some(a.content.as_bytes()))]).await;
        let mut r = ToolResult::ok(output);
        r.applied_diff = Some(Box::new(diff));
        r.effects.push(AppliedEffect::Edited {
            path: full,
            pre_image,
            pre_mode,
            post_hash,
        });
        r
    }

    async fn preview(
        &self,
        args: &Value,
        sb: &dyn Sandbox,
        cx: &ToolCx,
    ) -> Option<Result<Preview, ToolResult>> {
        let a: Args = match parse_args(args) {
            Ok(a) => a,
            Err(e) => return Some(Err(ToolResult::error(e.to_string()))),
        };
        let (full, pre_image, _) = match load(&a, sb).await {
            Ok(loaded) => loaded,
            Err(r) => return Some(Err(r)),
        };
        let diff = if cx.preview_diff() {
            let shown = shown_path(sb, &full, &a.path);
            display_diff(&[(shown, pre_image.as_deref(), Some(a.content.as_bytes()))]).await
        } else {
            DiffPreview::default()
        };
        let base = pre_image.as_deref().map(blake3::hash);
        Some(Ok(Preview {
            diff,
            bases: vec![(full, base)],
        }))
    }
}

/// The resolved path and the file as it is (pre-image and permission bits; None when it
/// does not exist yet). Errors are ready to return.
async fn load(
    a: &Args,
    sb: &dyn Sandbox,
) -> Result<(PathBuf, Option<Vec<u8>>, Option<u32>), ToolResult> {
    let full = sb
        .resolve(Path::new(&a.path))
        .map_err(|e| ToolResult::error(e.to_string()))?;
    let pre_image = match sb.read(&full).await {
        Ok(b) => Some(b),
        Err(SandboxError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(ToolResult::error(format!("{}: {e}", a.path))),
    };
    let pre_mode = match &pre_image {
        Some(_) => match sb.metadata(&full).await {
            Ok(m) => Some(m.mode & 0o7777),
            Err(e) => return Err(ToolResult::error(format!("{}: {e}", a.path))),
        },
        None => None,
    };
    Ok((full, pre_image, pre_mode))
}
