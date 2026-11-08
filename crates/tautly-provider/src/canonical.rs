use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// `provider/model` identifier, or `compat:<name>/model` for OpenAI-compatible endpoints.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ModelId(String);

impl ModelId {
    /// The id as written.
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

/// Who a message is from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// System instructions.
    System,
    /// The user, or tool results.
    User,
    /// The model.
    Assistant,
}

/// A reasoning effort level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effort {
    /// Little thinking.
    Low,
    /// Moderate thinking.
    Medium,
    /// Much thinking.
    High,
}

impl Effort {
    /// The thinking-token budget this level means for budget-style models (spec 4.7):
    /// 4k, 12k, 32k. Adapters cap it below the request's output limit.
    pub fn budget_tokens(self) -> u32 {
        match self {
            Effort::Low => 4_096,
            Effort::Medium => 12_288,
            Effort::High => 32_768,
        }
    }
}

/// How much a request asks the model to think.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reasoning {
    /// A thinking-token budget (budget-style models).
    Budget(u32),
    /// An effort level (effort-style models).
    Effort(Effort),
}

/// Where a prompt-cache breakpoint goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CachePoint {
    /// After the system blocks.
    AfterSystem,
    /// After the tool definitions.
    AfterTools,
    /// After message `n` (0-based).
    AfterMessage(usize),
}

/// One block of the system prompt.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Block {
    /// Text.
    Text(String),
    /// An image.
    Image {
        /// Its MIME type.
        media_type: String,
        /// Its bytes (base64 when serialized).
        #[serde(with = "b64")]
        data: Vec<u8>,
    },
}

/// One part of a message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Content {
    /// Text.
    Text(String),
    /// An image.
    Image {
        /// Its MIME type.
        media_type: String,
        /// Its bytes (base64 when serialized).
        #[serde(with = "b64")]
        data: Vec<u8>,
    },
    /// A tool call the model made.
    ToolUse {
        /// The call id.
        id: String,
        /// The tool's name.
        name: String,
        /// The arguments.
        input: Value,
    },
    /// The result of a tool call.
    ToolResult {
        /// The id of the call it answers.
        id: String,
        /// The output the model sees.
        output: String,
        /// Whether the call failed.
        is_error: bool,
    },
    /// The model's thinking text.
    Reasoning {
        /// The text.
        text: String,
        /// The provider's signature, sent back with the text (Anthropic).
        signature: Option<String>,
    },
    /// Reasoning the provider returned encrypted (Anthropic `redacted_thinking`). Opaque:
    /// sent back verbatim to the provider that produced it, dropped by every other adapter.
    RedactedReasoning {
        /// The opaque data, as received.
        data: String,
    },
}

/// One message of the conversation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    /// Who it is from.
    pub role: Role,
    /// Its parts, in order.
    pub content: Vec<Content>,
}

/// A tool as offered to the model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    /// The tool's name.
    pub name: String,
    /// What it does, for the model.
    pub description: String,
    /// The JSON Schema of its arguments.
    pub schema: Value,
}

/// A request in provider-neutral form; each adapter translates it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CanonicalRequest {
    /// The model.
    pub model: ModelId,
    /// The system prompt.
    pub system: Vec<Block>,
    /// The tools offered.
    pub tools: Vec<ToolSpec>,
    /// The conversation.
    pub messages: Vec<Message>,
    /// Most output tokens to generate.
    pub max_output: u32,
    /// How much to think (None: no reasoning).
    pub reasoning: Option<Reasoning>,
    /// Where to put prompt-cache breakpoints (adapters with explicit caching).
    pub cache_breakpoints: Vec<CachePoint>,
    /// The last message of `messages` is the volatile tail (the working state, which
    /// changes on almost every request) and sits after the last cache breakpoint. Adapters
    /// may use this to place the tail differently, for example inside the last tool result;
    /// today none does, and a request without a tail leaves it `false`.
    #[serde(default)]
    pub volatile_tail: bool,
}

