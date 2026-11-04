//! Turn receipts and the savings meter (spec 7.11.1): what one user turn spent, and what
//! Tautly's mechanisms saved against a plain harness with the same 30 KB output cap.
//!
//! A [`ReceiptBuilder`] lives for one user turn. The engine feeds it every charge (main and
//! side, estimated or reported, each priced by its own model), every provider request
//! (its step, and the tokens GC keeps out of the context at it) and every tool call that
//! ran; it emits the result as [`Event::TurnReceipt`] right before `TurnFinished`, and the
//! cost ledger stores it. Every savings number is an estimate (bytes / 4), so clients
//! label them "est.".
//!
//! Everything a receipt counts is in the session log (`Charged`, provider usage,
//! `ToolResult.meta.saved`, `GcApplied`). The one piece of state that spans turns, the
//! session's collections (`Session::gc_elided`, which `context_gc` credits for as long as
//! their results stay elided), is rebuilt from the `GcApplied` records on resume, fork and
//! rewind (`resume.rs`).

use std::time::Instant;

use serde::{Deserialize, Serialize};
use tautly_provider::{ModelId, Price, RawUsage};
use tautly_tools::{SaveKind, ToolMeta};

use crate::protocol::{Event, UsageRole};

/// Bytes per token in every savings estimate.
const BYTES_PER_TOKEN: u64 = 4;

/// What a turn saved, in estimated tokens (and USD where the catalog has prices).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Savings {
    /// `outline`, `symbol` and symbol `read` targets: whole files not read.
    #[serde(default)]
    pub index: u64,
    /// Re-reads answered `unchanged` or with a diff.
    #[serde(default)]
    pub diff_reads: u64,
    /// Tokens GC kept out of the context, summed over the turn's requests.
    #[serde(default)]
    pub context_gc: u64,
    /// Command output summarized by an output filter.
    #[serde(default)]
    pub filters: u64,
    /// The turn's `cache_read`.
    #[serde(default)]
    pub cache_read_tokens: u64,
    /// What the prompt cache saved on the main model's requests, at its prices (negative
    /// when writing to the cache cost more than reading from it saved); the side model's
    /// cache tokens are left out. `None` without prices.
    pub cache_usd: Option<f64>,
    /// The four token estimates at the main model's input price (`context_gc` at its
    /// cache-read price), plus `cache_usd`. `None` without prices.
    pub total_usd: Option<f64>,
}

/// Collects one user turn's receipt.
#[derive(Debug, Clone)]
pub struct ReceiptBuilder {
    turn: u32,
    model: ModelId,
    started: Instant,
    tokens_in: u64,
    tokens_out: u64,
    cache_read: u64,
    cache_write: u64,
    /// The main role's share of the cache tokens, which `cache_usd` prices.
    main_cache_read: u64,
    main_cache_write: u64,
    side_tokens: u64,
    /// `None` once an unpriced entry was charged.
    cost_usd: Option<f64>,
    estimated: bool,
    steps: u32,
    /// The step of the last request: a step sent again (after an overflow) counts once.
    last_step: Option<u32>,
    tool_calls: u32,
    index_bytes: u64,
    diff_read_bytes: u64,
    filter_bytes: u64,
    context_gc: u64,
}

impl ReceiptBuilder {
    /// The receipt of user turn `turn` (1-based) on `model`, started (`TurnStarted`) at
    /// `started`.
    pub fn new(turn: u32, model: ModelId, started: Instant) -> Self {
        ReceiptBuilder {
            turn,
            model,
            started,
            tokens_in: 0,
            tokens_out: 0,
            cache_read: 0,
            cache_write: 0,
            main_cache_read: 0,
            main_cache_write: 0,
            side_tokens: 0,
            cost_usd: Some(0.0),
            estimated: false,
            steps: 0,
            last_step: None,
            tool_calls: 0,
            index_bytes: 0,
            diff_read_bytes: 0,
            filter_bytes: 0,
            context_gc: 0,
        }
    }

