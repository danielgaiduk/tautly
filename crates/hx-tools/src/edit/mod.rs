//! The `edit` tool: stale-read guard, tolerant matching, atomic write. One tool, three
//! argument formats (`str_replace`, `patch`, `hashline`) chosen by the model's capabilities.

pub mod atomic;
pub mod guard;
pub mod hashline;
pub mod patch;
pub mod str_replace;
pub mod tolerant;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use hx_provider::{Capabilities, EditFormat, ToolSpec};
use hx_sandbox::{Effect, FileMeta, Sandbox, SandboxError};
use serde::Deserialize;
use serde_json::{Value, json};

use self::hashline::{Anchor, HashlineError, HashlineOp};
use self::patch::{Kind, PatchOp};
use self::str_replace::MatchError;
use self::tolerant::RepairStage;
use crate::spec::{parse_args, tool_spec};
use crate::{AppliedEffect, Tool, ToolCx, ToolError, ToolResult};

/// Why an edit could not be applied.
#[derive(Debug, thiserror::Error)]
pub enum EditError {
    #[error("read the file first (use `read`) before editing it")]
    NotRead,
    #[error("file changed since last read; read it again before editing")]
    Stale,
    #[error("{0}")]
    Undecodable(&'static str),
    #[error(transparent)]
    Sandbox(#[from] SandboxError),
}

pub struct EditTool;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StrReplaceArgs {
    path: String,
    old_text: String,
    new_text: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PatchArgs {
    patch: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HashlineArgs {
    path: String,
    ops: Vec<OpArgs>,
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum OpArgs {
    Replace {
        start: String,
        end: String,
        lines: Vec<String>,
    },
    InsertAfter {
        at: String,
        lines: Vec<String>,
    },
    Delete {
        start: String,
        end: String,
    },
}

impl OpArgs {
    fn into_op(self) -> Result<HashlineOp, HashlineError> {
        Ok(match self {
            OpArgs::Replace { start, end, lines } => HashlineOp::Replace {
                start: Anchor::parse(&start)?,
                end: Anchor::parse(&end)?,
                lines,
            },
            OpArgs::InsertAfter { at, lines } => HashlineOp::InsertAfter {
                at: Anchor::parse(&at)?,
                lines,
            },
            OpArgs::Delete { start, end } => HashlineOp::Delete {
                start: Anchor::parse(&start)?,
                end: Anchor::parse(&end)?,
            },
        })
    }
}

fn str_replace_spec() -> ToolSpec {
    tool_spec(
        "edit",
        "Replace one occurrence of old_text with new_text in a file you have read. \
         old_text must identify a single place; include surrounding lines to make it unique.",
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File path."},
                "old_text": {"type": "string", "description": "Text to replace; must match exactly one place."},
                "new_text": {"type": "string", "description": "Replacement text."}
            },
            "required": ["path", "old_text", "new_text"],
            "additionalProperties": false
        }),
    )
}

fn patch_spec() -> ToolSpec {
    tool_spec(
        "edit",
        "Edit files with a patch. Format: `*** Begin Patch`, then per file `*** Update File: <path>` \
         (hunks start with `@@`; lines start with ' ' for context, '-' to remove, '+' to add), \
         `*** Add File: <path>` (every line starts with '+') or `*** Delete File: <path>`, then \
         `*** End Patch`. A plain unified diff also works. Context and '-' lines must identify a \
         single place; files must have been read first. All files change or none do. Example:\n\
         *** Begin Patch\n*** Update File: src/a.rs\n@@ fn main() {\n fn main() {\n-    old();\n+    new();\n }\n*** End Patch",
        json!({
            "type": "object",
            "properties": {
                "patch": {"type": "string", "description": "The patch text."}
            },
            "required": ["patch"],
            "additionalProperties": false
        }),
    )
}

fn hashline_spec() -> ToolSpec {
    let anchor = json!({"type": "string", "description": "Line anchor `<n>:<hash>` exactly as `read` printed it before the `|`."});
    let lines = json!({"type": "array", "items": {"type": "string"}, "description": "New lines, without line numbers or anchors."});
    tool_spec(
        "edit",
        "Edit a file by line anchors from `read` (`<n>:<hash>|<line>`). ops: `replace` start..end \
         (inclusive) with lines, `insert_after` the anchored line, or `delete` start..end. All anchors \
         refer to the file as you last read it, and ops must not overlap. If a line changed, its anchor \
         is rejected; read again. Example: {\"path\":\"a.rs\",\"ops\":[{\"op\":\"replace\",\"start\":\"3:a1b2\",\"end\":\"3:a1b2\",\"lines\":[\"let x = 1;\"]}]}",
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File path."},
                "ops": {
                    "type": "array",
                    "minItems": 1,
                    "items": {
                        "oneOf": [
                            {
                                "type": "object",
                                "properties": {
                                    "op": {"const": "replace"},
                                    "start": anchor, "end": anchor, "lines": lines
                                },
                                "required": ["op", "start", "end", "lines"],
                                "additionalProperties": false
                            },
                            {
                                "type": "object",
                                "properties": {
                                    "op": {"const": "insert_after"},
                                    "at": anchor, "lines": lines
                                },
                                "required": ["op", "at", "lines"],
                                "additionalProperties": false
                            },
                            {
                                "type": "object",
                                "properties": {
                                    "op": {"const": "delete"},
                                    "start": anchor, "end": anchor
                                },
                                "required": ["op", "start", "end"],
                                "additionalProperties": false
                            }
                        ]
                    }
                }
            },
            "required": ["path", "ops"],
            "additionalProperties": false
        }),
    )
}

