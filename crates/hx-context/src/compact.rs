//! State-preserving, reversible compaction. The oldest history turns are replaced by one
//! summary turn; pinned items and the working state are not part of the history and so
//! survive verbatim, and the recent turns are kept as they are. The removed turns are kept
//! in a [`CompactionRecord`] so [`Context::uncompact`] can bring them back.
//!
//! This module only decides what to remove and how: the engine asks for a request
//! ([`Context::compaction_request`]), runs it on a model, and applies the answer
//! ([`Context::apply_compaction`]). When to do so (threshold, cache epochs) is the
//! scheduler's call, not this module's.

use std::collections::HashSet;
use std::fmt::Write;

use hx_provider::{Block, CanonicalRequest, Capabilities, Content, Message, ModelId, Role};

use crate::blocks::{ContextStatus, Turn, message_tokens};
use crate::{Context, assemble, pinned};

/// Share of the window at which compaction starts (80%).
pub const DEFAULT_THRESHOLD: f32 = 0.8;
/// Recent turns left verbatim.
pub const DEFAULT_KEEP_RECENT: usize = 4;

/// System prompt of the summarizing request.
const COMPACT_PROMPT: &str = include_str!("../prompts/compact.md");

/// What a compaction removed, enough to undo it with [`Context::uncompact`].
///
/// Turns are numbered absolutely over the whole session (see [`Context::first_turn`]), the
/// numbering the read tracker's baselines use. The summary turn takes the number of the
/// last turn it replaced, so every kept turn keeps its number. The engine passes
/// `last_removed_turn` to `ReadTracker::forget_shown_at` after applying a compaction: a
/// re-read must not answer `unchanged since turn N` for a turn the model can no longer see.
/// When the first removed turn is the summary of an earlier compaction, `first_removed_turn`
/// is that summary's number (the last turn it covered).
#[derive(Debug, Clone, PartialEq)]
pub struct CompactionRecord {
    pub removed_turns: Vec<Turn>,
    /// The summary as generated, without the `<summary>` wrapper.
    pub summary: String,
    pub first_removed_turn: u32,
    pub last_removed_turn: u32,
}

/// Why [`Context::uncompact`] did nothing.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UncompactError {
    /// Restoring would need `needed` estimated tokens, over the `budget`.
    #[error("restoring the compacted turns needs {needed} tokens, over the budget of {budget}")]
    TooLarge { needed: u32, budget: u32 },
    /// The record's summary is not the first turn of the history: a later compaction or
    /// edit came after it. Compactions are undone newest first.
    #[error("this compaction is not the latest one and cannot be undone yet")]
    NotLatest,
}

impl Context {
    /// Whether `status` is at or over `threshold` (a share of the window, normally
    /// [`DEFAULT_THRESHOLD`]). `None` disables automatic compaction, as does a threshold that
    /// is not a positive number.
    pub fn needs_compaction(&self, status: &ContextStatus, threshold: Option<f32>) -> bool {
        let Some(t) = threshold.filter(|t| t.is_finite() && *t > 0.0) else {
            return false;
        };
        if status.budget == 0 {
            return false;
        }
        // Basis points: exact for round thresholds such as 0.8, which f32 cannot hold.
        let bp = (f64::from(t) * 10_000.0).round() as u64;
        u64::from(status.used) * 10_000 >= u64::from(status.budget) * bp
    }

    /// The request that produces the summary, and how many of the oldest turns it covers
    /// (pass that count to [`apply_compaction`](Self::apply_compaction)). `keep_recent` turns
    /// stay verbatim; the cut moves earlier, keeping more, rather than separating a tool
    /// call from its result. The count is 0 when there is nothing to remove, and the request
    /// is then not worth sending.
    ///
    /// The request is for `model`, with a short dedicated system prompt instead of the stable
    /// prefix and no tools; the removed turns are rendered as one transcript in a single user
    /// message. Pinned items and the working state are not summarized.
    pub fn compaction_request(
        &self,
        model: &ModelId,
        caps: &Capabilities,
        keep_recent: usize,
    ) -> (CanonicalRequest, usize) {
        let removed = self.removable(keep_recent);
        let mut transcript = String::from("<transcript>\n");
        for message in self.history[..removed].iter().flat_map(|t| &t.messages) {
            render_message(&mut transcript, message);
        }
        transcript.push_str("</transcript>\n\nWrite the summary now.");
        let request = CanonicalRequest {
            model: model.clone(),
            system: vec![Block::Text(COMPACT_PROMPT.to_string())],
            tools: Vec::new(),
            messages: vec![Message {
                role: Role::User,
                content: vec![Content::Text(transcript)],
            }],
            max_output: caps.max_output,
            reasoning: None,
            cache_breakpoints: Vec::new(),
        };
        (request, removed)
    }

