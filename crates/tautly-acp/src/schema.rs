//! The part of the Agent Client Protocol v1 wire format the bridge speaks (schema of the
//! `agent-client-protocol-schema` crate 1.12, wire `protocolVersion` 1), as serde types:
//! the requests it reads and the updates, tool calls and permission requests it writes.
//!
//! Field names are camelCase and enums snake_case, as on the wire. Readers are lenient:
//! fields the bridge does not use are ignored, and a content block of a type this version
//! does not know reads as [`ContentBlock::Unknown`].

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// What a tool call does, so a client can pick an icon and a presentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
    Read,
    Edit,
    Delete,
    Move,
    Search,
    Execute,
    Think,
    Fetch,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
}

/// Why a prompt turn ended (`session/prompt` result).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    MaxTurnRequests,
    Refusal,
    Cancelled,
}

/// A block of a prompt or of a message chunk.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    Image {
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
    Audio {
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
    ResourceLink {
        uri: String,
        name: String,
    },
    Resource {
        resource: EmbeddedResource,
    },
    /// A block type this version does not know.
    #[serde(other)]
    Unknown,
}

/// The contents of a `resource` block: text or base64 `blob`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EmbeddedResource {
    pub uri: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blob: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
}

impl ContentBlock {
    pub fn text(text: impl Into<String>) -> ContentBlock {
        ContentBlock::Text { text: text.into() }
    }
}

/// What a tool call shows: content (text) or a file diff.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolCallContent {
    Content {
        content: ContentBlock,
    },
    /// `old_text` None: the file is new.
    Diff {
        path: PathBuf,
        #[serde(rename = "oldText")]
        old_text: Option<String>,
        #[serde(rename = "newText")]
        new_text: String,
    },
}

impl ToolCallContent {
    pub fn text(text: impl Into<String>) -> ToolCallContent {
        ToolCallContent::Content {
            content: ContentBlock::text(text),
        }
    }
}

/// A file a tool call works on (absolute path).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallLocation {
    pub path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
}

/// A `tool_call` update: a new tool call.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
    pub tool_call_id: String,
    pub title: String,
    pub kind: ToolKind,
    pub status: ToolCallStatus,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub content: Vec<ToolCallContent>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub locations: Vec<ToolCallLocation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_input: Option<Value>,
}

/// A `tool_call_update` (and the `toolCall` of a permission request): only the fields
/// that changed.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallUpdate {
    pub tool_call_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<ToolKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<ToolCallStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<Vec<ToolCallContent>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub locations: Option<Vec<ToolCallLocation>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_input: Option<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionOptionKind {
    AllowOnce,
    AllowAlways,
    RejectOnce,
    RejectAlways,
}

/// One answer a permission request offers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionOption {
    pub option_id: String,
    pub name: String,
    pub kind: PermissionOptionKind,
}

/// The `outcome` of a `session/request_permission` response.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum PermissionOutcome {
    Cancelled,
    Selected {
        #[serde(rename = "optionId")]
        option_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PermissionResponse {
    pub outcome: PermissionOutcome,
}

/// `initialize` params.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams {
    pub protocol_version: u16,
    #[serde(default)]
    pub client_capabilities: Value,
}

impl InitializeParams {
    /// The client presents `notice` session updates (`clientCapabilities.session.notices`).
    pub fn notices(&self) -> bool {
        !self.client_capabilities["session"]["notices"].is_null()
    }
}

/// An MCP server the client passes to `session/new` or `session/load`: `http` and `sse`
/// carry a `type`, stdio servers none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpServer {
    Stdio {
        name: String,
        command: String,
        args: Vec<String>,
        env: Vec<(String, String)>,
    },
    Http {
        name: String,
        url: String,
        headers: Vec<(String, String)>,
    },
    Sse {
        name: String,
    },
}

