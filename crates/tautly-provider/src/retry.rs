//! Retry wrapper: re-issues a request on transient errors before any content is yielded.
//!
//! Adapters emit `Quota` (from response headers) and may emit `Usage` before any content,
//! so those are held back until the first content-bearing event: a transient error before
//! content discards them and retries, as if the attempt never happened, except that its
//! billed usage is passed on as [`ProviderEvent::DiscardedUsage`] so cost is never
//! under-reported.

use std::time::Duration;

use futures::StreamExt;
use futures::stream::BoxStream;
use rand::Rng;
use tokio_util::sync::CancellationToken;

use crate::{CanonicalRequest, Provider, ProviderError, ProviderEvent, RawUsage};

pub struct Retrying<P> {
    inner: P,
    max_attempts: u32,
    base: Duration,
}

impl<P> Retrying<P> {
    pub fn new(inner: P) -> Self {
        Self::with_policy(inner, 5, Duration::from_millis(500))
    }
    pub fn with_policy(inner: P, max_attempts: u32, base: Duration) -> Self {
        Self {
            inner,
            max_attempts,
            base,
        }
    }
}

impl<P> Retrying<P> {
    /// `base * 2^failures` with +-25% jitter.
    fn backoff(base: Duration, failures: u32) -> Duration {
        let exp = base.saturating_mul(2u32.saturating_pow(failures));
        exp.mul_f64(rand::rng().random_range(0.75..=1.25))
    }
}

impl<P: Provider + Clone + 'static> Provider for Retrying<P> {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn stream(
        &self,
        req: CanonicalRequest,
        cancel: CancellationToken,
    ) -> BoxStream<'static, Result<ProviderEvent, ProviderError>> {
        let inner = self.inner.clone();
        let (max_attempts, base) = (self.max_attempts.max(1), self.base);
        Box::pin(async_stream::stream! {
            let mut failures = 0u32;
            loop {
                if cancel.is_cancelled() {
                    return;
                }
                let mut s = inner.stream(req.clone(), cancel.clone());
                // Metadata events held back until the first content event of this attempt.
                let mut held: Vec<ProviderEvent> = Vec::new();
                let mut content = false;
                let mut retry_err = None;
                while let Some(item) = s.next().await {
                    match item {
                        Ok(ev) if !content && !is_content(&ev) => held.push(ev),
                        Err(e) if !content && e.is_transient() => {
                            retry_err = Some(e);
                            break;
                        }
                        other => {
                            content = true;
                            for ev in held.drain(..) {
                                yield Ok(ev);
                            }
                            yield other;
                        }
                    }
                }
                let Some(err) = retry_err else {
                    // Ended without content: deliver what was held.
                    for ev in held {
                        yield Ok(ev);
                    }
                    return;
                };
                failures += 1;
                if failures >= max_attempts {
                    for ev in held {
                        yield Ok(ev);
                    }
                    yield Err(err);
                    return;
                }
                // `Usage` is cumulative within an attempt: the last one is its total.
                // Nothing billed, nothing to pass on.
                if let Some(u) = held
                    .iter()
                    .rev()
                    .find_map(|ev| match ev {
                        ProviderEvent::Usage(u) => Some(*u),
                        _ => None,
                    })
                    .filter(|u| *u != RawUsage::default())
                {
                    yield Ok(ProviderEvent::DiscardedUsage(u));
                }
                let delay = match &err {
                    ProviderError::Transient { retry_after: Some(d), .. } => *d,
                    _ => Self::backoff(base, failures - 1),
                };
                tokio::select! {
                    () = tokio::time::sleep(delay) => {}
                    () = cancel.cancelled() => return,
                }
            }
        })
    }
}

/// Whether an event carries response content (as opposed to quota/usage metadata).
fn is_content(ev: &ProviderEvent) -> bool {
    !matches!(
        ev,
        ProviderEvent::Quota(_) | ProviderEvent::Usage(_) | ProviderEvent::DiscardedUsage(_)
    )
}
