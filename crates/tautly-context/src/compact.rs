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
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tautly_provider::{
    Block, CanonicalRequest, Capabilities, Content, Message, ModelId, Price, Role,
};

use crate::blocks::{ContextStatus, ReasoningWire, Turn, message_tokens};
use crate::schedule::{self, MutationReason};
use crate::{Context, assemble, pinned};

/// Share of the window at which compaction starts (80%).
pub const DEFAULT_THRESHOLD: f32 = 0.8;
/// Recent turns left verbatim.
pub const DEFAULT_KEEP_RECENT: usize = 4;
/// Most output tokens a summary may take.
pub const MAX_SUMMARY_TOKENS: u32 = 4_096;

/// System prompt of the summarizing request.
const COMPACT_PROMPT: &str = include_str!("../prompts/compact.md");

/// What a compaction removed, enough to undo it with [`Context::uncompact`].
///
/// Turns are numbered absolutely over the whole session, from 1 (see [`Context::first_turn`]),
/// the numbering the read tracker's baselines use. The summary turn takes the number of the
/// last turn it replaced, so every kept turn keeps its number. When the first removed turn
/// is the summary of an earlier compaction, `first_removed_turn` is that summary's number
/// (the last turn it covered).
///
/// After applying a compaction the engine forgets the read tracker's baselines that rest on
/// removed output, so a re-read never answers `unchanged since turn N` or a diff against
/// content the model can no longer see:
///
/// - `ReadTracker::forget_shown_at(last_removed_turn)` drops the baselines shown in the
///   removed turns;
/// - `ReadTracker::forget_path(p)` for every `p` in `forgotten_paths` drops the baselines
///   recorded *later* that still depend on a removed read: a post-edit baseline (the model
///   knows the file from the removed read plus the edit) and a diff answer (only a diff
///   against the removed read). Like collection's `GcRecord::forgotten_paths`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompactionRecord {
    pub removed_turns: Vec<Turn>,
    /// The summary as generated, without the `<summary>` wrapper.
    pub summary: String,
    pub first_removed_turn: u32,
    pub last_removed_turn: u32,
    /// Every path a tool result of the removed turns read or wrote (from
    /// [`Context::results`]), in first-seen order. Turns an earlier compaction removed are
    /// not listed again.
    pub forgotten_paths: Vec<PathBuf>,
}

