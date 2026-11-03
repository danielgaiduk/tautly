//! Spending limits (spec 4.6): a turn budget (per user message) and a session budget, in
//! USD and/or tokens. Every provider response counts, main and side model alike, and so
//! does a response that was discarded and sent again. Tokens are everything billed: input,
//! output and both cache kinds.
//!
//! At [`WARN_AT`] of a limit a warning is shown; at the limit the engine stops at the next
//! tool boundary (a call already running finishes, calls not started are answered "not
//! run") and the turn ends with `TurnFinished{budget}`. A session budget that is used up
//! stops every later message before its first request, until `SetBudget` raises it.
//!
//! A model without a catalog price has no USD cost: a USD limit cannot be enforced for it,
//! which is said once, and only the token limits apply.

use tautly_provider::RawUsage;

use crate::config::{BudgetConfig, BudgetLimits};
use crate::protocol::BudgetScope;

/// Share of a limit at which [`Budget::check`] says [`BudgetState::Warn`].
pub const WARN_AT: f64 = 0.8;

/// A spending limit; `None` = unlimited.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Budget {
    pub max_usd: Option<f64>,
    pub max_tokens: Option<u64>,
}

/// Where spending stands against a [`Budget`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum BudgetState {
    Ok,
    /// At least [`WARN_AT`] of a limit is spent.
    Warn,
    /// A limit is reached.
    Exceeded,
}

impl Budget {
    pub fn check(&self, spent_usd: f64, spent_tokens: u64) -> BudgetState {
        let share = self.share(spent_usd, spent_tokens);
        if share >= 1.0 {
            BudgetState::Exceeded
        } else if share >= WARN_AT {
            BudgetState::Warn
        } else {
            BudgetState::Ok
        }
    }

    /// The largest share of a limit spent (0 without limits). A zero limit is spent at once.
    fn share(&self, spent_usd: f64, spent_tokens: u64) -> f64 {
        let usd = self.max_usd.map_or(0.0, |max| ratio(spent_usd, max));
        let tokens = self
            .max_tokens
            .map_or(0.0, |max| ratio(spent_tokens as f64, max as f64));
        usd.max(tokens)
    }

    /// This budget without its USD limit (the model has no price).
    fn tokens_only(self) -> Budget {
        Budget {
            max_usd: None,
            ..self
        }
    }
}

fn ratio(spent: f64, max: f64) -> f64 {
    if max <= 0.0 {
        f64::INFINITY
    } else {
        spent / max
    }
}

impl From<BudgetLimits> for Budget {
    fn from(l: BudgetLimits) -> Self {
        Budget {
            max_usd: l.max_usd,
            max_tokens: l.max_tokens,
        }
    }
}

/// Every token billed for `u`.
pub fn billed_tokens(u: &RawUsage) -> u64 {
    u.input + u.output + u.cache_read + u.cache_write
}

/// One scope's limit, spend, and the state last reported for it.
#[derive(Debug, Clone, Copy)]
struct Scope {
    limit: Budget,
    usd: f64,
    tokens: u64,
    reported: BudgetState,
}

impl Scope {
    fn new(limit: Budget) -> Self {
        Scope {
            limit,
            usd: 0.0,
            tokens: 0,
            reported: BudgetState::Ok,
        }
    }
}

/// A session's two budgets and what has been spent against them; shared by the actor
/// (`SetBudget`) and the running turn (spending and checks).
#[derive(Debug)]
pub(crate) struct Budgets {
    turn: Scope,
    session: Scope,
    /// The model has a catalog price, so USD limits apply.
    priced: bool,
    model: String,
    unpriced_reported: bool,
}

impl Budgets {
    pub fn new(config: &BudgetConfig, priced: bool, model: &str) -> Self {
        Budgets {
            turn: Scope::new(config.turn.into()),
            session: Scope::new(config.session.into()),
            priced,
            model: model.to_string(),
            unpriced_reported: false,
        }
    }

    /// A new user message: the turn budget starts again.
    pub fn start_turn(&mut self) {
        self.turn = Scope::new(self.turn.limit);
    }