    /// Replace the `removed` oldest turns (from [`compaction_request`](Self::compaction_request))
    /// with `summary` as one user turn, `<summary>…</summary>`, at the start of the history:
    /// pinned, summary, recent turns, state. `removed` is capped at the history length.
    /// None, and no change, when it is 0.
    pub fn apply_compaction(
        &mut self,
        removed: usize,
        summary: String,
    ) -> Option<CompactionRecord> {
        let removed = removed.min(self.history.len());
        if removed == 0 {
            return None;
        }
        let first_removed_turn = self.first_turn;
        let last_removed_turn = first_removed_turn + (removed as u32 - 1);
        let removed_turns: Vec<Turn> = self
            .history
            .splice(..removed, [summary_turn(&summary)])
            .collect();
        // The summary stands in the slot of the last turn it replaced.
        self.first_turn = last_removed_turn;
        Some(CompactionRecord {
            removed_turns,
            summary,
            first_removed_turn,
            last_removed_turn,
        })
    }

    /// Undo [`apply_compaction`](Self::apply_compaction): put the removed turns back in place
    /// of the summary, provided the context then fits `budget` (estimated tokens of
    /// everything but the tool schemas: system blocks, pinned, history and state; the engine
    /// passes the window less the tools). Only the latest compaction can be undone.
    /// Nothing changes on error.
    pub fn uncompact(&mut self, rec: &CompactionRecord, budget: u32) -> Result<(), UncompactError> {
        let summary = summary_turn(&rec.summary);
        if self.first_turn != rec.last_removed_turn || self.history.first() != Some(&summary) {
            return Err(UncompactError::NotLatest);
        }
        let est = &self.tokens;
        let turn_tokens =
            |t: &Turn| -> u32 { t.messages.iter().map(|m| message_tokens(m, est)).sum() };
        let needed = self
            .estimated_tokens()
            .saturating_sub(turn_tokens(&summary))
            .saturating_add(rec.removed_turns.iter().map(turn_tokens).sum::<u32>());
        if needed > budget {
            return Err(UncompactError::TooLarge { needed, budget });
        }
        self.history.splice(..1, rec.removed_turns.iter().cloned());
        self.first_turn = rec.first_removed_turn;
        Ok(())
    }

    /// Turns to remove so `keep_recent` stay: the cut moves earlier while it would put a tool
    /// call on one side and its result on the other.
    fn removable(&self, keep_recent: usize) -> usize {
        let mut cut = self.history.len().saturating_sub(keep_recent);
        while cut > 0 && splits_tool_pair(&self.history, cut) {
            cut -= 1;
        }
        cut
    }

    /// Estimated tokens of what `assemble` would send except the tool schemas.
    fn estimated_tokens(&self) -> u32 {
        let est = &self.tokens;
        let tagged = |tag: &str, text: &Option<String>| {
            est.estimate(&assemble::tagged(tag, text.as_deref()))
        };
        let pinned = pinned::render(&self.pinned).map_or(0, |t| est.estimate(&t));
        let state = self
            .state
            .render(self.root.as_deref(), est)
            .map_or(0, |t| est.estimate(&t));
        let history: u32 = self
            .history
            .iter()
            .flat_map(|t| &t.messages)
            .map(|m| message_tokens(m, est))
            .sum();
        est.estimate(crate::prompt::system())
            + tagged("agents_md", &self.agents_md)
            + tagged("memory_index", &self.memory_index)
            + tagged("repo_map", &self.repo_map)
            + pinned
            + state
            + history
    }
}

fn summary_turn(summary: &str) -> Turn {
    Turn {
        messages: vec![Message {
            role: Role::User,
            content: vec![Content::Text(format!(
                "<summary>\n{}\n</summary>",
                summary.trim()
            ))],
        }],
    }
}

/// Whether a tool call before `cut` has its result at or after it.
fn splits_tool_pair(history: &[Turn], cut: usize) -> bool {
    let ids = |turns: &[Turn], want_use: bool| -> HashSet<String> {
        turns
            .iter()
            .flat_map(|t| &t.messages)
            .flat_map(|m| &m.content)
            .filter_map(|c| match c {
                Content::ToolUse { id, .. } if want_use => Some(id.clone()),
                Content::ToolResult { id, .. } if !want_use => Some(id.clone()),
                _ => None,
            })
            .collect()
    };
    let results_kept = ids(&history[cut..], false);
    !results_kept.is_empty()
        && ids(&history[..cut], true)
            .iter()
            .any(|id| results_kept.contains(id))
}

fn render_message(out: &mut String, message: &Message) {
    let role = match message.role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
    };
    for content in &message.content {
        // Writing to a String cannot fail.
        let _ = match content {
            Content::Text(t) => writeln!(out, "[{role}]\n{t}\n"),
            Content::Image { .. } => writeln!(out, "[{role}]\n[image]\n"),
            Content::ToolUse { id, name, input } => {
                writeln!(out, "[{role} tool call {id}: {name}]\n{input}\n")
            }
            Content::ToolResult {
                id,
                output,
                is_error,
            } => {
                let kind = if *is_error { "error" } else { "result" };
                writeln!(out, "[tool {kind} {id}]\n{output}\n")
            }
            // Reasoning is the model's scratch work, not conversation.
            Content::Reasoning { .. } | Content::RedactedReasoning { .. } => Ok(()),
        };
    }
}