/// Why [`Context::apply_compaction`] did nothing.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CompactError {
    /// No turn to remove, or only the summary of an earlier compaction.
    #[error("nothing to compact")]
    Nothing,
    /// The summary would not make the context smaller (estimated tokens of the summary
    /// turn and of the removed turns).
    #[error("the summary ({summary} tokens) is not smaller than the {removed} tokens it replaces")]
    SummaryTooLong { summary: u32, removed: u32 },
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
    /// Whether compaction should run now: [`compaction_decision`](Self::compaction_decision)
    /// found a reason.
    pub fn needs_compaction(
        &self,
        status: &ContextStatus,
        threshold: Option<f32>,
        now: Instant,
        ttl: Option<Duration>,
        price: Option<&Price>,
    ) -> bool {
        self.compaction_decision(status, threshold, now, ttl, price)
            .is_some()
    }

    /// Why compaction should run now, if it should. `status` at or over `threshold` (a share
    /// of the window, normally [`DEFAULT_THRESHOLD`]) makes it a candidate; the cache-aware
    /// scheduler ([`schedule::decide`]) then lets it run at once when the cache is cold or the
    /// context is at 90%, and while the cache is warm only when the tokens it frees repay
    /// rebuilding the cached prefix. `None` for `threshold` disables automatic compaction, as
    /// does a threshold that is not a positive number; with any positive threshold the
    /// context at 90% of the window is compacted regardless of its value. The engine applies the result together
    /// with any collection decided at the same turn start.
    pub fn compaction_decision(
        &self,
        status: &ContextStatus,
        threshold: Option<f32>,
        now: Instant,
        ttl: Option<Duration>,
        price: Option<&Price>,
    ) -> Option<MutationReason> {
        let t = threshold.filter(|t| t.is_finite() && *t > 0.0)?;
        if status.budget == 0 {
            return None;
        }
        // Basis points: exact for round thresholds such as 0.8, which f32 cannot hold.
        let bp = (f64::from(t) * 10_000.0).round() as u64;
        let below_threshold = u64::from(status.used) * 10_000 < u64::from(status.budget) * bp;
        // With a threshold set, the scheduler's forced rule (at 90% of the window) holds
        // whatever the threshold is, so a high one cannot keep compaction away.
        let forced =
            u64::from(status.used) * 100 >= u64::from(status.budget) * schedule::FORCED_PERCENT;
        if below_threshold && !forced {
            return None;
        }
        // A forced cold epoch (a model switch) is not an expired cache: when the window
        // forces the compaction, say so.
        if forced && self.cold_epoch_forced {
            return Some(MutationReason::Forced);
        }
        schedule::decide(
            &self.clock,
            now,
            ttl,
            self.compactable_tokens(),
            price,
            status.used,
            status.budget,
        )
    }

    /// Estimated tokens a compaction with [`DEFAULT_KEEP_RECENT`] would remove, taking the
    /// summary that replaces them as small. An estimate for the scheduler: the real cut
    /// ([`compaction_request`](Self::compaction_request)) can be shorter.
    fn compactable_tokens(&self) -> u32 {
        let est = &self.tokens;
        let cut = self.history.len().saturating_sub(DEFAULT_KEEP_RECENT);
        self.history[..cut]
            .iter()
            .flat_map(|t| &t.messages)
            .map(|m| message_tokens(m, est, ReasoningWire::default()))
            .fold(0, u32::saturating_add)
    }

    /// The request that produces the summary, and how many of the oldest turns it covers
    /// (pass that count to [`apply_compaction`](Self::apply_compaction)). `keep_recent` turns
    /// stay verbatim. The count is 0 when there is nothing worth summarizing, and the request
    /// is then not worth sending. It can be smaller than `history.len() - keep_recent`:
    ///
    /// - the cut moves earlier rather than separate a tool call from its result, and a tool
    ///   call that has no result yet is never removed;
    /// - a lone summary turn is not worth summarizing again;
    /// - the request must fit the window of `model` (`caps`): the transcript may use
    ///   `context_window - max_output` less the prompt, where the summary's `max_output` is
    ///   the model's capped at [`MAX_SUMMARY_TOKENS`]. Tool inputs and results are cut to
    ///   their first and last [`KEEP_BYTES`] bytes, and if the transcript is still too big
    ///   the count shrinks to the longest oldest prefix that fits. Repeated compaction
    ///   handles the rest; 0 here with a long history means one turn alone is too big.
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
        const OPEN: &str = "<transcript>\n";
        const CLOSE: &str = "</transcript>\n\nWrite the summary now.";
        let est = &self.tokens;

        let max_output = caps.max_output.min(MAX_SUMMARY_TOKENS);
        let overhead = est.estimate(COMPACT_PROMPT) + est.estimate(OPEN) + est.estimate(CLOSE);
        let budget = caps
            .context_window
            .saturating_sub(max_output)
            .saturating_sub(overhead);

        // Largest cut up to the wanted one that is valid and whose transcript fits. The cost
        // of a turn is the sum of its chunks' estimates, which never undercounts the joined
        // text.
        let mut cut = self.history.len().saturating_sub(keep_recent);
        let chunks: Vec<String> = self.history[..cut].iter().map(render_turn).collect();
        let costs: Vec<u32> = chunks.iter().map(|c| est.estimate(c)).collect();
        let removed = loop {
            cut = self.settle(cut);
            let cost = costs[..cut].iter().fold(0u32, |a, c| a.saturating_add(*c));
            if cut == 0 || cost <= budget {
                break cut;
            }
            cut -= 1;
        };

        let mut transcript = String::from(OPEN);
        for chunk in &chunks[..removed] {
            transcript.push_str(chunk);
        }
        transcript.push_str(CLOSE);
        let request = CanonicalRequest {
            model: model.clone(),
            system: vec![Block::Text(COMPACT_PROMPT.to_string())],
            tools: Vec::new(),
            messages: vec![Message {
                role: Role::User,
                content: vec![Content::Text(transcript)],
            }],
            max_output,
            reasoning: None,
            cache_breakpoints: Vec::new(),
            volatile_tail: false,
        };
        (request, removed)
    }

    /// Replace the `removed` oldest turns (from [`compaction_request`](Self::compaction_request))
    /// with `summary` as one user turn, `<summary>…</summary>`, at the start of the history:
    /// pinned, summary, recent turns, state. `removed` is capped at the history length.
    ///
    /// Nothing changes on error: [`CompactError::Nothing`] when `removed` is 0 or only the
    /// summary of an earlier compaction, and [`CompactError::SummaryTooLong`] when the
    /// summary's estimate is not smaller than the turns it would replace. The engine then
    /// compacts without a summary: it calls this again with an empty `summary` (or a short
    /// placeholder note), and skips the compaction if that is refused as well.
    ///
    /// The record's `forgotten_paths` lists every path the removed turns' tool results read
    /// or wrote (from [`Context::results`]); see [`CompactionRecord`] for what the engine
    /// does with them.
    pub fn apply_compaction(
        &mut self,
        removed: usize,
        summary: String,
    ) -> Result<CompactionRecord, CompactError> {
        let removed = removed.min(self.history.len());
        if removed == 0 || (removed == 1 && is_summary_turn(&self.history[0])) {
            return Err(CompactError::Nothing);
        }
        let est = &self.tokens;
        let turn_tokens = |t: &Turn| -> u32 {
            t.messages
                .iter()
                .map(|m| message_tokens(m, est, ReasoningWire::default()))
                .fold(0, u32::saturating_add)
        };
        let replacement = summary_turn(&summary);
        let summary_tokens = turn_tokens(&replacement);
        let removed_tokens = self.history[..removed]
            .iter()
            .map(turn_tokens)
            .fold(0, u32::saturating_add);
        if summary_tokens >= removed_tokens {
            return Err(CompactError::SummaryTooLong {
                summary: summary_tokens,
                removed: removed_tokens,
            });
        }
        let first_removed_turn = self.first_turn;
        let last_removed_turn = first_removed_turn + (removed as u32 - 1);
        // A leading summary stands for turns an earlier compaction already reported.
        let after = if is_summary_turn(&self.history[0]) {
            first_removed_turn
        } else {
            first_removed_turn - 1
        };
        let mut forgotten_paths: Vec<PathBuf> = Vec::new();
        for meta in self
            .results
            .iter()
            .filter(|r| r.turn > after && r.turn <= last_removed_turn)
        {
            for path in meta.reads.iter().map(|t| &t.path).chain(&meta.writes) {
                if !forgotten_paths.contains(path) {
                    forgotten_paths.push(path.clone());
                }
            }
        }
        let removed_turns: Vec<Turn> = self.history.splice(..removed, [replacement]).collect();
        // The summary stands in the slot of the last turn it replaced.
        self.first_turn = last_removed_turn;
        Ok(CompactionRecord {
            removed_turns,
            summary,
            first_removed_turn,
            last_removed_turn,
            forgotten_paths,
        })
    }

    /// Undo [`apply_compaction`](Self::apply_compaction): put the removed turns back in place
    /// of the summary, provided the context then fits `budget` (estimated tokens of
    /// everything but the tool schemas: system blocks, pinned, history and state; the engine
    /// passes the window less the tools and less `max_output`, so a restore cannot fit only
    /// by leaving no room for the reply). Only the latest compaction can be undone.
    /// Nothing changes on error.
    pub fn uncompact(&mut self, rec: &CompactionRecord, budget: u32) -> Result<(), UncompactError> {
        let summary = summary_turn(&rec.summary);
        if self.first_turn != rec.last_removed_turn || self.history.first() != Some(&summary) {
            return Err(UncompactError::NotLatest);
        }
        let est = &self.tokens;
        let turn_tokens = |t: &Turn| -> u32 {
            t.messages
                .iter()
                .map(|m| message_tokens(m, est, ReasoningWire::default()))
                .sum()
        };
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

    /// The largest cut at or below `cut` that may be made: not past a tool call without a
    /// result yet, not between a tool call and its result, and not covering only a summary.
    fn settle(&self, mut cut: usize) -> usize {
        cut = cut.min(first_unanswered_call(&self.history));
        while cut > 0 && splits_tool_pair(&self.history, cut) {
            cut -= 1;
        }
        if cut == 1 && is_summary_turn(&self.history[0]) {
            0
        } else {
            cut
        }
    }

    /// The tracker turn number of the next turn: what the engine passes to
    /// `ToolCx::with_turn`, and the first number a compaction would not cover yet.
    pub fn next_turn(&self) -> u32 {
        self.first_turn + self.history.len() as u32
    }

    /// Estimated tokens of what `assemble` would send except the tool schemas.
    fn estimated_tokens(&self) -> u32 {
        let est = &self.tokens;
        let tagged = |tag: &str, text: &Option<String>| {
            est.estimate(&assemble::tagged(tag, text.as_deref()))
        };
        let pinned = pinned::render(&self.pinned).map_or(0, |t| est.estimate(&t))
            + pinned::render_session(&self.session_pins).map_or(0, |t| est.estimate(&t));
        let state = self
            .state
            .render(self.root.as_deref(), est)
            .map_or(0, |t| est.estimate(&t));
        let history: u32 = self
            .history
            .iter()
            .flat_map(|t| &t.messages)
            .map(|m| message_tokens(m, est, ReasoningWire::default()))
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
                neutralize(summary.trim())
            ))],
        }],
    }
}

