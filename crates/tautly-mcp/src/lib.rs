//! A tools-only Model Context Protocol client (spec 7.11.10), built on `rmcp`.
//!
//! [`McpHub`] starts a session's servers (stdio through the sandbox, or streamable HTTP)
//! in the background and offers each ready server's tools as [`McpTool`]s named
//! `mcp__<server>__<tool>` ([`names`]). Resources, prompts, sampling, elicitation, roots
//! and OAuth are not implemented; a server's requests for them are refused.
//!
//! Which servers to start is the engine's decision: repository-declared ones need an
//! approval, remembered by [`approvals`].

pub mod approvals;
mod client;
pub mod config;
pub mod names;
mod tool;

pub use approvals::{APPROVALS_FILE, ApprovalStore};
pub use client::{McpHub, McpState, ServerStatus};
pub use config::{McpConfig, PROVIDER_KEY_VARS, ServerSource, ServerSpec};
pub use tool::{DESCRIPTION_CAP, RESULT_CAP, SCHEMA_CAP};
