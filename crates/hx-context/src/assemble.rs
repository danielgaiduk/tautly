//! Request assembly in the fixed block order: system prompt, tools, AGENTS.md, memory index,
//! then pinned, working state and history as messages.

use hx_provider::{
    Block, CachePoint, CanonicalRequest, Capabilities, Content, Message, ModelId, Role, ToolSpec,
};

use crate::blocks::{Breakdown, ContextStatus, message_tokens};
use crate::{Context, pinned, prompt};

/// Leading `system` blocks that form the cache-stable prefix: system prompt, AGENTS.md,
/// memory index. Tools travel in `CanonicalRequest.tools`; pinned items and working state
/// follow as the first user content, never in `system`.
pub const STABLE_LEN: usize = 3;

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

        // Stable prefix. Absent AGENTS.md / memory still occupy their slot (an empty tag), so
        // the prefix has a fixed length and shape.
        let system_text = prompt::system();
        let agents_text = tagged("agents_md", self.agents_md.as_deref());
        let memory_text = tagged("memory_index", self.memory_index.as_deref());

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
        let history_tokens = messages.iter().map(|m| message_tokens(m, est)).sum();

        // Pinned and state lead the conversation. They join the first message when it is a
        // user message (providers want alternating roles), else stand in front of it.
        let lead: Vec<Content> = [&pinned_text, &state_text]
            .into_iter()
            .flatten()
            .map(|t| Content::Text(t.clone()))
            .collect();
        if !lead.is_empty() {
            match messages.first_mut() {
                Some(first) if first.role == Role::User => {
                    first.content.splice(0..0, lead);
                }
                _ => messages.insert(
                    0,
                    Message {
                        role: Role::User,
                        content: lead,
                    },
                ),
            }
        }

        let mut cache_breakpoints = Vec::new();
        if !tools.is_empty() {
            cache_breakpoints.push(CachePoint::AfterTools);
        }
        cache_breakpoints.push(CachePoint::AfterSystem);
        if !messages.is_empty() {
            cache_breakpoints.push(CachePoint::AfterMessage(messages.len() - 1));
        }

        let breakdown = Breakdown {
            system: est.estimate(system_text),
            tools: tools_tokens,
            agents_md: est.estimate(&agents_text),
            memory: est.estimate(&memory_text),
            pinned: tokens_of(&pinned_text),
            state: tokens_of(&state_text),
            history: history_tokens,
        };
        let used = breakdown.total();
        let budget = caps.context_window;

        let mut warnings = Vec::new();
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
        };
        (request, status)
    }
}

/// `<tag>\ntext\n</tag>`; an empty tag when there is no text.
fn tagged(tag: &str, text: Option<&str>) -> String {
    match text.map(str::trim).filter(|t| !t.is_empty()) {
        Some(t) => format!("<{tag}>\n{t}\n</{tag}>"),
        None => format!("<{tag}></{tag}>"),
    }
}