/// Whether `turn` has the shape [`summary_turn`] gives.
fn is_summary_turn(turn: &Turn) -> bool {
    matches!(
        &turn.messages[..],
        [Message { role: Role::User, content }]
            if matches!(&content[..], [Content::Text(t)]
                if t.starts_with("<summary>\n") && t.ends_with("\n</summary>"))
    )
}

/// `text` with every `</summary` and `</transcript` (any case) written `&lt;/...`, so text
/// that came from tool output cannot close the wrapper it sits in.
fn neutralize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find("</") {
        out.push_str(&rest[..i]);
        let after = &rest[i + 2..];
        let closes = ["summary", "transcript"].iter().any(|tag| {
            after
                .as_bytes()
                .get(..tag.len())
                .is_some_and(|b| b.eq_ignore_ascii_case(tag.as_bytes()))
        });
        out.push_str(if closes { "&lt;/" } else { "</" });
        rest = after;
    }
    out.push_str(rest);
    out
}

/// Index of the first turn with a tool call that has no result anywhere in the history (the
/// call is still running), or the history length.
fn first_unanswered_call(history: &[Turn]) -> usize {
    let answered: HashSet<&str> = history
        .iter()
        .flat_map(|t| &t.messages)
        .flat_map(|m| &m.content)
        .filter_map(|c| match c {
            Content::ToolResult { id, .. } => Some(id.as_str()),
            _ => None,
        })
        .collect();
    history
        .iter()
        .position(|t| {
            t.messages.iter().flat_map(|m| &m.content).any(
                |c| matches!(c, Content::ToolUse { id, .. } if !answered.contains(id.as_str())),
            )
        })
        .unwrap_or(history.len())
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

fn render_turn(turn: &Turn) -> String {
    let mut out = String::new();
    for message in &turn.messages {
        render_message(&mut out, message);
    }
    out
}

/// Bytes kept from each end of a tool input or result in the transcript (about 500 tokens
/// each). The summary needs what a call did, not its full output; the agent can run it again.
pub const KEEP_BYTES: usize = 2_000;

/// `text` neutralized, and cut to its first and last [`KEEP_BYTES`] bytes when longer than
/// both plus a margin, with a marker between.
fn clip(text: &str) -> String {
    if text.len() <= 2 * KEEP_BYTES + 200 {
        return neutralize(text);
    }
    let mut head = KEEP_BYTES;
    while !text.is_char_boundary(head) {
        head -= 1;
    }
    let mut tail = text.len() - KEEP_BYTES;
    while !text.is_char_boundary(tail) {
        tail += 1;
    }
    format!(
        "{}\n[... {} bytes omitted ...]\n{}",
        neutralize(&text[..head]),
        tail - head,
        neutralize(&text[tail..])
    )
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
            Content::Text(t) => writeln!(out, "[{role}]\n{}\n", neutralize(t)),
            Content::Image { .. } => writeln!(out, "[{role}]\n[image]\n"),
            Content::ToolUse { id, name, input } => {
                writeln!(
                    out,
                    "[{role} tool call {id}: {name}]\n{}\n",
                    clip(&input.to_string())
                )
            }
            Content::ToolResult {
                id,
                output,
                is_error,
            } => {
                let kind = if *is_error { "error" } else { "result" };
                writeln!(out, "[tool {kind} {id}]\n{}\n", clip(output))
            }
            // Reasoning is the model's scratch work, not conversation.
            Content::Reasoning { .. } | Content::RedactedReasoning { .. } => Ok(()),
        };
    }
}