    /// Replace one scope's limits (`SetBudget`); its state is reported afresh.
    pub fn set(&mut self, scope: BudgetScope, limit: Budget) {
        let s = self.scope_mut(scope);
        s.limit = limit;
        s.reported = BudgetState::Ok;
    }

    /// Count one provider response.
    pub fn spend(&mut self, usage: &RawUsage, cost: Option<f64>) {
        let tokens = billed_tokens(usage);
        let usd = cost.unwrap_or(0.0);
        for s in [&mut self.turn, &mut self.session] {
            s.tokens = s.tokens.saturating_add(tokens);
            s.usd += usd;
        }
    }

    /// Whether a limit is reached: the loop stops at the next tool boundary.
    pub fn exceeded(&self) -> Option<BudgetScope> {
        [BudgetScope::Session, BudgetScope::Turn]
            .into_iter()
            .find(|&scope| self.state(scope) == BudgetState::Exceeded)
    }

    /// What the user has not been told yet: a scope that reached 80% or its limit, and
    /// (once per session) a USD limit that cannot apply to an unpriced model.
    pub fn news(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        let usd_limit = self.turn.limit.max_usd.is_some() || self.session.limit.max_usd.is_some();
        if !self.priced && usd_limit && !self.unpriced_reported {
            self.unpriced_reported = true;
            out.push(format!(
                "the model {} has no known price, so the USD budget cannot be enforced; \
                 only the token budgets apply",
                self.model
            ));
        }
        for scope in [BudgetScope::Turn, BudgetScope::Session] {
            let state = self.state(scope);
            let s = self.scope_mut(scope);
            if state <= s.reported {
                continue;
            }
            s.reported = state;
            let (limit, usd, tokens) = (s.limit, s.usd, s.tokens);
            let name = match scope {
                BudgetScope::Turn => "turn",
                BudgetScope::Session => "session",
            };
            let detail = self.detail(limit, usd, tokens);
            out.push(match state {
                BudgetState::Exceeded => format!(
                    "{name} budget exhausted ({detail}); no further requests will be sent, and tool \
                     calls not yet started will not run"
                ),
                _ => {
                    let pct = (self.effective(limit).share(usd, tokens) * 100.0 + 1e-9).floor();
                    format!("{name} budget {pct:.0}% used ({detail})")
                }
            });
        }
        out
    }

    fn state(&self, scope: BudgetScope) -> BudgetState {
        let s = match scope {
            BudgetScope::Turn => &self.turn,
            BudgetScope::Session => &self.session,
        };
        self.effective(s.limit).check(s.usd, s.tokens)
    }

    fn effective(&self, limit: Budget) -> Budget {
        if self.priced {
            limit
        } else {
            limit.tokens_only()
        }
    }

    fn detail(&self, limit: Budget, usd: f64, tokens: u64) -> String {
        let limit = self.effective(limit);
        let mut parts = Vec::new();
        if let Some(max) = limit.max_usd {
            parts.push(format!("${usd:.4} of ${max:.4}"));
        }
        if let Some(max) = limit.max_tokens {
            parts.push(format!("{tokens} of {max} tokens"));
        }
        parts.join(", ")
    }

