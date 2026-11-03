//! Cache-aware scheduling of history mutations (spec 7.7). Collecting superseded tool
//! results ([`crate::gc`]) and threshold compaction both rewrite the history, which breaks
//! the provider's prompt cache from the first changed token on. Both go through [`decide`],
//! which says whether a mutation is worth it right now and why.
//!
//! The engine decides once at turn start and applies everything it decided together, one
//! "cache epoch", so the prefix breaks at most once.

use std::time::{Duration, Instant};

use hx_provider::Price;

/// Why a history mutation was applied; reported in `ContextCollected` and `Compacted`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MutationReason {
    /// The provider's cache has expired, so nothing cached is lost.
    ColdCache,
    /// The cache is warm but the tokens saved outweigh the cost of rebuilding it.
    CostBenefit,
    /// The context is at [`FORCED_PERCENT`] of the window or more.
    Forced,
    /// The user asked for it.
    Manual,
}

/// When the last provider request went out and how much of it the cache holds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CacheClock {
    pub last_request: Option<Instant>,
    /// Tokens of the request prefix up to its last cache breakpoint.
    pub cached_prefix_tokens: u32,
}

/// Share of the window (percent) at which mutations are applied whatever they cost.
pub const FORCED_PERCENT: u64 = 90;

/// Floor of the no-price threshold, see [`threshold_tokens`].
const MIN_THRESHOLD_TOKENS: u32 = 2_000;

/// Further requests a saved token is expected to be re-sent in (spec 7.7's horizon).
const HORIZON_REQUESTS: f64 = 5.0;

/// Decide whether to mutate the history now. `tokens_freed` is what the mutation would
/// save, `used` the context's current tokens and `window` the model's context window;
/// `ttl` is the model's cache lifetime (`None`: no prompt cache) and `price` its catalog
/// price, `None` when unknown.
///
/// - Cold cache (no request yet, `ttl` unknown, or at least `ttl` since the last one):
///   [`ColdCache`](MutationReason::ColdCache); nothing cached is lost.
/// - `used` at 90% of the window or more: [`Forced`](MutationReason::Forced).
/// - Warm, with a price: [`CostBenefit`](MutationReason::CostBenefit) when
///   `tokens_freed x input x 5 > invalidated_prefix x (cache_write - cache_read)`. The
///   invalidated prefix is the whole cached prefix, which overestimates the cost (the cache
///   survives up to the first changed token) and so defers more.
/// - Warm, without a price: [`CostBenefit`](MutationReason::CostBenefit) when
///   `tokens_freed` reaches [`threshold_tokens`] (`max(2000, 5%)` of the window).
/// - Otherwise `None`: wait for a cheaper moment.
///
/// Where a cache write costs less than plain input (automatic caches charge nothing for
/// writes), the cost of a miss is still the re-read of the prefix at the input rate, so
/// `max(cache_write, input)` stands in for the write price.
pub fn decide(
    clock: &CacheClock,
    now: Instant,
    ttl: Option<Duration>,
    tokens_freed: u32,
    price: Option<&Price>,
    used: u32,
    window: u32,
) -> Option<MutationReason> {
    let cold = match (clock.last_request, ttl) {
        (Some(last), Some(ttl)) => now.saturating_duration_since(last) >= ttl,
        _ => true,
    };
    if cold {
        return Some(MutationReason::ColdCache);
    }
    if window > 0 && u64::from(used) * 100 >= u64::from(window) * FORCED_PERCENT {
        return Some(MutationReason::Forced);
    }
    let worth_it = match price {
        Some(p) => {
            let saved = f64::from(tokens_freed) * p.input * HORIZON_REQUESTS;
            let miss_premium = p.cache_write.max(p.input) - p.cache_read;
            let cost = f64::from(clock.cached_prefix_tokens) * miss_premium;
            saved > cost
        }
        None => tokens_freed >= threshold_tokens(window),
    };
    worth_it.then_some(MutationReason::CostBenefit)
}

/// Elidable tokens at which a warm cache without catalog prices is worth breaking:
/// `max(2000, 5%)` of the window.
pub fn threshold_tokens(window: u32) -> u32 {
    MIN_THRESHOLD_TOKENS.max(window / 20)
}
