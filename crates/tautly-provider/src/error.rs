use std::time::Duration;

/// Why a provider request failed.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ProviderError {
    /// Worth retrying: rate limits, overload, server errors, broken connections.
    #[error("transient provider error (status {status:?}): {msg}")]
    Transient {
        /// The HTTP status, when there was one.
        status: Option<u16>,
        /// How long the provider asked to wait.
        retry_after: Option<Duration>,
        /// What happened.
        msg: String,
    },
    /// The key was missing, wrong or not allowed.
    #[error("authentication failed: {0}")]
    Auth(String),
    /// The provider rejected the request.
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    /// The request did not fit the context window.
    #[error("context window exceeded: {0}")]
    ContextOverflow(String),
    /// Anything else.
    #[error("provider error: {0}")]
    Other(String),
}

impl ProviderError {
    /// True when retrying the same request may succeed.
    pub fn is_transient(&self) -> bool {
        matches!(self, ProviderError::Transient { .. })
    }
}
