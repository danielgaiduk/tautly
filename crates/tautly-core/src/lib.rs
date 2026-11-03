//! The Tautly engine's public surface: configuration, authentication and the client
//! protocol ([`Event`], [`Command`]). Clients (the REPL, the JSON-RPC server) speak only
//! these types.

pub mod auth;
pub mod config;
pub mod protocol;

pub use auth::{AuthError, KEYCHAIN_SERVICE, KeyLookup, KeyringStore, SecretStore, api_key};
pub use config::{
    BudgetConfig, BudgetLimits, CliOverrides, CompatEntry, Config, ConfigError, LoadedConfig,
    ProviderEntry, ProvidersConfig, RoutingConfig, TelemetryConfig, config_home,
};
pub use protocol::{
    BudgetScope, Command, EffectView, ErrorKind, Event, FinishReason, PROTOCOL_VERSION,
    ToolMetaView, UsageRole,
};
// Types that appear in `Event` and `Command`, re-exported so clients need no other crate.
pub use tautly_context::{Breakdown, MutationReason};
pub use tautly_memory::{MemAnchor, MemoryChange, MemoryEntry};
pub use tautly_provider::Quota;
