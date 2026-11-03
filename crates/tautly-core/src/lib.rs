//! The Tautly engine: [`EngineBuilder`] starts a session, which takes [`Command`]s and
//! emits [`Event`]s; plus configuration, authentication, budgets, the cost [`Ledger`] and
//! signal handling ([`install_signal_handlers`]). Clients (the REPL, the JSON-RPC server)
//! speak only these types.

mod approvals;
pub mod attempts;
pub mod auth;
pub mod budget;
pub mod config;
pub mod diff;
mod dispatch;
pub mod engine;
pub mod evidence;
pub mod ledger;
pub mod mcp;
pub mod protocol;
mod provider_setup;
pub mod record;
mod session;
pub mod signals;
pub mod turn;
pub mod workspace;

pub use auth::{AuthError, KEYCHAIN_SERVICE, KeyLookup, KeyringStore, SecretStore, api_key};
pub use budget::{Budget, BudgetState};
pub use config::{
    BudgetConfig, BudgetLimits, CliOverrides, CompatEntry, Config, ConfigError, LoadedConfig,
    ProviderEntry, ProvidersConfig, RoutingConfig, TelemetryConfig, config_home,
};
pub use engine::{Engine, EngineBuilder, EngineError};
pub use evidence::{
    DEFAULT_TEST_COMMANDS, EvidenceConfig, EvidenceTracker, TestEvidence, TestMatcher,
};
pub use ledger::{Ledger, LedgerError, Totals};
pub use mcp::{Approval, McpPlan, NoPreExec, PlannedServer, PreExecHook, plan_servers};
pub use protocol::{
    BudgetScope, Command, DiffFile, DiffStatus, EffectView, ErrorKind, Event, FinishReason,
    McpState, PROTOCOL_VERSION, ToolMetaView, UsageRole,
};
pub use record::SessionRecord;
pub use signals::{Signal, SignalPolicy, SignalRouter, handle_signals, install_signal_handlers};
pub use workspace::Workspace;
// Types that appear in `Event` and `Command`, re-exported so clients need no other crate.
pub use tautly_context::{Breakdown, MutationReason};
pub use tautly_mcp::{McpConfig, ServerSource, ServerSpec};
pub use tautly_memory::{MemAnchor, MemoryChange, MemoryEntry};
pub use tautly_provider::Quota;
pub use tautly_tools::DiffPreview;
