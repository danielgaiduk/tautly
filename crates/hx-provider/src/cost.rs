use crate::{canonical::RawUsage, catalog::Price};

/// Cost in USD of `u` at price `p` (per million tokens); `None` when the price is unknown.
pub fn cost_usd(u: &RawUsage, p: Option<&Price>) -> Option<f64> {
    let p = p?;
    let m = |tokens: u64, rate: f64| tokens as f64 * rate / 1_000_000.0;
    Some(m(u.input, p.input) + m(u.output, p.output) + m(u.cache_read, p.cache_read) + m(u.cache_write, p.cache_write))
}
