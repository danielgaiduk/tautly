use std::time::Duration;

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ProviderError {
    #[error("transient provider error (status {status:?}): {msg}")]
    Transient { status: Option<u16>, retry_after: Option<Duration>, msg: String },
    #[error("authentication failed: {0}")]
    Auth(String),
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("context window exceeded: {0}")]
    ContextOverflow(String),
    #[error("provider error: {0}")]
    Other(String),
}

impl ProviderError {
    /// True when retrying the same request may succeed.
    pub fn is_transient(&self) -> bool {
        matches!(self, ProviderError::Transient { .. })
    }
}
