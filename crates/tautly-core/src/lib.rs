//! The Tautly engine: [`EngineBuilder`] starts a session, which takes [`Command`]s and
//! emits [`Event`]s; plus configuration and authentication. Clients (the REPL, the
//! JSON-RPC server) speak only these types.

mod approvals;
pub mod attempts;
pub mod auth;
pub mod config;
mod dispatch;
pub mod engine;
pub mod protocol;
mod provider_setup;
pub mod record;
mod session;
pub mod turn;
pub mod workspace;

pub use auth::{AuthError, KEYCHAIN_SERVICE, KeyLookup, KeyringStore, SecretStore, api_key};
pub use config::{
    BudgetConfig, BudgetLimits, CliOverrides, CompatEntry, Config, ConfigError, LoadedConfig,
    ProviderEntry, ProvidersConfig, RoutingConfig, TelemetryConfig, config_home,
};
pub use engine::{Engine, EngineBuilder, EngineError};
pub use protocol::{
    BudgetScope, Command, EffectView, ErrorKind, Event, FinishReason, PROTOCOL_VERSION,
    ToolMetaView, UsageRole,
};
pub use record::SessionRecord;
pub use workspace::Workspace;
// Types that appear in `Event` and `Command`, re-exported so clients need no other crate.
pub use tautly_context::{Breakdown, MutationReason};
pub use tautly_memory::{MemAnchor, MemoryChange, MemoryEntry};
pub use tautly_provider::Quota;
