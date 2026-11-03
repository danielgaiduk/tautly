//! `pin` and `state`: context-management tools. They change nothing themselves; the
//! engine intercepts the parsed arguments as a [`ContextOp`] and applies them to the
//! context. `run` only validates and acknowledges.

use async_trait::async_trait;
use hx_provider::{Capabilities, ToolSpec};
use hx_sandbox::{Effect, Sandbox};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::spec::{parse_args, tool_spec};
use crate::{Tool, ToolCx, ToolError, ToolResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Pending,
    InProgress,
    Done,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanStep {
    pub text: String,
    pub status: StepStatus,
}

/// An update to the structured task state. Absent fields change nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatePatch {
    /// Replaces the whole plan when present.
    #[serde(default)]
    pub plan: Option<Vec<PlanStep>>,
    #[serde(default)]
    pub decisions_add: Vec<String>,
    #[serde(default)]
    pub questions_add: Vec<String>,
    /// 0-based indices of open questions to mark resolved, in the order the state
    /// summary lists them.
    #[serde(default)]
    pub questions_resolve: Vec<usize>,
}

/// What the engine does with a `pin` or `state` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextOp {
    Pin(String),
    StateUpdate(StatePatch),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PinArgs {
    text: String,
}

fn parse_pin(args: &Value) -> Result<String, ToolError> {
    let a: PinArgs = parse_args(args)?;
    if a.text.trim().is_empty() {
        return Err(ToolError::BadArgs("text must not be empty".into()));
    }
    Ok(a.text)
}

/// The context operation `tool` called with `args` stands for. None if `tool` is neither
/// `pin` nor `state`, or the arguments are invalid (the tool's own `run` reports why).
pub fn parse_context_op(tool: &str, args: &Value) -> Option<ContextOp> {
    match tool {
        "pin" => parse_pin(args).ok().map(ContextOp::Pin),
        "state" => parse_args(args).ok().map(ContextOp::StateUpdate),
        _ => None,
    }
}

pub struct PinTool;
pub struct StateTool;

#[async_trait]
impl Tool for PinTool {
    fn name(&self) -> &'static str {
        "pin"
    }

    fn spec(&self, _caps: &Capabilities) -> ToolSpec {
        tool_spec(
            "pin",
            "Pin a constraint or fact that must stay in context for the rest of the session; \
             pinned text is never summarized or dropped. Keep it short and use it sparingly.",
            json!({
                "type": "object",
                "properties": {
                    "text": {"type": "string", "description": "The text to pin."}
                },
                "required": ["text"],
                "additionalProperties": false
            }),
        )
    }

    fn effect(&self, args: &Value) -> Result<Effect, ToolError> {
        parse_pin(args).map(|_| Effect::ReadOnly)
    }

    async fn run(&self, args: Value, _sb: &dyn Sandbox, _cx: &ToolCx) -> ToolResult {
        match parse_pin(&args) {
            Ok(text) => ToolResult::ok(json!({"ok": true, "pinned": text}).to_string()),
            Err(e) => ToolResult::error(e.to_string()),
        }
    }
}

#[async_trait]
impl Tool for StateTool {
    fn name(&self) -> &'static str {
        "state"
    }

    fn spec(&self, _caps: &Capabilities) -> ToolSpec {
        tool_spec(
            "state",
            "Update the task state that survives context compaction: the plan (replaced \
             whole when given), decisions made, and open questions. Only fields you pass change.",
            json!({
                "type": "object",
                "properties": {
                    "plan": {
                        "type": "array",
                        "description": "The full plan, replacing the previous one.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "text": {"type": "string"},
                                "status": {"type": "string", "enum": ["pending", "in_progress", "done"]}
                            },
                            "required": ["text", "status"],
                            "additionalProperties": false
                        }
                    },
                    "decisions_add": {"type": "array", "items": {"type": "string"}, "description": "Decisions to record."},
                    "questions_add": {"type": "array", "items": {"type": "string"}, "description": "Open questions to record."},
                    "questions_resolve": {"type": "array", "items": {"type": "integer", "minimum": 0}, "description": "0-based indices of open questions to mark resolved, in the order the state summary lists them."}
                },
                "additionalProperties": false
            }),
        )
    }

    fn effect(&self, args: &Value) -> Result<Effect, ToolError> {
        parse_args::<StatePatch>(args).map(|_| Effect::ReadOnly)
    }

    async fn run(&self, args: Value, _sb: &dyn Sandbox, _cx: &ToolCx) -> ToolResult {
        match parse_args::<StatePatch>(&args) {
            Ok(p) => ToolResult::ok(
                json!({
                    "ok": true,
                    "plan_steps": p.plan.as_ref().map(Vec::len),
                    "decisions_added": p.decisions_add.len(),
                    "questions_added": p.questions_add.len(),
                    "questions_resolved": p.questions_resolve.len(),
                })
                .to_string(),
            ),
            Err(e) => ToolResult::error(e.to_string()),
        }
    }
}