/// A file that passed the stale-read guard.
struct Loaded {
    full: PathBuf,
    meta: FileMeta,
    original: Vec<u8>,
    /// BOM-less LF text; empty when decoding was not requested.
    text: String,
}

/// Resolve `path`, read it, and apply the stale-read guard. Errors are ready to show.
async fn load_fresh(
    sb: &dyn Sandbox,
    cx: &ToolCx,
    path: &str,
    decode: bool,
) -> Result<Loaded, String> {
    let full = sb.resolve(Path::new(path)).map_err(|e| e.to_string())?;
    let (meta, original) = match (sb.metadata(&full).await, sb.read(&full).await) {
        (Ok(m), Ok(b)) => (m, b),
        (Err(e), _) | (_, Err(e)) => return Err(format!("{path}: {e}")),
    };
    // Binary files can never be `read`, so say why before asking for a read.
    let text = if decode {
        atomic::to_lf_text(&original).map_err(|e| format!("{path}: {e}"))?
    } else {
        String::new()
    };
    guard::check_fresh(cx.reads(), &full, &meta, &original).map_err(|e| format!("{path}: {e}"))?;
    Ok(Loaded {
        full,
        meta,
        original,
        text,
    })
}

/// Remember what was written, so consecutive edits need no re-read.
async fn record_written(sb: &dyn Sandbox, cx: &ToolCx, full: &Path, bytes: &[u8]) -> blake3::Hash {
    let hash = blake3::hash(bytes);
    if let Ok(meta) = sb.metadata(full).await {
        cx.reads().record(full, &meta, hash);
    }
    hash
}

/// Write `edited` (LF text) over a loaded file, keeping its encoding, and report the effect.
async fn write_edited(
    sb: &dyn Sandbox,
    cx: &ToolCx,
    path: &str,
    file: Loaded,
    edited: &str,
) -> Result<AppliedEffect, String> {
    atomic::preserve_and_write(sb, &file.full, &file.original, edited)
        .await
        .map_err(|e| format!("{path}: {e}"))?;
    let post_hash =
        record_written(sb, cx, &file.full, &atomic::restore(&file.original, edited)).await;
    Ok(AppliedEffect::Edited {
        path: file.full,
        pre_image: Some(file.original),
        post_hash,
    })
}

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &'static str {
        "edit"
    }

    fn spec(&self, caps: &Capabilities) -> ToolSpec {
        match caps.edit_format {
            EditFormat::StrReplace => str_replace_spec(),
            EditFormat::Patch => patch_spec(),
            EditFormat::Hashline => hashline_spec(),
        }
    }

    /// The call's format is recognised by its arguments (the active format is not known
    /// here); every path any format would touch is listed.
    fn effect(&self, args: &Value) -> Result<Effect, ToolError> {
        let paths = if args.get("patch").is_some() {
            let a: PatchArgs = parse_args(args)?;
            patch::touched(&a.patch)
                .map_err(|e| ToolError::BadArgs(e.to_string()))?
                .into_iter()
                .map(|t| t.path)
                .collect()
        } else if args.get("ops").is_some() {
            let a: HashlineArgs = parse_args(args)?;
            vec![PathBuf::from(a.path)]
        } else {
            let a: StrReplaceArgs = parse_args(args)?;
            vec![PathBuf::from(a.path)]
        };
        Ok(Effect::Write { paths })
    }

    async fn run(&self, args: Value, sb: &dyn Sandbox, cx: &ToolCx) -> ToolResult {
        match cx.edit_format() {
            EditFormat::StrReplace => match parse_args(&args) {
                Ok(a) => run_str_replace(a, sb, cx).await,
                Err(e) => ToolResult::error(e.to_string()),
            },
            EditFormat::Patch => match parse_args(&args) {
                Ok(a) => run_patch(a, sb, cx).await,
                Err(e) => ToolResult::error(e.to_string()),
            },
            EditFormat::Hashline => match parse_args(&args) {
                Ok(a) => run_hashline(a, sb, cx).await,
                Err(e) => ToolResult::error(e.to_string()),
            },
        }
    }
}

