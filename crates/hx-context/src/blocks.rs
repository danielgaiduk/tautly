//! Plain data carried through assembly: turns, per-block token breakdown, status, and the
//! token weight of messages.

use hx_provider::{Content, Message, ModelId};
use serde::{Deserialize, Serialize};

use crate::tokens::TokenEstimator;

/// Flat estimate for one image; providers bill images by size, which the context does not see.
const IMAGE_TOKENS: u32 = 1_000;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Turn {
    pub messages: Vec<Message>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Breakdown {
    pub system: u32,
    pub tools: u32,
    pub agents_md: u32,
    pub memory: u32,
    pub repo_map: u32,
    pub pinned: u32,
    pub state: u32,
    pub history: u32,
}

impl Breakdown {
    pub fn total(&self) -> u32 {
        self.system
            + self.tools
            + self.agents_md
            + self.memory
            + self.repo_map
            + self.pinned
            + self.state
            + self.history
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ContextStatus {
    pub used: u32,
    pub budget: u32,
    pub breakdown: Breakdown,
    pub warnings: Vec<String>,
    /// Tokens of fixed-size estimates (images) in `used`, which calibration leaves out of
    /// its sample: what the measured request contained, not what the history holds later.
    pub flat_tokens: u32,
    /// Estimated tokens of the request up to its last cache breakpoint (system blocks,
    /// tools, pinned items and history; not the state tail), 0 without a prompt cache.
    /// What the engine passes to [`Context::note_request`](crate::Context::note_request).
    pub cached_prefix: u32,
}

/// Which stored reasoning a provider's adapter sends back, so the estimate counts what
/// the provider will actually be sent, not what the history holds.
///
/// - [`Thinking`](Self::Thinking): Anthropic. Signed thinking text and redacted thinking
///   data; unsigned thinking is dropped by the adapter.
/// - [`Items`](Self::Items): OpenAI. Only the replayed reasoning items
///   (`RedactedReasoning`); thinking text is dropped.
/// - [`None`](Self::None): Google (thought text is never sent, only its signature) and
///   OpenAI-compatible endpoints (no reasoning at all).
///
/// An OpenAI reasoning item is counted without its `encrypted_content`, an opaque payload
/// whose length says nothing about its tokens. Estimates made without a model (compaction's
/// planning and `uncompact`'s budget) use the default, [`Thinking`](Self::Thinking), which
/// counts everything any adapter sends.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ReasoningWire {
    #[default]
    Thinking,
    Items,
    None,
}

impl ReasoningWire {
    /// The wire of `model`'s provider; unknown providers get the default.
    pub fn for_model(model: &ModelId) -> Self {
        match model.provider() {
            "openai" => Self::Items,
            "google" | "compat" => Self::None,
            _ => Self::Thinking,
        }
    }
}

/// Tokens of fixed-size estimates (images) in `messages`, which the ratio does not scale.
pub(crate) fn flat_tokens(messages: &[Message]) -> u32 {
    let images = messages
        .iter()
        .flat_map(|m| &m.content)
        .filter(|c| matches!(c, Content::Image { .. }))
        .count();
    IMAGE_TOKENS.saturating_mul(images as u32)
}

pub(crate) fn content_tokens(c: &Content, est: &TokenEstimator, wire: ReasoningWire) -> u32 {
    match c {
        Content::Text(t) => est.estimate(t),
        Content::Image { .. } => IMAGE_TOKENS,
        Content::ToolUse { name, input, .. } => {
            est.estimate(name) + est.estimate(&input.to_string())
        }
        Content::ToolResult { output, .. } => est.estimate(output),
        Content::Reasoning {
            text,
            signature: Some(_),
        } if wire == ReasoningWire::Thinking => est.estimate(text),
        Content::Reasoning { .. } => 0,
        Content::RedactedReasoning { data } if wire != ReasoningWire::None => {
            redacted_tokens(data, est)
        }
        Content::RedactedReasoning { .. } => 0,
    }
}

/// Estimate of a `RedactedReasoning` payload: an OpenAI reasoning item (JSON with an
/// `encrypted_content` string) without that payload, anything else (Anthropic's opaque
/// data) as it is.
fn redacted_tokens(data: &str, est: &TokenEstimator) -> u32 {
    if data.starts_with('{')
        && let Ok(serde_json::Value::Object(mut item)) = serde_json::from_str(data)
        && item
            .remove("encrypted_content")
            .is_some_and(|v| v.is_string())
    {
        return est.estimate(&serde_json::Value::Object(item).to_string());
    }
    est.estimate(data)
}

pub(crate) fn message_tokens(m: &Message, est: &TokenEstimator, wire: ReasoningWire) -> u32 {
    m.content.iter().map(|c| content_tokens(c, est, wire)).sum()
}
