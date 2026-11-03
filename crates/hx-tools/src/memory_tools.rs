//! `recall` and `remember`: search project memory and propose new entries. `remember`
//! writes nothing itself; the engine intercepts the call as a [`ContextOp::Remember`]
//! (see `parse_context_op`) and asks the user before storing the entry.
//!
//! [`ContextOp::Remember`]: crate::ContextOp::Remember

use std::path::PathBuf;

use async_trait::async_trait;
use hx_memory::{Hit, HitSource, MemoryDraft, MemoryKind};
use hx_provider::{Capabilities, ToolSpec};
use hx_sandbox::{Effect, Sandbox};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::spec::{parse_args, tool_spec};
use crate::{Tool, ToolCx, ToolError, ToolResult};

/// Results `recall` returns at most.
const RECALL_TOP_K: usize = 5;
/// The whole `recall` output stays within this many tokens (spec 7.4)...
const RECALL_MAX_TOKENS: u32 = 2000;
/// ...so the hits' own text gets this much; the rest is room for the numbering and flags.
const RECALL_TEXT_TOKENS: u32 = RECALL_MAX_TOKENS - 200;

const PROPOSED: &str = "proposed; awaiting user approval";

/// About four bytes per token, the same rule as the context estimator before calibration.
fn est(s: &str) -> u32 {
    s.len().div_ceil(4) as u32
}

pub struct RecallTool;
pub struct RememberTool;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RecallArgs {
    query: String,
}

fn parse_recall(args: &Value) -> Result<String, ToolError> {
    let a: RecallArgs = parse_args(args)?;
    if a.query.trim().is_empty() {
        return Err(ToolError::BadArgs("query must not be empty".into()));
    }
    Ok(a.query)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AnchorArg {
    path: PathBuf,
    #[serde(default)]
    symbol: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RememberArgs {
    #[serde(default)]
    id: Option<String>,
    title: String,
    kind: MemoryKind,
    body: String,
    #[serde(default)]
    anchors: Vec<AnchorArg>,
}

/// The draft `remember` was called with; the same parse backs `run`, `effect` and
/// `parse_context_op`.
pub(crate) fn parse_remember(args: &Value) -> Result<MemoryDraft, ToolError> {
    let a: RememberArgs = parse_args(args)?;
    if a.title.trim().is_empty() {
        return Err(ToolError::BadArgs("title must not be empty".into()));
    }
    if a.body.trim().is_empty() {
        return Err(ToolError::BadArgs("body must not be empty".into()));
    }
    Ok(MemoryDraft {
        id: a.id,
        title: a.title,
        kind: a.kind,
        anchors: a.anchors.into_iter().map(|x| (x.path, x.symbol)).collect(),
        body: a.body,
    })
}

#[async_trait]
impl Tool for RecallTool {
    fn name(&self) -> &'static str {
        "recall"
    }

    fn spec(&self, _caps: &Capabilities) -> ToolSpec {
        tool_spec(
            "recall",
            "Search the project's memory (saved conventions, facts, gotchas, commands) and \
             notes from earlier sessions. Returns the best 5 matches. A match marked STALE \
             is about code that has changed since it was written: check it before relying on it.",
            json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Words to search for."}
                },
                "required": ["query"],
                "additionalProperties": false
            }),
        )
    }

    fn effect(&self, args: &Value) -> Result<Effect, ToolError> {
        parse_recall(args).map(|_| Effect::ReadOnly)
    }

    async fn run(&self, args: Value, _sb: &dyn Sandbox, cx: &ToolCx) -> ToolResult {
        let query = match parse_recall(&args) {
            Ok(q) => q,
            Err(e) => return ToolResult::error(e.to_string()),
        };
        let Some(memory) = cx.memory().cloned() else {
            return ToolResult::error("memory is not available in this session");
        };
        // Index queries and the first read of the episode files block: keep them off the
        // async threads. Staleness is re-checked so edits made this session show.
        let hits = tokio::task::spawn_blocking(move || {
            let mut store = memory.write().unwrap_or_else(|e| e.into_inner());
            store.refresh_staleness();
            store.search(&query, RECALL_TOP_K, RECALL_TEXT_TOKENS, &est)
        })
        .await;
        match hits {
            Ok(hits) => ToolResult::ok(format_hits(&hits)),
            Err(e) => ToolResult::error(format!("memory search failed: {e}")),
        }
    }
}

fn format_hits(hits: &[Hit]) -> String {
    if hits.is_empty() {
        return "no matching memory".into();
    }
    let mut sections = Vec::new();
    for (i, h) in hits.iter().enumerate() {
        let origin = match &h.source {
            HitSource::Project(id) => format!("id: {id}"),
            HitSource::Episode(session) => format!("earlier session {session}"),
        };
        let flag = if h.stale {
            " [STALE: the code it is anchored to has changed since; verify before relying on it]"
        } else {
            ""
        };
        sections.push(format!(
            "{}. {} ({origin}){flag}\n{}",
            i + 1,
            h.title,
            h.text
        ));
    }
    sections.join("\n\n")
}

#[async_trait]
impl Tool for RememberTool {
    fn name(&self) -> &'static str {
        "remember"
    }

    fn spec(&self, _caps: &Capabilities) -> ToolSpec {
        tool_spec(
            "remember",
            "Propose a note for the project's memory, to be shown to the user for approval. \
             Use it when the user corrects you or states a convention, or when you learn a \
             lasting fact, gotcha or command. Keep the body short (under 300 tokens) and \
             anchor it to the code it is about, so it can be flagged when that code changes. \
             Pass `id` to update an existing entry.",
            json!({
                "type": "object",
                "properties": {
                    "title": {"type": "string", "description": "One-line summary."},
                    "kind": {"type": "string", "enum": ["convention", "fact", "gotcha", "command"]},
                    "body": {"type": "string", "description": "The note, under 300 tokens."},
                    "anchors": {
                        "type": "array",
                        "description": "Code the note is about.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "path": {"type": "string", "description": "File path relative to the project root."},
                                "symbol": {"type": "string", "description": "A definition in the file, such as `parse` or `Config::load`; omit to anchor to the whole file."}
                            },
                            "required": ["path"],
                            "additionalProperties": false
                        }
                    },
                    "id": {"type": "string", "description": "Id of the entry to update, as shown by `recall`."}
                },
                "required": ["title", "kind", "body"],
                "additionalProperties": false
            }),
        )
    }

    fn effect(&self, args: &Value) -> Result<Effect, ToolError> {
        parse_remember(args).map(|_| Effect::ReadOnly)
    }

    async fn run(&self, args: Value, _sb: &dyn Sandbox, cx: &ToolCx) -> ToolResult {
        let draft = match parse_remember(&args) {
            Ok(d) => d,
            Err(e) => return ToolResult::error(e.to_string()),
        };
        // Catch what approval would fail on (too long, unknown anchor or id) now, while the
        // model can still fix it. The hashes are filled in again when the user approves.
        if let Some(memory) = cx.memory() {
            let store = memory.read().unwrap_or_else(|e| e.into_inner());
            if let Err(e) = store.draft_to_change(draft, &est) {
                return ToolResult::error_kind(e.to_string(), "invalid_memory");
            }
        }
        ToolResult::ok(PROPOSED)
    }
}