async fn run_str_replace(a: StrReplaceArgs, sb: &dyn Sandbox, cx: &ToolCx) -> ToolResult {
    if a.old_text.is_empty() {
        return ToolResult::error("old_text must not be empty");
    }
    if a.old_text == a.new_text {
        return ToolResult::error("old_text and new_text are identical; nothing to change");
    }
    let file = match load_fresh(sb, cx, &a.path, true).await {
        Ok(f) => f,
        Err(e) => return ToolResult::error(e),
    };
    let (edited, stage) = match str_replace::apply_staged(&file.text, &a.old_text, &a.new_text) {
        Ok(r) => r,
        Err(e) => return ToolResult::error(match_error_message(&a.path, "old_text", &e)),
    };
    let effect = match write_edited(sb, cx, &a.path, file, &edited).await {
        Ok(e) => e,
        Err(e) => return ToolResult::error(e),
    };
    let mut output = format!("edited {}", a.path);
    if stage != RepairStage::Exact {
        output.push_str(&format!("; applied via {}", stage.describe()));
    }
    let mut r = ToolResult::ok(output);
    r.effects.push(effect);
    r
}

async fn run_hashline(a: HashlineArgs, sb: &dyn Sandbox, cx: &ToolCx) -> ToolResult {
    if a.ops.is_empty() {
        return ToolResult::error("ops must not be empty");
    }
    let count = a.ops.len();
    let ops: Vec<HashlineOp> = match a.ops.into_iter().map(OpArgs::into_op).collect() {
        Ok(ops) => ops,
        Err(e) => return ToolResult::error(format!("{}: {e}", a.path)),
    };
    let file = match load_fresh(sb, cx, &a.path, true).await {
        Ok(f) => f,
        Err(e) => return ToolResult::error(e),
    };
    let edited = match hashline::apply(&file.text, &ops) {
        Ok(t) => t,
        Err(e @ (HashlineError::Stale { .. } | HashlineError::OutOfRange { .. })) => {
            return ToolResult::error(format!(
                "{}: {e}; read the file again to get fresh anchors",
                a.path
            ));
        }
        Err(e) => return ToolResult::error(format!("{}: {e}", a.path)),
    };
    match write_edited(sb, cx, &a.path, file, &edited).await {
        Ok(effect) => {
            let mut r = ToolResult::ok(format!("edited {} ({count} ops)", a.path));
            r.effects.push(effect);
            r
        }
        Err(e) => ToolResult::error(e),
    }
}

/// One file of a validated patch, ready to write.
struct Step {
    rel: String,
    full: PathBuf,
    op: PatchOp,
    /// The file as it was (None for an added file).
    original: Option<Vec<u8>>,
    mode: Option<u32>,
    /// What to write for an update or add.
    bytes: Vec<u8>,
}

impl Step {
    async fn apply(&self, sb: &dyn Sandbox) -> Result<(), SandboxError> {
        match self.op {
            PatchOp::Delete => sb.remove_file(&self.full).await,
            PatchOp::Update(_) | PatchOp::Add(_) => {
                sb.write_atomic(&self.full, &self.bytes, None).await
            }
        }
    }

    /// Put the file back as it was before this step.
    async fn undo(&self, sb: &dyn Sandbox) -> Result<(), SandboxError> {
        match &self.original {
            Some(original) => {
                let mode = self.mode.map(|m| m & 0o7777);
                sb.write_atomic(&self.full, original, mode).await
            }
            None => sb.remove_file(&self.full).await,
        }
    }
}

