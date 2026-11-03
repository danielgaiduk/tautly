//! The `edit` tool: stale-read guard, tolerant matching, atomic write.

pub mod atomic;
pub mod guard;
pub mod str_replace;
pub mod tolerant;

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use hx_provider::{Capabilities, EditFormat, ToolSpec};
use hx_sandbox::{Effect, Sandbox, SandboxError};
use serde::Deserialize;
use serde_json::{Value, json};

use self::str_replace::MatchError;
use self::tolerant::RepairStage;
use crate::spec::{parse_args, tool_spec};
use crate::{AppliedEffect, Tool, ToolCx, ToolError, ToolResult};

/// Why an edit could not be applied.
#[derive(Debug, thiserror::Error)]
pub enum EditError {
    #[error("read the file first (use `read`) before editing it")]
    NotRead,
    #[error("file changed since last read; read it again before editing")]
    Stale,
    #[error("{0}")]
    Undecodable(&'static str),
    #[error(transparent)]
    Sandbox(#[from] SandboxError),
}

pub struct EditTool;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StrReplaceArgs {
    path: String,
    old_text: String,
    new_text: String,
}

fn str_replace_spec() -> ToolSpec {
    tool_spec(
        "edit",
        "Replace one occurrence of old_text with new_text in a file you have read. \
         old_text must identify a single place; include surrounding lines to make it unique.",
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File path."},
                "old_text": {"type": "string", "description": "Text to replace; must match exactly one place."},
                "new_text": {"type": "string", "description": "Replacement text."}
            },
            "required": ["path", "old_text", "new_text"],
            "additionalProperties": false
        }),
    )
}

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &'static str {
        "edit"
    }

    fn spec(&self, caps: &Capabilities) -> ToolSpec {
        match caps.edit_format {
            EditFormat::StrReplace => str_replace_spec(),
            // Task 12 adds the patch and hashline schemas; until then both models are
            // offered the str_replace one.
            EditFormat::Patch | EditFormat::Hashline => str_replace_spec(),
        }
    }

    fn effect(&self, args: &Value) -> Result<Effect, ToolError> {
        let a: StrReplaceArgs = parse_args(args)?;
        Ok(Effect::Write {
            paths: vec![PathBuf::from(a.path)],
        })
    }

    async fn run(&self, args: Value, sb: &dyn Sandbox, cx: &ToolCx) -> ToolResult {
        let a: StrReplaceArgs = match parse_args(&args) {
            Ok(a) => a,
            Err(e) => return ToolResult::error(e.to_string()),
        };
        if a.old_text.is_empty() {
            return ToolResult::error("old_text must not be empty");
        }
        if a.old_text == a.new_text {
            return ToolResult::error("old_text and new_text are identical; nothing to change");
        }
        let full = match sb.resolve(Path::new(&a.path)) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(e.to_string()),
        };
        let (meta, original) = match (sb.metadata(&full).await, sb.read(&full).await) {
            (Ok(m), Ok(b)) => (m, b),
            (Err(e), _) | (_, Err(e)) => return ToolResult::error(format!("{}: {e}", a.path)),
        };
        // Binary files can never be `read`, so say why before asking for a read.
        let text = match atomic::to_lf_text(&original) {
            Ok(t) => t,
            Err(e) => return ToolResult::error(format!("{}: {e}", a.path)),
        };
        if let Err(e) = guard::check_fresh(cx.reads(), &full, &meta, &original) {
            return ToolResult::error(format!("{}: {e}", a.path));
        }
        let (edited, stage) = match str_replace::apply_staged(&text, &a.old_text, &a.new_text) {
            Ok(r) => r,
            Err(e) => return ToolResult::error(match_error_message(&a.path, &e)),
        };
        if let Err(e) = atomic::preserve_and_write(sb, &full, &original, &edited).await {
            return ToolResult::error(format!("{}: {e}", a.path));
        }
        // What was written, so consecutive edits need no re-read.
        let post_hash = blake3::hash(&atomic::restore(&original, &edited));
        if let Ok(meta) = sb.metadata(&full).await {
            cx.reads().record(&full, &meta, post_hash);
        }
        let mut output = format!("edited {}", a.path);
        if stage != RepairStage::Exact {
            output.push_str(&format!("; applied via {}", stage.describe()));
        }
        let mut r = ToolResult::ok(output);
        r.effects.push(AppliedEffect::Edited {
            path: full,
            pre_image: Some(original),
            post_hash,
        });
        r
    }
}

fn match_error_message(path: &str, e: &MatchError) -> String {
    match e {
        MatchError::Ambiguous { lines } => {
            let lines: Vec<String> = lines.iter().map(usize::to_string).collect();
            format!(
                "{path}: old_text matches {} places (starting at lines {}); \
                 include more surrounding lines to make it unique",
                lines.len(),
                lines.join(", ")
            )
        }
        MatchError::NotFound {
            candidates,
            nearest,
        } => {
            let mut msg = format!("{path}: old_text not found");
            if !candidates.is_empty() {
                let lines: Vec<String> = candidates.iter().map(usize::to_string).collect();
                msg.push_str(&format!("; similar text near lines {}", lines.join(", ")));
            }
            if let Some((start, window)) = nearest {
                msg.push_str(&format!(
                    "\nnearest text (from line {start}):\n{window}\nRetry with old_text copied from these lines."
                ));
            }
            msg
        }
    }
}
