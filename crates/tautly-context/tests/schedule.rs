use std::time::{Duration, Instant};

use tautly_context::compact::DEFAULT_THRESHOLD;
use tautly_context::schedule::{decide, threshold_tokens};
use tautly_context::{CacheClock, Context, ContextStatus, MutationReason, Turn};
use tautly_provider::{Content, Message, Price, Role};

const TTL: Duration = Duration::from_secs(300);
const WINDOW: u32 = 100_000;

/// Sonnet-like explicit-cache prices: a miss costs 3.45 per million tokens more than a hit.
fn price() -> Price {
    Price {
        input: 3.0,
        output: 15.0,
        cache_read: 0.3,
        cache_write: 3.75,
    }
}

/// A clock whose last request is `age` before `now` and whose prefix is `prefix` tokens.
fn clock(now: Instant, age: Duration, prefix: u32) -> CacheClock {
    CacheClock {
        last_request: Some(now - age),
        cached_prefix_tokens: prefix,
    }
}

const WARM: Duration = Duration::from_secs(60);

#[test]
fn cold_cache_applies_any_pending() {
    let now = Instant::now();
    let c = clock(now, TTL + Duration::from_secs(1), 150_000);
    // Even a few tokens, far under every threshold and the cost model.
    assert_eq!(
        decide(&c, now, Some(TTL), 10, Some(&price()), 1_000, WINDOW),
        Some(MutationReason::ColdCache)
    );
    // Exactly at the TTL is cold, one second short of it is warm.
    let c = clock(now, TTL, 150_000);
    assert_eq!(
        decide(&c, now, Some(TTL), 10, Some(&price()), 1_000, WINDOW),
        Some(MutationReason::ColdCache)
    );
    let c = clock(now, TTL - Duration::from_secs(1), 150_000);
    assert_eq!(
        decide(&c, now, Some(TTL), 10, Some(&price()), 1_000, WINDOW),
        None
    );
    // No request yet: nothing is cached. No TTL (a model without a cache): same.
    assert_eq!(
        decide(
            &CacheClock::default(),
            now,
            Some(TTL),
            10,
            Some(&price()),
            1_000,
            WINDOW
        ),
        Some(MutationReason::ColdCache)
    );
    let warm = clock(now, WARM, 150_000);
    assert_eq!(
        decide(&warm, now, None, 10, Some(&price()), 1_000, WINDOW),
        Some(MutationReason::ColdCache)
    );
}

#[test]
fn warm_cache_defers_when_not_worth_it() {
    let now = Instant::now();
    // Rebuilding a 100k prefix costs 100k x 3.45 = 345k; freeing 10k tokens saves
    // 10k x 3 x 5 = 150k.
    let c = clock(now, WARM, 100_000);
    assert_eq!(
        decide(&c, now, Some(TTL), 10_000, Some(&price()), 50_000, WINDOW),
        None
    );
    // Savings equal to the cost are not enough: it must be strictly more.
    assert_eq!(
        decide(&c, now, Some(TTL), 23_000, Some(&price()), 50_000, WINDOW),
        None
    );
}

#[test]
fn warm_cache_applies_when_savings_exceed_cache_write_cost() {
    let now = Instant::now();
    let c = clock(now, WARM, 100_000);
    assert_eq!(
        decide(&c, now, Some(TTL), 23_001, Some(&price()), 50_000, WINDOW),
        Some(MutationReason::CostBenefit)
    );
    // A short cached prefix is cheap to rebuild.
    let c = clock(now, WARM, 5_000);
    assert_eq!(
        decide(&c, now, Some(TTL), 2_000, Some(&price()), 50_000, WINDOW),
        Some(MutationReason::CostBenefit)
    );
    // Nothing to free is never worth it, however cheap the rebuild.
    let c = clock(now, WARM, 0);
    assert_eq!(
        decide(&c, now, Some(TTL), 0, Some(&price()), 50_000, WINDOW),
        None
    );
}

#[test]
fn automatic_cache_without_a_write_price_still_pays_for_the_miss() {
    let now = Instant::now();
    // OpenAI-style: writes are free, reads cost a tenth. Losing the prefix still costs
    // (input - read) per token; a literal `cache_write - cache_read` would be negative
    // and make every collection look free.
    let p = Price {
        input: 1.25,
        output: 10.0,
        cache_read: 0.125,
        cache_write: 0.0,
    };
    let c = clock(now, WARM, 100_000);
    // cost = 100k x (1.25 - 0.125) = 112.5k; saved = 5k x 1.25 x 5 = 31.25k
    assert_eq!(
        decide(&c, now, Some(TTL), 5_000, Some(&p), 50_000, WINDOW),
        None
    );
    // saved = 20k x 1.25 x 5 = 125k
    assert_eq!(
        decide(&c, now, Some(TTL), 20_000, Some(&p), 50_000, WINDOW),
        Some(MutationReason::CostBenefit)
    );
}

#[test]
fn forced_at_90_percent() {
    let now = Instant::now();
    let c = clock(now, WARM, 100_000);
    // Warm, and one token freed against a huge prefix: only the 90% rule applies.
    assert_eq!(
        decide(&c, now, Some(TTL), 1, Some(&price()), 90_000, WINDOW),
        Some(MutationReason::Forced)
    );
    assert_eq!(
        decide(&c, now, Some(TTL), 1, Some(&price()), 99_999, WINDOW),
        Some(MutationReason::Forced)
    );
    // Also without a price.
    assert_eq!(
        decide(&c, now, Some(TTL), 1, None, 90_000, WINDOW),
        Some(MutationReason::Forced)
    );
    assert_eq!(
        decide(&c, now, Some(TTL), 1, Some(&price()), 89_999, WINDOW),
        None
    );
    // A cold cache is reported as cold, not forced.
    let cold = clock(now, TTL, 100_000);
    assert_eq!(
        decide(&cold, now, Some(TTL), 1, Some(&price()), 95_000, WINDOW),
        Some(MutationReason::ColdCache)
    );
    // An empty window never forces.
    assert_eq!(decide(&c, now, Some(TTL), 1, None, 10, 0), None);
}