async fn run_patch(a: PatchArgs, sb: &dyn Sandbox, cx: &ToolCx) -> ToolResult {
    let touched = match patch::touched(&a.patch) {
        Ok(t) => t,
        Err(e) => return ToolResult::error(e.to_string()),
    };
    // Load and guard every file before looking at any hunk. `texts` holds the content the
    // pure applier sees; added files only need to be known to exist (or not).
    let mut texts: HashMap<PathBuf, String> = HashMap::new();
    let mut loaded: Vec<(PathBuf, Option<Loaded>)> = Vec::new();
    for t in &touched {
        let rel = t.path.to_string_lossy().into_owned();
        let file = match t.kind {
            Kind::Add => {
                let full = match sb.resolve(&t.path) {
                    Ok(p) => p,
                    Err(e) => return ToolResult::error(e.to_string()),
                };
                match sb.metadata(&full).await {
                    Ok(_) => {
                        texts.insert(t.path.clone(), String::new());
                    }
                    Err(SandboxError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return ToolResult::error(format!("{rel}: {e}")),
                }
                loaded.push((full, None));
                continue;
            }
            Kind::Update => load_fresh(sb, cx, &rel, true).await,
            Kind::Delete => load_fresh(sb, cx, &rel, false).await,
        };
        match file {
            Ok(f) => {
                texts.insert(t.path.clone(), f.text.clone());
                loaded.push((f.full.clone(), Some(f)));
            }
            Err(e) => return ToolResult::error(e),
        }
    }
    // Two spellings of one path would otherwise be written twice.
    if let Some((i, _)) = loaded
        .iter()
        .enumerate()
        .find(|(i, (p, _))| loaded[..*i].iter().any(|(q, _)| q == p))
    {
        return ToolResult::error(format!(
            "{}: appears more than once in the patch",
            touched[i].path.display()
        ));
    }

    let applied = match patch::apply_staged(&|p| texts.get(p).cloned(), &a.patch) {
        Ok(v) => v,
        Err(e) => return ToolResult::error(e.to_string()),
    };

    let mut steps: Vec<Step> = Vec::with_capacity(applied.len());
    let mut notes: Vec<String> = Vec::new();
    for (ap, (full, file)) in applied.iter().zip(loaded) {
        let rel = ap.path.to_string_lossy().into_owned();
        let (original, mode) = match file {
            Some(f) => (Some(f.original), Some(f.meta.mode)),
            None => (None, None),
        };
        let bytes = match (&ap.op, &original) {
            (PatchOp::Update(text), Some(orig)) => atomic::restore(orig, text),
            (PatchOp::Add(text), _) => text.clone().into_bytes(),
            _ => Vec::new(),
        };
        for (hunk, stage) in &ap.repairs {
            notes.push(format!(
                "{rel}: hunk {hunk} applied via {}",
                stage.describe()
            ));
        }
        steps.push(Step {
            rel,
            full,
            op: ap.op.clone(),
            original,
            mode,
            bytes,
        });
    }

    // All hunks are validated; now write. A failure undoes the files already written.
    for (i, step) in steps.iter().enumerate() {
        let Err(e) = step.apply(sb).await else {
            continue;
        };
        let mut failures = Vec::new();
        for done in steps[..i].iter().rev() {
            if let Err(undo) = done.undo(sb).await {
                failures.push(format!("{}: {undo}", done.rel));
            }
        }
        let mut msg = format!("{}: {e}", step.rel);
        if i == 0 {
            msg.push_str("; no files were changed");
        } else if failures.is_empty() {
            msg.push_str(&format!("; rolled back {i} earlier change(s)"));
        } else {
            msg.push_str(&format!(
                "; rollback incomplete, check these files: {}",
                failures.join("; ")
            ));
        }
        return ToolResult::error(msg);
    }

    let mut lines = Vec::with_capacity(steps.len() + notes.len());
    let mut effects = Vec::with_capacity(steps.len());
    for step in steps {
        let (verb, post_hash) = match step.op {
            PatchOp::Delete => ("deleted", blake3::hash(b"")),
            PatchOp::Update(_) | PatchOp::Add(_) => {
                let h = record_written(sb, cx, &step.full, &step.bytes).await;
                let verb = if matches!(step.op, PatchOp::Add(_)) {
                    "added"
                } else {
                    "updated"
                };
                (verb, h)
            }
        };
        lines.push(format!("{verb} {}", step.rel));
        effects.push(AppliedEffect::Edited {
            path: step.full,
            pre_image: step.original,
            post_hash,
        });
    }
    lines.extend(notes);
    let mut r = ToolResult::ok(lines.join("\n"));
    r.effects = effects;
    r
}

/// Describe a failed match. `subject` prefixes the message (a path, or path and hunk) and
/// `what` names the text that was being matched.
pub(crate) fn match_error_message(subject: &str, what: &str, e: &MatchError) -> String {
    match e {
        MatchError::Ambiguous { lines } => {
            let lines: Vec<String> = lines.iter().map(usize::to_string).collect();
            format!(
                "{subject}: {what} matches {} places (starting at lines {}); \
                 include more surrounding lines to make it unique",
                lines.len(),
                lines.join(", ")
            )
        }
        MatchError::NotFound {
            candidates,
            nearest,
        } => {
            let mut msg = format!("{subject}: {what} not found");
            if !candidates.is_empty() {
                let lines: Vec<String> = candidates.iter().map(usize::to_string).collect();
                msg.push_str(&format!("; similar text near lines {}", lines.join(", ")));
            }
            if let Some((start, window)) = nearest {
                msg.push_str(&format!(
                    "\nnearest text (from line {start}):\n{window}\nRetry with {what} copied from these lines."
                ));
            }
            msg
        }
    }
}