    fn scope_mut(&mut self, scope: BudgetScope) -> &mut Scope {
        match scope {
            BudgetScope::Turn => &mut self.turn,
            BudgetScope::Session => &mut self.session,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn warn_from_80_percent_exceeded_from_100() {
        let b = Budget {
            max_usd: None,
            max_tokens: Some(1000),
        };
        assert_eq!(b.check(0.0, 0), BudgetState::Ok);
        assert_eq!(b.check(0.0, 799), BudgetState::Ok);
        assert_eq!(b.check(0.0, 800), BudgetState::Warn);
        assert_eq!(b.check(0.0, 999), BudgetState::Warn);
        assert_eq!(b.check(0.0, 1000), BudgetState::Exceeded);
        assert_eq!(b.check(5.0, 10), BudgetState::Ok, "no USD limit");
    }

    #[test]
    fn the_nearer_limit_decides() {
        let b = Budget {
            max_usd: Some(1.0),
            max_tokens: Some(1000),
        };
        assert_eq!(b.check(0.85, 10), BudgetState::Warn);
        assert_eq!(b.check(0.1, 1000), BudgetState::Exceeded);
        assert_eq!(b.check(1.0, 0), BudgetState::Exceeded);
        assert_eq!(Budget::default().check(1e9, u64::MAX), BudgetState::Ok);
    }

    fn tokens(n: u64) -> RawUsage {
        RawUsage {
            input: n,
            ..RawUsage::default()
        }
    }

    fn limits(turn_tokens: Option<u64>, session_usd: Option<f64>) -> BudgetConfig {
        BudgetConfig {
            turn: BudgetLimits {
                max_usd: None,
                max_tokens: turn_tokens,
            },
            session: BudgetLimits {
                max_usd: session_usd,
                max_tokens: None,
            },
        }
    }

    #[test]
    fn each_threshold_is_reported_once_per_scope() {
        let mut b = Budgets::new(&limits(Some(100), Some(1.0)), true, "m");
        b.spend(&tokens(85), Some(0.1));
        assert_eq!(b.news(), ["turn budget 85% used (85 of 100 tokens)"]);
        assert!(b.news().is_empty());
        b.spend(&tokens(10), Some(0.75));
        assert_eq!(b.news(), ["session budget 85% used ($0.8500 of $1.0000)"]);
        assert_eq!(b.exceeded(), None);
        b.spend(&tokens(5), None);
        assert_eq!(b.exceeded(), Some(BudgetScope::Turn));
        assert_eq!(
            b.news(),
            [
                "turn budget exhausted (100 of 100 tokens); no further requests will be \
                 sent, and tool calls not yet started will not run"
            ]
        );
        // A new message starts the turn budget again; the session spend stays.
        b.start_turn();
        assert_eq!(b.exceeded(), None);
        assert!(b.news().is_empty());
    }

    #[test]
    fn set_budget_replaces_a_scope_and_reports_it_afresh() {
        let mut b = Budgets::new(&BudgetConfig::default(), true, "m");
        b.spend(&tokens(90), None);
        assert!(b.news().is_empty());
        b.set(
            BudgetScope::Session,
            Budget {
                max_usd: None,
                max_tokens: Some(50),
            },
        );
        assert_eq!(b.exceeded(), Some(BudgetScope::Session));
        assert_eq!(b.news().len(), 1);
        b.set(
            BudgetScope::Session,
            Budget {
                max_usd: None,
                max_tokens: Some(100),
            },
        );
        assert_eq!(b.exceeded(), None);
        assert_eq!(b.news(), ["session budget 90% used (90 of 100 tokens)"]);
    }

    #[test]
    fn an_unpriced_model_keeps_only_token_limits_and_says_so_once() {
        let mut b = Budgets::new(&limits(Some(100), Some(0.01)), false, "x/free");
        let news = b.news();
        assert_eq!(news.len(), 1);
        assert!(news[0].contains("x/free has no known price"), "{news:?}");
        b.spend(&tokens(50), None);
        assert!(b.news().is_empty(), "the USD limit must not count as spent");
        b.spend(&tokens(50), None);
        assert_eq!(b.exceeded(), Some(BudgetScope::Turn));
        b.start_turn();
        assert!(b.news().is_empty());
    }

    #[test]
    fn a_zero_limit_is_exhausted_at_once() {
        let b = Budget {
            max_usd: Some(0.0),
            max_tokens: None,
        };
        assert_eq!(b.check(0.0, 0), BudgetState::Exceeded);
    }

    proptest! {
        /// Spending only ever moves the state up, and it is `Exceeded` exactly when the
        /// sum reached a limit.
        #[test]
        fn accounting_is_monotonic(
            max in 1u64..10_000,
            spends in proptest::collection::vec(0u64..2_000, 0..30),
        ) {
            let b = Budget { max_usd: None, max_tokens: Some(max) };
            let mut spent = 0u64;
            let mut last = BudgetState::Ok;
            for s in spends {
                spent += s;
                let now = b.check(0.0, spent);
                prop_assert!(now >= last);
                prop_assert_eq!(now == BudgetState::Exceeded, spent >= max);
                prop_assert_eq!(now >= BudgetState::Warn, spent as f64 >= 0.8 * max as f64);
                last = now;
            }
        }
    }
}
