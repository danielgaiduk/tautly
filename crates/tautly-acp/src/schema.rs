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
    /// Reads a file or other content.
    Read,
    /// Edits a file.
    Edit,
    /// Deletes a file.
    Delete,
    /// Moves or renames a file.
    Move,
    /// Searches files or the code index.
    Search,
    /// Runs a command.
    Execute,
    /// Internal reasoning or planning.
    Think,
    /// Fetches from the network.
    Fetch,
    /// Anything else.
    Other,
}

/// Where a tool call is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallStatus {
    /// Not started yet (waiting for input or approval).
    Pending,
    /// Running.
    InProgress,
    /// Finished successfully.
    Completed,
    /// Finished with an error, or refused.
    Failed,
}

/// Why a prompt turn ended (`session/prompt` result).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// The model finished its answer.
    EndTurn,
    /// The model hit its output token limit.
    MaxTokens,
    /// The turn hit its step limit (`max_turns`).
    MaxTurnRequests,
    /// The model refused to continue.
    Refusal,
    /// The client cancelled the turn.
    Cancelled,
}

/// A block of a prompt or of a message chunk.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    /// Plain text.
    Text {
        /// The text.
        text: String,
    },
    /// A base64 image.
    Image {
        /// Base64 image data.
        data: String,
        /// The image's MIME type.
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
    /// Base64 audio.
    Audio {
        /// Base64 audio data.
        data: String,
        /// The audio's MIME type.
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
    /// A link to a resource the client can read.
    ResourceLink {
        /// The resource's URI.
        uri: String,
        /// A display name.
        name: String,
    },
    /// A resource embedded in the prompt.
    Resource {
        /// Its contents.
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
    /// The resource's URI.
    pub uri: String,
    /// Text contents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Base64 binary contents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blob: Option<String>,
    /// The contents' MIME type.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
}

impl ContentBlock {
    /// A text block.
    pub fn text(text: impl Into<String>) -> ContentBlock {
        ContentBlock::Text { text: text.into() }
    }
}

/// What a tool call shows: content (text) or a file diff.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolCallContent {
    /// Content blocks to show.
    Content {
        /// The block.
        content: ContentBlock,
    },
    /// `old_text` None: the file is new.
    Diff {
        /// The file (absolute).
        path: PathBuf,
        /// The text before the change.
        #[serde(rename = "oldText")]
        old_text: Option<String>,
        /// The text after the change.
        #[serde(rename = "newText")]
        new_text: String,
    },
}

impl ToolCallContent {
    /// A text content item.
    pub fn text(text: impl Into<String>) -> ToolCallContent {
        ToolCallContent::Content {
            content: ContentBlock::text(text),
        }
    }
}

/// A file a tool call works on (absolute path).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallLocation {
    /// The file (absolute).
    pub path: PathBuf,
    /// The line, 1-based, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
}

/// A `tool_call` update: a new tool call.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
    /// The id later updates refer to.
    pub tool_call_id: String,
    /// A one-line title.
    pub title: String,
    /// What the call does.
    pub kind: ToolKind,
    /// Where it is in its life.
    pub status: ToolCallStatus,
    /// What it shows so far.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub content: Vec<ToolCallContent>,
    /// The files it works on.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub locations: Vec<ToolCallLocation>,
    /// The tool's input as the model sent it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_input: Option<Value>,
}

/// A `tool_call_update` (and the `toolCall` of a permission request): only the fields
/// that changed.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallUpdate {
    /// The tool call this updates.
    pub tool_call_id: String,
    /// A new title.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// A new kind.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<ToolKind>,
    /// A new status.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<ToolCallStatus>,
    /// Replaces what the call shows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<Vec<ToolCallContent>>,
    /// Replaces the files it works on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub locations: Option<Vec<ToolCallLocation>>,
    /// The tool's input.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_input: Option<Value>,
}

/// What choosing a [`PermissionOption`] means.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionOptionKind {
    /// Allow this call.
    AllowOnce,
    /// Allow this and similar calls from now on.
    AllowAlways,
    /// Refuse this call.
    RejectOnce,
    /// Refuse this and similar calls from now on.
    RejectAlways,
}

/// One answer a permission request offers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionOption {
    /// The id the client sends back.
    pub option_id: String,
    /// The label shown to the user.
    pub name: String,
    /// What choosing it means.
    pub kind: PermissionOptionKind,
}

/// The `outcome` of a `session/request_permission` response.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum PermissionOutcome {
    /// The prompt turn was cancelled before the user answered.
    Cancelled,
    /// The user chose an option.
    Selected {
        /// The chosen [`PermissionOption::option_id`].
        #[serde(rename = "optionId")]
        option_id: String,
    },
}

/// A `session/request_permission` result.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PermissionResponse {
    /// How the request ended.
    pub outcome: PermissionOutcome,
}

/// `initialize` params.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams {
    /// The highest protocol version the client speaks.
    pub protocol_version: u16,
    /// What the client supports, as sent.
    #[serde(default)]
    pub client_capabilities: Value,
}

impl InitializeParams {
    /// The client presents `notice` session updates: `clientCapabilities.session.notices` is
    /// an object (as in the official schema; `true` or a string is not).
    pub fn notices(&self) -> bool {
        self.client_capabilities["session"]["notices"].is_object()
    }
}

/// An MCP server the client passes to `session/new` or `session/load`: `http` and `sse`
/// carry a `type`, stdio servers none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpServer {
    /// A child process speaking MCP on stdin and stdout.
    Stdio {
        /// The server's name.
        name: String,
        /// The program to run.
        command: String,
        /// Its arguments.
        args: Vec<String>,
        /// Environment variables to set, as name and value.
        env: Vec<(String, String)>,
    },
    /// A Streamable HTTP server.
    Http {
        /// The server's name.
        name: String,
        /// The endpoint URL.
        url: String,
        /// HTTP headers, as name and value.
        headers: Vec<(String, String)>,
    },
    /// An SSE server (unsupported: reported and skipped).
    Sse {
        /// The server's name.
        name: String,
    },
}

impl McpServer {
    /// The server's name, whatever its transport.
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
    /// The work tree the session runs in (absolute).
    pub cwd: PathBuf,
    /// MCP servers the client adds for this session.
    #[serde(default)]
    pub mcp_servers: Vec<McpServer>,
}

/// `session/load` params.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoadSessionParams {
    /// The session to load.
    pub session_id: String,
    /// The work tree the session runs in (absolute).
    pub cwd: PathBuf,
    /// MCP servers the client adds for this session.
    #[serde(default)]
    pub mcp_servers: Vec<McpServer>,
}

/// `session/prompt` params.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptParams {
    /// The session the prompt goes to.
    pub session_id: String,
    /// The user's message.
    pub prompt: Vec<ContentBlock>,
}

/// `session/cancel` params.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CancelParams {
    /// The session whose turn to cancel.
    pub session_id: String,
}

/// `session/set_config_option` params; `value` is a value id (a string) for a select.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetConfigParams {
    /// The session to change.
    pub session_id: String,
    /// The option to set: `model` or `reasoning`.
    pub config_id: String,
    /// The new value.
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