/// Why a response ended. Persisted in session logs (`Done`): a reason this version does
/// not know reads as [`StopReason::Unknown`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(remote = "Self", rename_all = "snake_case")]
pub enum StopReason {
    /// The model finished.
    EndTurn,
    /// The model wants tool results.
    ToolUse,
    /// The output token limit was reached.
    MaxTokens,
    /// A stop sequence was produced.
    StopSequence,
    /// The model refused.
    Refusal,
    /// Another reason, as the provider named it.
    Other(String),
    /// A stop reason this version does not know (read back from a newer log).
    Unknown,
}

/// Token usage as a provider reports it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawUsage {
    /// Input tokens not read from the cache.
    pub input: u64,
    /// Output tokens (thinking included).
    pub output: u64,
    /// Input tokens read from the prompt cache.
    pub cache_read: u64,
    /// Input tokens written to the prompt cache.
    pub cache_write: u64,
}

/// Rate-limit headroom a provider reported.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Quota {
    /// Requests left in the window.
    pub remaining_requests: Option<u64>,
    /// Tokens left in the window.
    pub remaining_tokens: Option<u64>,
    /// When the window resets.
    pub reset_at: Option<SystemTime>,
}

/// One item of a streamed response. Persisted in session logs: an event type this version
/// does not know reads as [`ProviderEvent::Unknown`] (match with a wildcard arm).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(remote = "Self", rename_all = "snake_case")]
pub enum ProviderEvent {
    /// Text of the answer.
    TextDelta(String),
    /// Thinking text.
    ReasoningDelta(String),
    /// Signature of the thinking block just streamed; send it back with the reasoning text.
    ReasoningSignature(String),
    /// An encrypted reasoning block (opaque data); keep it as
    /// [`Content::RedactedReasoning`] and send it back unchanged.
    RedactedReasoning(String),
    /// A complete tool call.
    ToolCall {
        /// The call id.
        id: String,
        /// The tool's name.
        name: String,
        /// The arguments, as the JSON text the model wrote.
        input_json: String,
    },
    /// Usage so far of this response, cumulative: a later `Usage` replaces an earlier one
    /// (Anthropic reports the input at the start and the total at the end).
    Usage(RawUsage),
    /// Usage of an earlier attempt that [`crate::Retrying`] discarded and retried: billed,
    /// but it describes no response. Add it to the cost; never calibrate with it.
    DiscardedUsage(RawUsage),
    /// Rate-limit headroom.
    Quota(Quota),
    /// The response ended.
    Done {
        /// Why.
        stop: StopReason,
    },
    /// An event this version does not know (read back from a newer log); ignored.
    Unknown,
}

/// The variant names [`StopReason`] reads.
const STOP_REASONS: &[&str] = &[
    "end_turn",
    "tool_use",
    "max_tokens",
    "stop_sequence",
    "refusal",
    "other",
    "unknown",
];

/// The variant names [`ProviderEvent`] reads.
const PROVIDER_EVENTS: &[&str] = &[
    "text_delta",
    "reasoning_delta",
    "reasoning_signature",
    "redacted_reasoning",
    "tool_call",
    "usage",
    "discarded_usage",
    "quota",
    "done",
    "unknown",
];

/// The variant name of an externally tagged value: the string itself (a unit variant), or
/// the key of a one-key object.
fn external_tag(value: &Value) -> Option<&str> {
    match value {
        Value::String(s) => Some(s),
        Value::Object(m) if m.len() == 1 => m.keys().next().map(String::as_str),
        _ => None,
    }
}

/// Deserialize an externally tagged enum whose derive is `remote = "Self"`: a variant name
/// not in `known` is `unknown`; anything else (a bad payload, a value that is no variant)
/// is left to the derived impl, and fails as before.
fn tolerant<'de, D, T>(
    d: D,
    known: &[&str],
    unknown: T,
    derived: impl FnOnce(Value) -> Result<T, serde_json::Error>,
) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Value::deserialize(d)?;
    match external_tag(&value) {
        Some(tag) if !known.contains(&tag) => Ok(unknown),
        _ => derived(value).map_err(serde::de::Error::custom),
    }
}

impl Serialize for StopReason {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        StopReason::serialize(self, s)
    }
}

