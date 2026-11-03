//! One server tool as a Tautly [`Tool`].

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rmcp::model::{CallToolRequestParams, CallToolResponse, CallToolResult};
use serde_json::Value;
use tautly_provider::{Capabilities, ToolSpec};
use tautly_sandbox::{Effect, Sandbox};
use tautly_tools::{AppliedEffect, Tool, ToolCx, ToolError, ToolResult, cap_output};

use crate::client::Connection;

/// Longest description sent to the model, in bytes.
pub const DESCRIPTION_CAP: usize = 1024;
/// A tool whose input schema is larger than this (serialized JSON, bytes) is skipped.
pub const SCHEMA_CAP: usize = 8 * 1024;
/// Results are capped (head + tail) at this many bytes, with the rest spilled.
pub const RESULT_CAP: usize = 30 * 1024;

/// A tool as the server listed it, already within the caps.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RemoteTool {
    pub name: String,
    pub description: String,
    pub schema: Value,
}

/// `mcp__<server>__<tool>`: calls `remote` on the server behind `conn`.
pub(crate) struct McpTool {
    pub name: String,
    pub remote: RemoteTool,
    pub conn: Arc<Connection>,
    pub timeout: Duration,
}

/// `s` cut to at most `cap` bytes on a character boundary.
pub(crate) fn truncate(s: &str, cap: usize) -> String {
    if s.len() <= cap {
        return s.to_string();
    }
    let mut end = cap;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn spec(&self, _caps: &Capabilities) -> ToolSpec {
        ToolSpec {
            name: self.name.clone(),
            description: self.remote.description.clone(),
            schema: self.remote.schema.clone(),
        }
    }

    /// The same for every call: the server's launch (stdio) or its host (HTTP).
    fn effect(&self, args: &Value) -> Result<Effect, ToolError> {
        if !args.is_object() {
            return Err(ToolError::BadArgs("the arguments must be an object".into()));
        }
        Ok(self.conn.effect.clone())
    }

    async fn run(&self, args: Value, sb: &dyn Sandbox, cx: &ToolCx) -> ToolResult {
        let Value::Object(arguments) = args else {
            return ToolResult::error("invalid arguments: the arguments must be an object");
        };
        let mut params = CallToolRequestParams::new(self.remote.name.clone());
        params.arguments = Some(arguments);
        let call = self.conn.peer.call_tool_once(params);
        let outcome = tokio::select! {
            r = tokio::time::timeout(self.timeout, call) => r,
            () = cx.cancel().cancelled() => {
                return self.finish(ToolResult::error("cancelled: the turn was cancelled during this MCP call"));
            }
        };
        let result = match outcome {
            Err(_) => ToolResult::error_kind(
                format!(
                    "the MCP call timed out after {} s (server `{}`)",
                    self.timeout.as_secs(),
                    self.conn.server
                ),
                "timeout",
            ),
            Ok(Err(e)) => ToolResult::error_kind(
                format!("the MCP call failed (server `{}`): {e}", self.conn.server),
                "mcp_error",
            ),
            Ok(Ok(CallToolResponse::Complete(r))) => {
                let (text, is_error) = map_result(&r);
                let (mut text, spilled) = cap_output(&text, RESULT_CAP, sb).await;
                if let Some(p) = spilled {
                    if !text.ends_with('\n') {
                        text.push('\n');
                    }
                    text.push_str(&format!("[full output saved to {}]", p.display()));
                }
                let mut out = ToolResult::ok(text);
                out.is_error = is_error;
                if is_error {
                    out.meta.error_kind = Some("tool_error");
                }
                out
            }
            // Input requests (elicitation, sampling) and tasks are not implemented.
            Ok(Ok(_)) => ToolResult::error_kind(
                format!(
                    "server `{}` asked for client input or started a task, which Tautly does \
                     not support; the call did not complete",
                    self.conn.server
                ),
                "unsupported",
            ),
        };
        self.finish(result)
    }
}

impl McpTool {
    /// A stdio server is a local process that may have changed any file.
    fn finish(&self, mut r: ToolResult) -> ToolResult {
        if self.conn.may_write {
            r.effects.push(AppliedEffect::Exec { may_write: true });
        }
        r
    }
}

/// The text the model sees, and whether the server flagged an error: text blocks as they
/// are, `structuredContent` as JSON when there is no text, images and audio as a note,
/// resource links and embedded resources as text. Blocks are joined by newlines.
pub(crate) fn map_result(r: &CallToolResult) -> (String, bool) {
    let mut parts = Vec::new();
    let mut has_text = false;
    for block in &r.content {
        let Ok(v) = serde_json::to_value(block) else {
            continue;
        };
        let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("");
        match s("type") {
            "text" => {
                has_text = true;
                parts.push(s("text").to_string());
            }
            "image" | "audio" => parts.push(format!(
                "[{}: {}, {} bytes base64, not shown]",
                s("type"),
                s("mimeType"),
                s("data").len()
            )),
            "resource_link" => {
                let name = s("name");
                let uri = s("uri");
                parts.push(if name.is_empty() {
                    format!("[resource link: <{uri}>]")
                } else {
                    format!("[resource link: {name} <{uri}>]")
                });
            }
            "resource" => {
                let res = v.get("resource").cloned().unwrap_or(Value::Null);
                let uri = res.get("uri").and_then(Value::as_str).unwrap_or("");
                match res.get("text").and_then(Value::as_str) {
                    Some(text) => {
                        has_text = true;
                        parts.push(format!("[resource <{uri}>]\n{text}"));
                    }
                    None => parts.push(format!("[resource <{uri}>: binary, not shown]")),
                }
            }
            other => parts.push(format!("[{other} content, not shown]")),
        }
    }
    if !has_text {
        if let Some(sc) = &r.structured_content {
            parts.insert(0, sc.to_string());
        }
    }
    (parts.join("\n"), r.is_error.unwrap_or(false))
}