impl McpServer {
    pub fn name(&self) -> &str {
        match self {
            McpServer::Stdio { name, .. }
            | McpServer::Http { name, .. }
            | McpServer::Sse { name } => name,
        }
    }
}

#[derive(Deserialize)]
struct NameValue {
    name: String,
    value: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawMcpServer {
    #[serde(rename = "type")]
    kind: Option<String>,
    name: String,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: Vec<NameValue>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    headers: Vec<NameValue>,
}

impl<'de> Deserialize<'de> for McpServer {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error as _;
        let raw = RawMcpServer::deserialize(d)?;
        let pairs = |v: Vec<NameValue>| v.into_iter().map(|p| (p.name, p.value)).collect();
        Ok(match raw.kind.as_deref() {
            Some("http") => McpServer::Http {
                name: raw.name,
                url: raw.url.ok_or_else(|| D::Error::missing_field("url"))?,
                headers: pairs(raw.headers),
            },
            Some("sse") => McpServer::Sse { name: raw.name },
            Some(other) => {
                return Err(D::Error::custom(format!(
                    "unknown MCP server type `{other}`"
                )));
            }
            None => McpServer::Stdio {
                name: raw.name,
                command: raw
                    .command
                    .ok_or_else(|| D::Error::missing_field("command"))?,
                args: raw.args,
                env: pairs(raw.env),
            },
        })
    }
}

/// `session/new` params.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewSessionParams {
    pub cwd: PathBuf,
    #[serde(default)]
    pub mcp_servers: Vec<McpServer>,
}

/// `session/load` params.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoadSessionParams {
    pub session_id: String,
    pub cwd: PathBuf,
    #[serde(default)]
    pub mcp_servers: Vec<McpServer>,
}

/// `session/prompt` params.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptParams {
    pub session_id: String,
    pub prompt: Vec<ContentBlock>,
}

/// `session/cancel` params.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CancelParams {
    pub session_id: String,
}

/// `session/set_config_option` params; `value` is a value id (a string) for a select.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetConfigParams {
    pub session_id: String,
    pub config_id: String,
    pub value: Value,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn wire_forms() {
        let servers: Vec<McpServer> = serde_json::from_value(json!([
            {"name": "a", "command": "/bin/srv", "args": ["-x"], "env": [{"name": "K", "value": "v"}]},
            {"type": "http", "name": "b", "url": "https://h/mcp", "headers": []},
            {"type": "sse", "name": "c", "url": "https://h/sse", "headers": []},
        ]))
        .unwrap();
        assert_eq!(
            servers[0],
            McpServer::Stdio {
                name: "a".into(),
                command: "/bin/srv".into(),
                args: vec!["-x".into()],
                env: vec![("K".into(), "v".into())],
            }
        );
        assert!(matches!(&servers[1], McpServer::Http { url, .. } if url == "https://h/mcp"));
        assert_eq!(servers[2].name(), "c");

        let blocks: Vec<ContentBlock> = serde_json::from_value(json!([
            {"type": "text", "text": "hi"},
            {"type": "resource", "resource": {"uri": "file:///a.rs", "text": "fn a() {}"}},
            {"type": "video", "data": "x"},
        ]))
        .unwrap();
        assert_eq!(blocks[0], ContentBlock::text("hi"));
        assert_eq!(blocks[2], ContentBlock::Unknown);

        let diff = ToolCallContent::Diff {
            path: "/r/a.rs".into(),
            old_text: None,
            new_text: "x".into(),
        };
        assert_eq!(
            serde_json::to_value(&diff).unwrap(),
            json!({"type": "diff", "path": "/r/a.rs", "oldText": null, "newText": "x"})
        );
        let out: PermissionResponse =
            serde_json::from_value(json!({"outcome": {"outcome": "selected", "optionId": "x"}}))
                .unwrap();
        assert_eq!(
            out.outcome,
            PermissionOutcome::Selected {
                option_id: "x".into()
            }
        );
    }
}