#[test]
fn no_price_falls_back_to_threshold() {
    let now = Instant::now();
    let c = clock(now, WARM, 100_000);
    // max(2000, 5% of 100k) = 5000
    assert_eq!(threshold_tokens(100_000), 5_000);
    assert_eq!(threshold_tokens(10_000), 2_000);
    assert_eq!(threshold_tokens(0), 2_000);
    assert_eq!(
        decide(&c, now, Some(TTL), 4_999, None, 50_000, WINDOW),
        None
    );
    assert_eq!(
        decide(&c, now, Some(TTL), 5_000, None, 50_000, WINDOW),
        Some(MutationReason::CostBenefit)
    );
    // The floor of 2000 on a small window.
    assert_eq!(decide(&c, now, Some(TTL), 1_999, None, 5_000, 10_000), None);
    assert_eq!(
        decide(&c, now, Some(TTL), 2_000, None, 5_000, 10_000),
        Some(MutationReason::CostBenefit)
    );
}

#[test]
fn note_request_sets_the_clock() {
    let mut ctx = Context::default();
    assert_eq!(ctx.clock, CacheClock::default());
    let at = Instant::now();
    ctx.note_request(at, 12_345);
    assert_eq!(
        ctx.clock,
        CacheClock {
            last_request: Some(at),
            cached_prefix_tokens: 12_345
        }
    );
}

fn status(used: u32) -> ContextStatus {
    ContextStatus {
        used,
        budget: WINDOW,
        ..Default::default()
    }
}

/// A history of `n` turns of about 1000 tokens each.
fn big_history(n: usize) -> Context {
    let text = "x".repeat(4_000);
    Context {
        history: (0..n)
            .map(|_| Turn {
                messages: vec![Message {
                    role: Role::User,
                    content: vec![Content::Text(text.clone())],
                }],
            })
            .collect(),
        ..Default::default()
    }
}

#[test]
fn compaction_deferred_between_80_and_90_while_warm() {
    let now = Instant::now();
    let t = Some(DEFAULT_THRESHOLD);
    // Compaction would free about 7 turns x 1000 tokens = 7k tokens; rebuilding the 80k
    // prefix costs 276k against 7k x 3 x 5 = 105k saved.
    let mut c = big_history(11);
    c.note_request(now - WARM, 80_000);
    let p = price();
    assert!(!c.needs_compaction(&status(80_000), t, now, Some(TTL), Some(&p)));
    assert!(!c.needs_compaction(&status(89_999), t, now, Some(TTL), Some(&p)));
    assert_eq!(
        c.compaction_decision(&status(85_000), t, now, Some(TTL), Some(&p)),
        None
    );
    // Never beyond 90%.
    assert!(c.needs_compaction(&status(90_000), t, now, Some(TTL), Some(&p)));
    assert_eq!(
        c.compaction_decision(&status(95_000), t, now, Some(TTL), Some(&p)),
        Some(MutationReason::Forced)
    );
    // Below the threshold nothing is considered, cold or not.
    assert!(!c.needs_compaction(&status(79_999), t, now, Some(TTL), Some(&p)));
    assert!(!c.needs_compaction(&status(79_999), t, now + TTL * 2, Some(TTL), Some(&p)));
    // A cold cache compacts at the threshold.
    assert_eq!(
        c.compaction_decision(&status(80_000), t, now + TTL, Some(TTL), Some(&p)),
        Some(MutationReason::ColdCache)
    );
    // So does a warm one when compaction frees enough to repay the rebuild: 60 turns free
    // 56k tokens against a 10k prefix.
    let mut big = big_history(60);
    big.note_request(now - WARM, 10_000);
    assert_eq!(
        big.compaction_decision(&status(85_000), t, now, Some(TTL), Some(&p)),
        Some(MutationReason::CostBenefit)
    );
    // The threshold stays configurable and can be disabled.
    assert!(!big.needs_compaction(&status(99_999), None, now, Some(TTL), Some(&p)));
}

#[test]
fn forced_compaction_at_90_percent_whatever_the_threshold() {
    let now = Instant::now();
    let p = price();
    let mut c = big_history(11);
    c.note_request(now - WARM, 80_000);
    // A threshold above 90% (or 100%) does not push the forced rule out of reach, and the
    // warm-cache cost check does not apply to it.
    for t in [0.95_f32, 1.0, 2.0] {
        assert_eq!(
            c.compaction_decision(&status(90_000), Some(t), now, Some(TTL), Some(&p)),
            Some(MutationReason::Forced),
            "threshold {t}"
        );
        assert_eq!(
            c.compaction_decision(&status(120_000), Some(t), now, Some(TTL), Some(&p)),
            Some(MutationReason::Forced)
        );
        assert!(!c.needs_compaction(&status(89_999), Some(t), now, Some(TTL), Some(&p)));
    }
    // Disabling compaction (spec 6.4) still disables everything, forced included.
    for t in [None, Some(0.0)] {
        assert!(!c.needs_compaction(&status(95_000), t, now, Some(TTL), Some(&p)));
    }
}
