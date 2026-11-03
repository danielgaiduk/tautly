//! Request assembly in the fixed block order: system prompt, tools, AGENTS.md, memory index,
//! repo map, then pinned and history as messages, and the working state as a tail message.

use hx_provider::{
    Block, CachePoint, CacheStyle, CanonicalRequest, Capabilities, Content, Message, ModelId, Role,
    ToolSpec,
};

use crate::blocks::{self, Breakdown, ContextStatus, ReasoningWire, message_tokens};
use crate::{Context, pinned, prompt};

/// Leading `system` blocks that form the cache-stable prefix: system prompt, AGENTS.md,
/// memory index, repo map. Tools travel in `CanonicalRequest.tools`; pinned items follow as the first
/// user content and the working state as a tail message after the history, never in `system`.
pub const STABLE_LEN: usize = 4;

/// Pinned text above this share of the window triggers a warning.
const PINNED_WARN_PERCENT: u64 = 10;
/// Use above this share of the window triggers a warning.
const USED_WARN_PERCENT: u64 = 80;

impl Context {
    pub fn assemble(
        &self,
        caps: &Capabilities,
        tools: &[ToolSpec],
        model: &ModelId,
    ) -> (CanonicalRequest, ContextStatus) {
        let est = &self.tokens;

        // Stable prefix. Absent AGENTS.md / memory / repo map still occupy their slot (an empty tag), so
        // the prefix has a fixed length and shape.
        let system_text = prompt::system();
        let agents_text = tagged("agents_md", self.agents_md.as_deref());
        let memory_text = tagged("memory_index", self.memory_index.as_deref());
        let repo_map_text = tagged("repo_map", self.repo_map.as_deref());

        // Fixed tool order keeps the serialized prefix byte-stable whatever order the caller
        // collected the tools in.
        let mut tools: Vec<ToolSpec> = tools.to_vec();
        tools.sort_by(|a, b| a.name.cmp(&b.name));
        let tools_tokens = tools
            .iter()
            .map(|t| {
                est.estimate(&t.name)
                    + est.estimate(&t.description)
                    + est.estimate(&t.schema.to_string())
            })
            .sum();

        let pinned_text = pinned::render(&self.pinned);
        let state_text = self.state.render(self.root.as_deref(), est);
        let tokens_of = |t: &Option<String>| t.as_deref().map_or(0, |t| est.estimate(t));

        let mut messages: Vec<Message> = self
            .history
            .iter()
            .flat_map(|t| t.messages.iter().cloned())
            .collect();
        let wire = ReasoningWire::for_model(model);
        let history_tokens = messages.iter().map(|m| message_tokens(m, est, wire)).sum();
        let flat_tokens = blocks::flat_tokens(&messages);

        // Pinned leads the conversation: it joins the first message when that is a user
        // message, else stands in front of it. It changes rarely, so it stays cacheable.
        if let Some(pinned) = &pinned_text {
            let lead = Content::Text(pinned.clone());
            match messages.first_mut() {
                Some(first) if first.role == Role::User => first.content.insert(0, lead),
                _ => messages.insert(
                    0,
                    Message {
                        role: Role::User,
                        content: vec![lead],
                    },
                ),
            }
        }

        // The working state changes on almost every tool call, so it is a tail block after the
        // last message, built fresh for each request. It is always its own user message: the
        // last cache breakpoint marks the final content block of its message, and appending
        // to a user message (tool results) would put the volatile state inside the cached
        // prefix. That leaves two user messages in a row; Anthropic merges them server-side
        // and the Google and compat adapters merge same-role messages themselves.
        let cached_len = messages.len();
        if let Some(state) = &state_text {
            messages.push(Message {
                role: Role::User,
                content: vec![Content::Text(state.clone())],
            });
        }

        let mut cache_breakpoints = Vec::new();
        if caps.cache != CacheStyle::None {
            if !tools.is_empty() {
                cache_breakpoints.push(CachePoint::AfterTools);
            }
            cache_breakpoints.push(CachePoint::AfterSystem);
            if cached_len > 0 {
                // The previous request ended just before the latest assistant reply. Marking
                // that message too keeps the earlier cache entry within the provider's
                // lookback window when one step adds many blocks (4 breakpoints in all).
                if caps.cache == CacheStyle::Explicit
                    && let Some(a) = messages[..cached_len]
                        .iter()
                        .rposition(|m| m.role == Role::Assistant)
                    && a >= 1
                {
                    cache_breakpoints.push(CachePoint::AfterMessage(a - 1));
                }
                cache_breakpoints.push(CachePoint::AfterMessage(cached_len - 1));
            }
        }

        let breakdown = Breakdown {
            system: est.estimate(system_text),
            tools: tools_tokens,
            agents_md: est.estimate(&agents_text),
            memory: est.estimate(&memory_text),
            repo_map: est.estimate(&repo_map_text),
            pinned: tokens_of(&pinned_text),
            state: tokens_of(&state_text),
            history: history_tokens,
        };
        let used = breakdown.total();
        let budget = caps.context_window;
        // Everything before the tail: the last breakpoint marks the message before it.
        let cached_prefix = if caps.cache == CacheStyle::None {
            0
        } else {
            used - breakdown.state
        };

        let mut warnings = self.load_warnings.clone();
        if u64::from(breakdown.pinned) * 100 > u64::from(budget) * PINNED_WARN_PERCENT {
            warnings.push(format!(
                "pinned items use {} tokens, over {PINNED_WARN_PERCENT}% of the {budget}-token window",
                breakdown.pinned
            ));
        }
        if u64::from(used) * 100 >= u64::from(budget) * USED_WARN_PERCENT {
            warnings.push(format!(
                "context is at {used} of {budget} tokens, over {USED_WARN_PERCENT}% of the window"
            ));
        }

        let request = CanonicalRequest {
            model: model.clone(),
            system: vec![
                Block::Text(system_text.to_string()),
                Block::Text(agents_text),
                Block::Text(memory_text),
                Block::Text(repo_map_text),
            ],
            tools,
            messages,
            max_output: caps.max_output,
            reasoning: None,
            cache_breakpoints,
        };
        let status = ContextStatus {
            used,
            budget,
            breakdown,
            warnings,
            flat_tokens,
            cached_prefix,
        };
        (request, status)
    }
}

/// `<tag>\ntext\n</tag>`; an empty tag when there is no text.
pub(crate) fn tagged(tag: &str, text: Option<&str>) -> String {
    match text.map(str::trim).filter(|t| !t.is_empty()) {
        Some(t) => format!("<{tag}>\n{t}\n</{tag}>"),
        None => format!("<{tag}></{tag}>"),
    }
}