impl<'de> Deserialize<'de> for StopReason {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        tolerant(
            d,
            STOP_REASONS,
            StopReason::Unknown,
            StopReason::deserialize,
        )
    }
}

impl Serialize for ProviderEvent {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        ProviderEvent::serialize(self, s)
    }
}

impl<'de> Deserialize<'de> for ProviderEvent {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        tolerant(
            d,
            PROVIDER_EVENTS,
            ProviderEvent::Unknown,
            ProviderEvent::deserialize,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_reasons_from_a_newer_version_read_as_unknown() {
        for text in [
            r#""paused""#,
            r#"{"paused":{"for":3}}"#,
            r#"{"paused":"x"}"#,
        ] {
            assert_eq!(
                serde_json::from_str::<StopReason>(text).unwrap(),
                StopReason::Unknown,
                "{text}"
            );
        }
        let known = [
            StopReason::EndTurn,
            StopReason::ToolUse,
            StopReason::MaxTokens,
            StopReason::StopSequence,
            StopReason::Refusal,
            StopReason::Other("x".into()),
            StopReason::Unknown,
        ];
        // A variant added without a sample above fails to compile here; one missing from
        // `STOP_REASONS` fails the round trip.
        for r in &known {
            match r {
                StopReason::EndTurn
                | StopReason::ToolUse
                | StopReason::MaxTokens
                | StopReason::StopSequence
                | StopReason::Refusal
                | StopReason::Other(_)
                | StopReason::Unknown => {}
            }
        }
        for r in known {
            let text = serde_json::to_string(&r).unwrap();
            assert_eq!(serde_json::from_str::<StopReason>(&text).unwrap(), r);
        }
        // A known reason with a bad payload is still an error.
        assert!(serde_json::from_str::<StopReason>(r#"{"other":7}"#).is_err());
        assert!(serde_json::from_str::<StopReason>("7").is_err());
    }

    #[test]
    fn provider_events_from_a_newer_version_read_as_unknown() {
        for text in [
            r#""from_the_future""#,
            r#"{"from_the_future":{"a":1}}"#,
            r#"{"from_the_future":[1,2]}"#,
        ] {
            assert_eq!(
                serde_json::from_str::<ProviderEvent>(text).unwrap(),
                ProviderEvent::Unknown,
                "{text}"
            );
        }
        let done: ProviderEvent = serde_json::from_str(r#"{"done":{"stop":"paused"}}"#).unwrap();
        assert_eq!(
            done,
            ProviderEvent::Done {
                stop: StopReason::Unknown
            }
        );
        let usage = RawUsage {
            input: 1,
            output: 2,
            cache_read: 3,
            cache_write: 4,
        };
        let known = [
            ProviderEvent::TextDelta("t".into()),
            ProviderEvent::ReasoningDelta("r".into()),
            ProviderEvent::ReasoningSignature("s".into()),
            ProviderEvent::RedactedReasoning("d".into()),
            ProviderEvent::ToolCall {
                id: "c".into(),
                name: "n".into(),
                input_json: "{}".into(),
            },
            ProviderEvent::Usage(usage),
            ProviderEvent::DiscardedUsage(usage),
            ProviderEvent::Quota(Quota::default()),
            ProviderEvent::Done {
                stop: StopReason::EndTurn,
            },
            ProviderEvent::Unknown,
        ];
        for e in &known {
            match e {
                ProviderEvent::TextDelta(_)
                | ProviderEvent::ReasoningDelta(_)
                | ProviderEvent::ReasoningSignature(_)
                | ProviderEvent::RedactedReasoning(_)
                | ProviderEvent::ToolCall { .. }
                | ProviderEvent::Usage(_)
                | ProviderEvent::DiscardedUsage(_)
                | ProviderEvent::Quota(_)
                | ProviderEvent::Done { .. }
                | ProviderEvent::Unknown => {}
            }
        }
        for e in known {
            let text = serde_json::to_string(&e).unwrap();

            assert_eq!(serde_json::from_str::<ProviderEvent>(&text).unwrap(), e);
        }
        assert!(serde_json::from_str::<ProviderEvent>(r#"{"usage":"x"}"#).is_err());
    }
}
