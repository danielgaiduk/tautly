//! Canonical provider types, capabilities, model catalog and cost accounting.

pub mod anthropic;
pub mod canonical;
pub mod capabilities;
pub mod catalog;
pub mod compat;
pub mod cost;
pub mod error;
pub mod google;
pub mod openai;
pub(crate) mod util;

pub use canonical::*;
pub use capabilities::*;
pub use catalog::{Catalog, CatalogError, ModelInfo, Price, Tier};
pub use cost::cost_usd;
pub use error::ProviderError;
pub mod mock;
pub mod retry;
pub mod sse;

pub use mock::{MockProvider, MockTurn};
pub use retry::Retrying;

use async_trait::async_trait;
use futures::stream::BoxStream;
use tokio_util::sync::CancellationToken;

/// A cheap model for work off the main conversation: summaries, memory work and `web_fetch`
/// answers (spec 7.5). One request, no tools, no reasoning; the answer's text comes back,
/// and what the request cost is reported by the implementation to its owner.
#[async_trait]
pub trait SideModel: Send + Sync {
    async fn complete(
        &self,
        system: &str,
        user: &str,
        max_tokens: u32,
    ) -> Result<String, ProviderError>;
}

/// A streaming model backend. Adapters implement this; the engine wraps them in [`Retrying`].
pub trait Provider: Send + Sync {
    fn name(&self) -> &str;
    fn stream(
        &self,
        req: CanonicalRequest,
        cancel: CancellationToken,
    ) -> BoxStream<'static, Result<ProviderEvent, ProviderError>>;
}

/// Providers are shared behind `Arc<dyn Provider>` (engine, side runner) and wrapped
/// in [`Retrying`], which needs `Provider + Clone`.
impl<T: Provider + ?Sized> Provider for std::sync::Arc<T> {
    fn name(&self) -> &str {
        (**self).name()
    }
    fn stream(
        &self,
        req: CanonicalRequest,
        cancel: CancellationToken,
    ) -> BoxStream<'static, Result<ProviderEvent, ProviderError>> {
        (**self).stream(req, cancel)
    }
}

impl<T: Provider + ?Sized> Provider for Box<T> {
    fn name(&self) -> &str {
        (**self).name()
    }
    fn stream(
        &self,
        req: CanonicalRequest,
        cancel: CancellationToken,
    ) -> BoxStream<'static, Result<ProviderEvent, ProviderError>> {
        (**self).stream(req, cancel)
    }
}
