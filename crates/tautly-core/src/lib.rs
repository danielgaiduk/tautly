//! The Tautly engine: [`EngineBuilder`] starts a session, which takes [`Command`]s and
//! emits [`Event`]s; plus configuration, authentication, budgets, the cost [`Ledger`] and
//! signal handling ([`install_signal_handlers`]). Clients (the REPL, the JSON-RPC server)
//! speak only these types.

mod approvals;
pub use approvals::exact_rule;
pub mod attempts;
pub mod auth;
pub mod bisect;
pub mod budget;
pub mod checkpoint;
pub mod compare;
pub mod config;
pub mod detect;
pub mod diff;
mod dispatch;
pub mod engine;
pub mod evidence;
pub mod lease;
pub mod ledger;
pub mod mcp;
mod memory;
mod model_switch;
pub mod protocol;
mod provider_setup;
pub mod reasoning;
pub mod receipt;
pub mod record;
pub mod replay;
pub mod resume;
pub mod routing;
mod session;
pub mod sessions;
pub mod signals;
pub mod timetravel;
pub mod tree;
pub mod turn;
pub mod undo;
pub mod workspace;
pub mod worktree;

pub use auth::{AuthError, KEYCHAIN_SERVICE, KeyLookup, KeyringStore, SecretStore, api_key};
pub use budget::{Budget, BudgetState};
pub use checkpoint::{CheckpointError, CheckpointRef, Checkpointer, RestorePlan, Skip};
pub use config::{
    BudgetConfig, BudgetLimits, CheckpointConfig, CliOverrides, CompatEntry, Config, ConfigError,
    LoadedConfig, MODELS_FILE, ProviderEntry, ProvidersConfig, ReceiptsConfig, RoutingConfig,
    TelemetryConfig, config_home, parse_models_file,
};
pub use engine::{Engine, EngineBuilder, EngineError, session_root, sessions_running};
pub use evidence::{
    DEFAULT_TEST_COMMANDS, EvidenceConfig, EvidenceTracker, TestEvidence, TestMatcher,
};
pub use lease::{ExclusiveGuard, Lease, LeaseError};
pub use ledger::{GroupBy, Ledger, LedgerError, StatsRow, StoredReceipt, Totals};
pub use mcp::{
    Approval, McpPlan, NoPreExec, PlannedServer, PreExecHook, plan_servers, provider_key_vars,
};
pub use protocol::{
    BudgetScope, Command, DiffFile, DiffStatus, EffectView, ErrorKind, Event, FinishReason,
    MAX_REPLACED_SHOWN, McpState, PROTOCOL_VERSION, ToolMetaView, UsageRole,
};
pub use provider_setup::{
    ProviderFactory, ProviderSetupError, known_provider, provider_for_with, provider_label,
};
pub use reasoning::ReasoningLevel;
pub use receipt::{ReceiptBuilder, Savings};
pub use record::SessionRecord;
pub use signals::{
    ServerHold, Signal, SignalPolicy, SignalRouter, handle_signals, hold_signal_router,
    install_signal_handlers, install_termination_handlers,
};
pub use timetravel::{FileState, MaterializeReport, TtError};
pub use tree::TreeNode;
pub use workspace::{IndexLockError, Workspace, Workspaces};
// Types that appear in `Event` and `Command`, re-exported so clients need no other crate.
pub use tautly_context::{Breakdown, MutationReason};
pub use tautly_mcp::{McpConfig, ServerSource, ServerSpec};
pub use tautly_memory::{MemAnchor, MemoryChange, MemoryEntry};
pub use tautly_provider::{ModelId, ProviderEvent, Quota, RawUsage};
// What a client needs to fill `CliOverrides` and to find the data directory.
pub use tautly_sandbox::Mode;
pub use tautly_session::paths::data_home;
pub use tautly_tools::{DiffPreview, SaveKind, Saved};
// What the CLI's utility commands (`tautly prompt|sessions|models|mcp ...`) read, so that
// `tautly-cli` depends on this crate alone.
pub use tautly_context::prompt::system as system_prompt;
pub use tautly_mcp::config::{import_into_toml, parse_mcp_json};
pub use tautly_mcp::names::is_valid_server_name;
pub use tautly_mcp::{McpHub, ServerStatus, printable, printable_multiline};
pub use tautly_provider::{Catalog, ModelInfo, Price};
pub use tautly_session::{
    Error as LogError, Origin, SessionReader, SessionSummary as LogSummary,
    list_here_in as list_logs_here, list_in as list_logs,
};
