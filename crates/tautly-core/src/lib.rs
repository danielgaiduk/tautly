//! The Tautly engine: [`EngineBuilder`] starts a session, which takes [`Command`]s and
//! emits [`Event`]s; plus configuration, authentication, budgets, the cost [`Ledger`] and
//! signal handling ([`install_signal_handlers`]). Clients (the REPL, the JSON-RPC server)
//! speak only these types.

mod approvals;
pub mod attempts;
pub mod auth;
pub mod budget;
pub mod checkpoint;
pub mod config;
pub mod detect;
pub mod diff;
mod dispatch;
pub mod engine;
pub mod evidence;
pub mod ledger;
pub mod mcp;
mod memory;
mod model_switch;
pub mod protocol;
mod provider_setup;
pub mod record;
pub mod resume;
pub mod routing;
mod session;
pub mod sessions;
pub mod signals;
pub mod turn;
pub mod undo;
pub mod workspace;

pub use auth::{AuthError, KEYCHAIN_SERVICE, KeyLookup, KeyringStore, SecretStore, api_key};
pub use budget::{Budget, BudgetState};
pub use checkpoint::{CheckpointError, CheckpointRef, Checkpointer, RestorePlan, Skip};
pub use config::{
    BudgetConfig, BudgetLimits, CheckpointConfig, CliOverrides, CompatEntry, Config, ConfigError,
    LoadedConfig, ProviderEntry, ProvidersConfig, RoutingConfig, TelemetryConfig, config_home,
};
pub use engine::{Engine, EngineBuilder, EngineError, session_root};
pub use evidence::{
    DEFAULT_TEST_COMMANDS, EvidenceConfig, EvidenceTracker, TestEvidence, TestMatcher,
};
pub use ledger::{Ledger, LedgerError, Totals};
pub use mcp::{Approval, McpPlan, NoPreExec, PlannedServer, PreExecHook, plan_servers};
pub use protocol::{
    BudgetScope, Command, DiffFile, DiffStatus, EffectView, ErrorKind, Event, FinishReason,
    MAX_REPLACED_SHOWN, McpState, PROTOCOL_VERSION, ToolMetaView, UsageRole,
};
pub use provider_setup::{
    ProviderFactory, ProviderSetupError, known_provider, provider_for_with, provider_label,
};
pub use record::SessionRecord;
pub use signals::{Signal, SignalPolicy, SignalRouter, handle_signals, install_signal_handlers};
pub use workspace::{Workspace, Workspaces};
// Types that appear in `Event` and `Command`, re-exported so clients need no other crate.
pub use tautly_context::{Breakdown, MutationReason};
pub use tautly_mcp::{McpConfig, ServerSource, ServerSpec};
pub use tautly_memory::{MemAnchor, MemoryChange, MemoryEntry};
pub use tautly_provider::{ModelId, Quota};
// What a client needs to fill `CliOverrides` and to find the data directory.
pub use tautly_sandbox::Mode;
pub use tautly_session::paths::data_home;
pub use tautly_tools::DiffPreview;
