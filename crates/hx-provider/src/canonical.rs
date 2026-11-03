use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// `provider/model` identifier, or `compat:<name>/model` for OpenAI-compatible endpoints.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ModelId(String);

impl ModelId {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Provider prefix: text before the first `/`, truncated at `:` (so `compat:x/m` -> `compat`).
    pub fn provider(&self) -> &str {
        let head = self.0.split('/').next().unwrap_or(&self.0);
        head.split(':').next().unwrap_or(head)
    }

    /// Model name: text after the first `/` (the whole id if there is no `/`).
    pub fn model(&self) -> &str {
        self.0.split_once('/').map_or(self.0.as_str(), |(_, m)| m)
    }
}

impl From<&str> for ModelId {
    fn from(s: &str) -> Self {
        ModelId(s.to_string())
    }
}

impl From<String> for ModelId {
    fn from(s: String) -> Self {
        ModelId(s)
    }
}

impl std::fmt::Display for ModelId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

mod b64 {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(data: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&STANDARD.encode(data))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        STANDARD.decode(s).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effort {
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reasoning {
    Budget(u32),
    Effort(Effort),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CachePoint {
    AfterSystem,
    AfterTools,
    AfterMessage(usize),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Block {
    Text(String),
    Image {
        media_type: String,
        #[serde(with = "b64")]
        data: Vec<u8>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Content {
    Text(String),
    Image {
        media_type: String,
        #[serde(with = "b64")]
        data: Vec<u8>,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        id: String,
        output: String,
        is_error: bool,
    },
    Reasoning {
        text: String,
        signature: Option<String>,
    },
    /// Reasoning the provider returned encrypted (Anthropic `redacted_thinking`). Opaque:
    /// sent back verbatim to the provider that produced it, dropped by every other adapter.
    RedactedReasoning {
        data: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<Content>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub schema: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CanonicalRequest {
    pub model: ModelId,
    pub system: Vec<Block>,
    pub tools: Vec<ToolSpec>,
    pub messages: Vec<Message>,
    pub max_output: u32,
    pub reasoning: Option<Reasoning>,
    pub cache_breakpoints: Vec<CachePoint>,
    /// The last message of `messages` is the volatile tail (the working state, which
    /// changes on almost every request) and sits after the last cache breakpoint. Adapters
    /// may use this to place the tail differently, for example inside the last tool result;
    /// today none does, and a request without a tail leaves it `false`.
    #[serde(default)]
    pub volatile_tail: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    StopSequence,
    Refusal,
    Other(String),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawUsage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Quota {
    pub remaining_requests: Option<u64>,
    pub remaining_tokens: Option<u64>,
    pub reset_at: Option<SystemTime>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderEvent {
    TextDelta(String),
    ReasoningDelta(String),
    /// Signature of the thinking block just streamed; send it back with the reasoning text.
    ReasoningSignature(String),
    /// An encrypted reasoning block (opaque data); keep it as
    /// [`Content::RedactedReasoning`] and send it back unchanged.
    RedactedReasoning(String),
    ToolCall {
        id: String,
        name: String,
        input_json: String,
    },
    Usage(RawUsage),
    Quota(Quota),
    Done {
        stop: StopReason,
    },
}
