//! Helpers shared by tool implementations.

use std::path::Path;
use std::sync::Arc;

use hx_index::Index;
use hx_provider::ToolSpec;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::{ToolCx, ToolError};

pub(crate) fn parse_args<T: DeserializeOwned>(args: &Value) -> Result<T, ToolError> {
    serde_json::from_value(args.clone()).map_err(|e| ToolError::BadArgs(e.to_string()))
}

pub(crate) fn tool_spec(name: &str, description: &str, schema: Value) -> ToolSpec {
    ToolSpec {
        name: name.to_string(),
        description: description.to_string(),
        schema,
    }
}

/// Decode file bytes as text: a NUL in the first 8 KB means binary.
pub(crate) fn decode_text(bytes: &[u8]) -> Result<&str, &'static str> {
    if bytes.iter().take(8192).any(|&b| b == 0) {
        return Err("binary file");
    }
    std::str::from_utf8(bytes).map_err(|_| "file is not UTF-8")
}

/// All regular files under `base`, honoring ignore files, sorted. Walking starts from
/// the sandbox walker so confinement applies; unreadable entries and loops are skipped.
pub(crate) async fn walk_files(
    sb: &dyn hx_sandbox::Sandbox,
    base: &std::path::Path,
) -> Result<Vec<std::path::PathBuf>, String> {
    let mut builder = sb.walker(base).map_err(|e| e.to_string())?;
    builder.require_git(false).follow_links(true);
    let mut files = tokio::task::spawn_blocking(move || {
        builder
            .build()
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
            .map(|e| e.into_path())
            .collect::<Vec<_>>()
    })
    .await
    .map_err(|e| e.to_string())?;
    files.sort();
    Ok(files)
}

pub(crate) fn display_rel(sb: &dyn hx_sandbox::Sandbox, p: &std::path::Path) -> String {
    p.strip_prefix(sb.root()).unwrap_or(p).display().to_string()
}

/// The index when its tree and trigram candidates can be trusted: it has completed a
/// refresh, and nothing (a shell command) may have changed files since without a refresh or
/// watcher batch having caught up.
pub(crate) fn live_index(cx: &ToolCx) -> Option<&Arc<Index>> {
    cx.index().filter(|i| i.is_ready() && !i.is_stale())
}

/// Whether the tree can answer for the search directory or file `base` (absolute, as the
/// sandbox resolved it). The tree omits hidden and ignored paths, which the walker does
/// visit when asked for them by name, so a base the tree does not know goes to the walker.
pub(crate) fn tree_covers(index: &Index, base: &Path) -> bool {
    index.tree().contains(base)
}