    /// One charge: a response's usage (or an estimate of it) and its cost at its own
    /// model's price (`None`: unpriced).
    pub fn usage(&mut self, role: UsageRole, raw: &RawUsage, cost: Option<f64>) {
        self.tokens_in = self.tokens_in.saturating_add(raw.input);
        self.tokens_out = self.tokens_out.saturating_add(raw.output);
        self.cache_read = self.cache_read.saturating_add(raw.cache_read);
        self.cache_write = self.cache_write.saturating_add(raw.cache_write);
        match role {
            UsageRole::Main => {
                self.main_cache_read = self.main_cache_read.saturating_add(raw.cache_read);
                self.main_cache_write = self.main_cache_write.saturating_add(raw.cache_write);
            }
            UsageRole::Side => {
                self.side_tokens = self
                    .side_tokens
                    .saturating_add(raw.input.saturating_add(raw.output));
            }
            UsageRole::Unknown => {}
        }
        self.cost_usd = self.cost_usd.zip(cost).map(|(a, b)| a + b);
    }

    /// Some of the charged usage is the engine's estimate.
    pub fn estimated(&mut self) {
        self.estimated = true;
    }

    /// One provider request of `step` (the absolute provider step, as `Request.step`),
    /// sent while GC kept `elided_tokens_out_of_context` tokens out of the context. Steps
    /// rise within a turn, so a step sent again (after an overflow compaction or without
    /// reasoning) counts once in `steps`; its GC saving counts per request.
    pub fn request(&mut self, step: u32, elided_tokens_out_of_context: u64) {
        if self.last_step != Some(step) {
            self.last_step = Some(step);
            self.steps = self.steps.saturating_add(1);
        }
        self.context_gc = self.context_gc.saturating_add(elided_tokens_out_of_context);
    }

    /// One tool call that ran, with what it saved (calls refused, denied, blocked or
    /// cancelled before running are not counted).
    pub fn tool(&mut self, meta: &ToolMeta) {
        self.tool_calls = self.tool_calls.saturating_add(1);
        let Some(saved) = meta.saved else { return };
        let total = match saved.kind {
            SaveKind::Index => &mut self.index_bytes,
            SaveKind::DiffRead => &mut self.diff_read_bytes,
            SaveKind::Filter => &mut self.filter_bytes,
            SaveKind::Unknown => return,
        };
        *total = total.saturating_add(u64::from(saved.bytes));
    }

    /// The [`Event::TurnReceipt`] at `now` (`TurnFinished`), with the savings priced at
    /// the main model's `price`.
    pub fn finish(self, price: Option<&Price>, now: Instant) -> Event {
        let per_million = |tokens: u64, rate: f64| tokens as f64 * rate / 1_000_000.0;
        let index = self.index_bytes / BYTES_PER_TOKEN;
        let diff_reads = self.diff_read_bytes / BYTES_PER_TOKEN;
        let filters = self.filter_bytes / BYTES_PER_TOKEN;
        // The main model's cache tokens only: the side model's are priced at other rates.
        let cache_usd = price.map(|p| {
            per_million(self.main_cache_read, p.input - p.cache_read)
                - per_million(self.main_cache_write, p.cache_write - p.input)
        });
        let total_usd = price.zip(cache_usd).map(|(p, cache)| {
            per_million(index + diff_reads + filters, p.input)
                + per_million(self.context_gc, p.cache_read)
                + cache
        });
        let seen = self.tokens_in.saturating_add(self.cache_read);
        Event::TurnReceipt {
            turn: self.turn,
            model: self.model,
            tokens_in: self.tokens_in,
            tokens_out: self.tokens_out,
            cache_read: self.cache_read,
            cache_write: self.cache_write,
            side_tokens: self.side_tokens,
            cost_usd: self.cost_usd,
            wall_ms: u64::try_from(now.saturating_duration_since(self.started).as_millis())
                .unwrap_or(u64::MAX),
            cache_hit_ratio: (seen > 0).then(|| self.cache_read as f64 / seen as f64),
            steps: self.steps,
            tool_calls: self.tool_calls,
            saved: Savings {
                index,
                diff_reads,
                context_gc: self.context_gc,
                filters,
                cache_read_tokens: self.cache_read,
                cache_usd,
                total_usd,
            },
            estimated: self.estimated,
        }
    }
}
