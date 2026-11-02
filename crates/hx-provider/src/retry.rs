//! Retry wrapper: re-issues a request on transient errors before any event is yielded.

use std::time::Duration;

use futures::StreamExt;
use futures::stream::BoxStream;
use rand::Rng;
use tokio_util::sync::CancellationToken;

use crate::{CanonicalRequest, Provider, ProviderError, ProviderEvent};

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
        Self { inner, max_attempts, base }
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

    fn stream(&self, req: CanonicalRequest, cancel: CancellationToken) -> BoxStream<'static, Result<ProviderEvent, ProviderError>> {
        let inner = self.inner.clone();
        let (max_attempts, base) = (self.max_attempts.max(1), self.base);
        Box::pin(async_stream::stream! {
            let mut failures = 0u32;
            loop {
                if cancel.is_cancelled() {
                    return;
                }
                let mut s = inner.stream(req.clone(), cancel.clone());
                let mut yielded = false;
                let mut retry_err = None;
                while let Some(item) = s.next().await {
                    match item {
                        Err(e) if !yielded && e.is_transient() => {
                            retry_err = Some(e);
                            break;
                        }
                        other => {
                            yielded = true;
                            yield other;
                        }
                    }
                }
                let Some(err) = retry_err else { return };
                failures += 1;
                if failures >= max_attempts {
                    yield Err(err);
                    return;
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
