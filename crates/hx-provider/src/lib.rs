//! Canonical provider types, capabilities, model catalog and cost accounting.

pub mod anthropic;
pub mod canonical;
pub mod capabilities;
pub mod catalog;
pub mod cost;
pub mod error;
pub mod openai;
pub(crate) mod util;

pub use canonical::*;
pub use capabilities::*;
pub use catalog::{Catalog, CatalogError, ModelInfo, Price};
pub use cost::cost_usd;
pub use error::ProviderError;
pub mod mock;
pub mod retry;
pub mod sse;

pub use mock::{MockProvider, MockTurn};
pub use retry::Retrying;

use futures::stream::BoxStream;
use tokio_util::sync::CancellationToken;

/// A streaming model backend. Adapters implement this; the engine wraps them in [`Retrying`].
pub trait Provider: Send + Sync {
    fn name(&self) -> &str;
    fn stream(
        &self,
        req: CanonicalRequest,
        cancel: CancellationToken,
    ) -> BoxStream<'static, Result<ProviderEvent, ProviderError>>;
}
