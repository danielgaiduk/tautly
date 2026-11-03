//! A tools-only Model Context Protocol client (spec 7.11.10), built on `rmcp`.
//!
//! [`McpHub`] starts a session's servers (stdio through the sandbox, or streamable HTTP)
//! in the background and offers each ready server's tools as Tautly tools named
//! `mcp__<server>__<tool>` ([`names`]), descriptions marked `[MCP server <name>]`.
//! Resources, prompts, sampling, elicitation, roots and OAuth are not implemented; a
//! server's requests for them are refused. Text from servers and repositories is escaped
//! before it is shown ([`printable`]) and stripped of control characters before the model
//! reads it.
//!
//! Which servers to start is the engine's decision: repository-declared ones need an
//! approval, remembered by [`approvals`]. The test server lives in the
//! `tautly-mcp-fixture` crate.

pub mod approvals;
mod client;
pub mod config;
pub mod names;
mod text;
mod tool;

pub use approvals::{APPROVALS_FILE, ApprovalStore};
pub use client::{MAX_TOTAL_TOOLS, McpHub, McpState, ServerStatus};
pub use config::{McpConfig, PROVIDER_KEY_VARS, ServerSource, ServerSpec};
pub use text::printable;
pub use tool::{DESCRIPTION_CAP, RESULT_CAP, SCHEMA_CAP};
