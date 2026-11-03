//! Plain data carried through assembly: turns, per-block token breakdown, status, and the
//! token weight of messages.

use hx_provider::{Content, Message};

use crate::tokens::TokenEstimator;

/// Flat estimate for one image; providers bill images by size, which the context does not see.
const IMAGE_TOKENS: u32 = 1_000;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Turn {
    pub messages: Vec<Message>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Breakdown {
    pub system: u32,
    pub tools: u32,
    pub agents_md: u32,
    pub memory: u32,
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
            + self.pinned
            + self.state
            + self.history
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextStatus {
    pub used: u32,
    pub budget: u32,
    pub breakdown: Breakdown,
    pub warnings: Vec<String>,
}

pub(crate) fn content_tokens(c: &Content, est: &TokenEstimator) -> u32 {
    match c {
        Content::Text(t) => est.estimate(t),
        Content::Image { .. } => IMAGE_TOKENS,
        Content::ToolUse { name, input, .. } => {
            est.estimate(name) + est.estimate(&input.to_string())
        }
        Content::ToolResult { output, .. } => est.estimate(output),
        Content::Reasoning { text, .. } => est.estimate(text),
        Content::RedactedReasoning { data } => est.estimate(data),
    }
}

pub(crate) fn message_tokens(m: &Message, est: &TokenEstimator) -> u32 {
    m.content.iter().map(|c| content_tokens(c, est)).sum()
}
