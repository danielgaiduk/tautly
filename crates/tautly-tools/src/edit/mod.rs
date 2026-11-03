//! The `edit` tool: stale-read guard, tolerant matching, atomic write. One tool, three
//! argument formats (`str_replace`, `patch`, `hashline`) chosen by the model's capabilities.

pub mod atomic;
pub mod guard;
pub mod hashline;
pub mod patch;
pub mod str_replace;
pub mod syntax;
pub mod tolerant;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tautly_provider::{Capabilities, EditFormat, ToolSpec};
use tautly_sandbox::{Effect, FileMeta, Sandbox, SandboxError};

use self::hashline::{Anchor, HashlineError, HashlineOp};
use self::patch::{Kind, PatchError, PatchOp};
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

/// One hashline op as the model writes it. Kept flat (no tagged union) so the JSON
/// schema stays simple for models that handle `oneOf` poorly; [`OpArgs::into_op`]
/// checks which fields the chosen op needs.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OpArgs {
    op: String,
    start: Option<String>,
    end: Option<String>,
    at: Option<String>,
    lines: Option<Vec<String>>,
}

impl OpArgs {
    /// `n` is the op's 1-based position, for messages.
    fn into_op(self, n: usize) -> Result<HashlineOp, String> {
        let name = self.op.clone();
        let missing = |field: &str| format!("op {n} ({name}): missing `{field}`");
        let anchor = |a: Option<String>, field: &str| -> Result<Anchor, String> {
            let a = a.ok_or_else(|| missing(field))?;
            Anchor::parse(&a).map_err(|e| format!("op {n} ({name}): {e}"))
        };
        match self.op.as_str() {
            "replace" => Ok(HashlineOp::Replace {
                start: anchor(self.start, "start")?,
                end: anchor(self.end, "end")?,
                lines: self.lines.ok_or_else(|| missing("lines"))?,
            }),
            "insert_after" => Ok(HashlineOp::InsertAfter {
                at: anchor(self.at, "at")?,
                lines: self.lines.ok_or_else(|| missing("lines"))?,
            }),
            "delete" => Ok(HashlineOp::Delete {
                start: anchor(self.start, "start")?,
                end: anchor(self.end, "end")?,
            }),
            other => Err(format!(
                "op {n}: unknown op `{other}`; use `replace`, `insert_after` or `delete`"
            )),
        }
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
    tool_spec(
        "edit",
        "Edit a file by line anchors from `read` (`<n>:<hash>|<line>`). Each op has an `op` of: \
         `replace` (needs start, end, lines: replaces start..end inclusive), `insert_after` (needs at, \
         lines) or `delete` (needs start, end). All anchors refer to the file as you last read it, and \
         ops must not overlap. If a line changed, its anchor is rejected; read again. Example: \
         {\"path\":\"a.rs\",\"ops\":[{\"op\":\"replace\",\"start\":\"3:a1b2\",\"end\":\"3:a1b2\",\"lines\":[\"let x = 1;\"]}]}",
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File path."},
                "ops": {
                    "type": "array",
                    "minItems": 1,
                    "items": {
                        "type": "object",
                        "properties": {
                            "op": {"type": "string", "enum": ["replace", "insert_after", "delete"]},
                            "start": {"type": "string", "description": "replace/delete: first line anchor `<n>:<hash>` exactly as `read` printed it before the `|`."},
                            "end": {"type": "string", "description": "replace/delete: last line anchor (inclusive); same as start for one line."},
                            "at": {"type": "string", "description": "insert_after: anchor of the line to insert after."},
                            "lines": {"type": "array", "items": {"type": "string"}, "description": "replace/insert_after: new lines, without line numbers or anchors."}
                        },
                        "required": ["op"],
                        "additionalProperties": false
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

/// Stable [`crate::ToolMeta::error_kind`] values of the edit tool.
mod kind {
    pub const BAD_ARGS: &str = "bad_args";
    pub const IO: &str = "io";
    pub const UNDECODABLE: &str = "undecodable";
    pub const NOT_READ: &str = "not_read";
    pub const STALE: &str = "stale";
    pub const NOT_FOUND: &str = "not_found";
    pub const AMBIGUOUS: &str = "ambiguous";
    pub const MIXED_ARTIFACTS: &str = "mixed_artifacts";
    pub const SYNTAX: &str = "syntax";
    pub const STALE_ANCHOR: &str = "stale_anchor";
    pub const INVALID_OPS: &str = "invalid_ops";
    pub const INVALID_PATCH: &str = "invalid_patch";
}

fn match_error_kind(e: &MatchError) -> &'static str {
    match e {
        MatchError::NotFound { .. } => kind::NOT_FOUND,
        MatchError::Ambiguous { .. } => kind::AMBIGUOUS,
        MatchError::MixedArtifacts => kind::MIXED_ARTIFACTS,
    }
}

fn patch_error_kind(e: &PatchError) -> &'static str {
    match e {
        PatchError::Hunk { error, .. } => match_error_kind(error),
        _ => kind::INVALID_PATCH,
    }
}

/// Resolve `path`, read it, and apply the stale-read guard. Errors are ready to return.
async fn load_fresh(
    sb: &dyn Sandbox,
    cx: &ToolCx,
    path: &str,
    decode: bool,
) -> Result<Loaded, ToolResult> {
    let full = sb
        .resolve(Path::new(path))
        .map_err(|e| ToolResult::error_kind(e.to_string(), kind::IO))?;
    let (meta, original) = match (sb.metadata(&full).await, sb.read(&full).await) {
        (Ok(m), Ok(b)) => (m, b),
        (Err(e), _) | (_, Err(e)) => {
            return Err(ToolResult::error_kind(format!("{path}: {e}"), kind::IO));
        }
    };
    // Binary files can never be `read`, so say why before asking for a read.
    let text = if decode {
        atomic::to_lf_text(&original)
            .map_err(|e| ToolResult::error_kind(format!("{path}: {e}"), kind::UNDECODABLE))?
    } else {
        String::new()
    };
    guard::check_fresh(cx.reads(), &full, &meta, &original).map_err(|e| {
        let k = match e {
            EditError::NotRead => kind::NOT_READ,
            _ => kind::STALE,
        };
        ToolResult::error_kind(format!("{path}: {e}"), k)
    })?;
    Ok(Loaded {
        full,
        meta,
        original,
        text,
    })
}

/// Remember what was written, so consecutive edits need no re-read.
async fn record_written(
    sb: &dyn Sandbox,
    cx: &ToolCx,
    full: &Path,
    from: Option<&[u8]>,
    bytes: &[u8],
    repaired: bool,
) -> blake3::Hash {
    if let Ok(meta) = sb.metadata(full).await {
        let by = crate::WrittenBy::Edit { from, repaired };
        crate::record_written_file(sb, cx, full, &meta, bytes, by).await;
    }
    blake3::hash(bytes)
}

/// Whether a match found at `stage` must lie in content the model has seen as it is now
/// (spec 7.9): every stage that can match text differing from the model's `old` text.
fn needs_seen(stage: RepairStage) -> bool {
    !matches!(stage, RepairStage::Exact | RepairStage::Normalized)
}

/// Whether the model has seen `lines` (1-based, inclusive) of `file` as they are now.
fn seen_now(cx: &ToolCx, file: &Loaded, (first, last): (usize, usize)) -> bool {
    let Ok(text) = crate::spec::decode_text(&file.original) else {
        return false;
    };
    let lines: Vec<&str> = text.lines().collect();
    cx.reads().has_seen(
        &file.full,
        blake3::hash(&file.original),
        &lines,
        first,
        last,
    )
}

/// The error for a repaired match over lines the model has not seen as they are now.
fn unseen_repair(subject: &str, stage: RepairStage, (first, last): (usize, usize)) -> String {
    format!(
        "{subject}: matched lines {first}-{last} only via {}, and you have not seen those \
         lines as they are now (they changed since you read them, or you never read them); \
         read lines {first}-{last} again before editing",
        stage.describe()
    )
}

/// Most errors a syntax warning lists.
const MAX_WARNED_ERRORS: usize = 5;

/// New parse errors `after` has that `before` did not, as `line N (message)` items (at
/// most [`MAX_WARNED_ERRORS`], then `N more`). None when there are none or the language
/// has no syntax check.
fn new_error_list(path: &str, before: &str, after: &str) -> Option<String> {
    let errors = syntax::new_errors(Path::new(path), before, after)?;
    if errors.is_empty() {
        return None;
    }
    let mut listed: Vec<String> = errors
        .iter()
        .take(MAX_WARNED_ERRORS)
        .map(|(line, msg)| format!("line {line} ({msg})"))
        .collect();
    if errors.len() > MAX_WARNED_ERRORS {
        listed.push(format!("{} more", errors.len() - MAX_WARNED_ERRORS));
    }
    Some(listed.join(", "))
}

/// The warning to append to an edit's output when `after` has parse errors `before` did
/// not. The edit is already on disk; this only tells the model.
fn syntax_warning(path: &str, before: &str, after: &str) -> Option<String> {
    let listed = new_error_list(path, before, after)?;
    Some(format!(
        "syntax warning: new parse errors in {path} at lines {listed}. The edit was applied; \
         check it and fix the syntax with another edit if it is unintended."
    ))
}

/// Re-index `path` after a successful write so `outline`/`symbol`/`refs` see it at once
/// (spec 7.1). A failure never fails the edit; it comes back as a note for the output.
pub(crate) async fn reindex(cx: &ToolCx, path: &str) -> Option<String> {
    let index = cx.index()?;
    if cx.cancel().is_cancelled() {
        // The turn is ending: leave the catch-up to the background refresh.
        index.mark_stale();
        drop(index.refresh_stale_in_background());
        return Some(format!(
            "index note: {path} was not re-indexed (the turn was cancelled); \
             outline/symbol/refs may be stale for it"
        ));
    }
    match index.reindex_file(Path::new(path)).await {
        Ok(()) => None,
        Err(e) => Some(format!(
            "index note: could not re-index {path} ({e}); outline/symbol/refs may be stale for it"
        )),
    }
}

/// Join the optional notes (syntax warning, index note) that follow an edit's output.
fn join_notes(notes: impl IntoIterator<Item = Option<String>>) -> Option<String> {
    let notes: Vec<String> = notes.into_iter().flatten().collect();
    (!notes.is_empty()).then(|| notes.join("\n"))
}

/// Write `edited` (LF text) over a loaded file, keeping its encoding, and report the effect
/// plus a syntax warning if the edit introduced parse errors.
async fn write_edited(
    sb: &dyn Sandbox,
    cx: &ToolCx,
    path: &str,
    file: Loaded,
    edited: &str,
    repaired: bool,
) -> Result<(AppliedEffect, Option<String>), String> {
    if cx.cancel().is_cancelled() {
        return Err(crate::CANCELLED_BEFORE_WRITE.to_string());
    }
    // The requested path, not `file.full`: the sandbox refuses symlinks into protected paths.
    atomic::preserve_and_write(sb, Path::new(path), &file.original, edited)
        .await
        .map_err(|e| format!("{path}: {e}"))?;
    let post_hash = record_written(
        sb,
        cx,
        &file.full,
        Some(&file.original),
        &atomic::restore(&file.original, edited),
        repaired,
    )
    .await;
    let warning = join_notes([
        syntax_warning(path, &file.text, edited),
        reindex(cx, path).await,
    ]);
    let effect = AppliedEffect::Edited {
        path: file.full,
        pre_image: Some(file.original),
        pre_mode: Some(file.meta.mode & 0o7777),
        post_hash,
    };
    Ok((effect, warning))
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
                Err(e) => ToolResult::error_kind(e.to_string(), kind::BAD_ARGS),
            },
            EditFormat::Patch => match parse_args(&args) {
                Ok(a) => run_patch(a, sb, cx).await,
                Err(e) => ToolResult::error_kind(e.to_string(), kind::BAD_ARGS),
            },
            EditFormat::Hashline => match parse_args(&args) {
                Ok(a) => run_hashline(a, sb, cx).await,
                Err(e) => ToolResult::error_kind(e.to_string(), kind::BAD_ARGS),
            },
        }
    }
}

async fn run_str_replace(a: StrReplaceArgs, sb: &dyn Sandbox, cx: &ToolCx) -> ToolResult {
    if a.old_text.is_empty() {
        return ToolResult::error_kind("old_text must not be empty", kind::BAD_ARGS);
    }
    if a.old_text == a.new_text {
        return ToolResult::error_kind(
            "old_text and new_text are identical; nothing to change",
            kind::BAD_ARGS,
        );
    }
    let file = match load_fresh(sb, cx, &a.path, true).await {
        Ok(f) => f,
        Err(e) => return e,
    };
    let spliced = match str_replace::apply_staged_for(
        Path::new(&a.path),
        &file.text,
        &a.old_text,
        &a.new_text,
    ) {
        Ok(r) => r,
        Err(e) => {
            return ToolResult::error_kind(
                match_error_message(&a.path, "old_text", &e),
                match_error_kind(&e),
            );
        }
    };
    let (edited, stage) = (spliced.text, spliced.stage);
    if needs_seen(stage) && !seen_now(cx, &file, spliced.lines) {
        return ToolResult::error_kind(unseen_repair(&a.path, stage, spliced.lines), kind::STALE);
    }
    // A symbol-level guess must leave the file parsing at least as well as before. Nothing
    // has been written yet, so rejecting it leaves the pre-image in place.
    if stage == RepairStage::Structural
        && let Some(errors) = new_error_list(&a.path, &file.text, &edited)
    {
        return ToolResult::error_kind(
            format!(
                "{}: old_text not found exactly; the symbol it names was matched structurally, \
                 but replacing it would introduce syntax errors at {errors}, so nothing was \
                 changed. Retry with old_text copied from `read` or `symbol` output.",
                a.path
            ),
            kind::SYNTAX,
        );
    }
    let repaired = stage != RepairStage::Exact;
    let (effect, warning) = match write_edited(sb, cx, &a.path, file, &edited, repaired).await {
        Ok(e) => e,
        Err(e) => return ToolResult::error_kind(e, kind::IO),
    };
    let mut output = format!("edited {}", a.path);
    if stage != RepairStage::Exact {
        output.push_str(&format!("; applied via {}", stage.describe()));
    }
    if let Some(w) = warning {
        output.push('\n');
        output.push_str(&w);
    }
    let mut r = ToolResult::ok(output);
    r.effects.push(effect);
    r
}

async fn run_hashline(a: HashlineArgs, sb: &dyn Sandbox, cx: &ToolCx) -> ToolResult {
    if a.ops.is_empty() {
        return ToolResult::error_kind("ops must not be empty", kind::BAD_ARGS);
    }
    let count = a.ops.len();
    let ops: Vec<HashlineOp> = match (a.ops.into_iter().enumerate())
        .map(|(i, op)| op.into_op(i + 1))
        .collect()
    {
        Ok(ops) => ops,
        Err(e) => return ToolResult::error_kind(format!("{}: {e}", a.path), kind::BAD_ARGS),
    };
    let file = match load_fresh(sb, cx, &a.path, true).await {
        Ok(f) => f,
        Err(e) => return e,
    };
    let edited = match hashline::apply(&file.text, &ops) {
        Ok(t) => t,
        Err(e @ (HashlineError::Stale { .. } | HashlineError::OutOfRange { .. })) => {
            return ToolResult::error_kind(
                format!("{}: {e}; read the file again to get fresh anchors", a.path),
                kind::STALE_ANCHOR,
            );
        }
        Err(e) => return ToolResult::error_kind(format!("{}: {e}", a.path), kind::INVALID_OPS),
    };
    match write_edited(sb, cx, &a.path, file, &edited, false).await {
        Ok((effect, warning)) => {
            let mut output = format!("edited {} ({count} ops)", a.path);
            if let Some(w) = warning {
                output.push('\n');
                output.push_str(&w);
            }
            let mut r = ToolResult::ok(output);
            r.effects.push(effect);
            r
        }
        Err(e) => ToolResult::error_kind(e, kind::IO),
    }
}

/// One file of a validated patch, ready to write.
struct Step {
    /// The path as the patch names it; writes go through this (not `full`) so the
    /// sandbox can refuse symlinks into protected paths.
    rel: String,
    full: PathBuf,
    op: PatchOp,
    /// The file as it was (None for an added file).
    original: Option<Vec<u8>>,
    mode: Option<u32>,
    /// What to write for an update or add.
    bytes: Vec<u8>,
    /// Set when the new content has parse errors the old content did not.
    warning: Option<String>,
    /// A tolerant stage located one of its hunks.
    repaired: bool,
}

impl Step {
    async fn apply(&self, sb: &dyn Sandbox) -> Result<(), SandboxError> {
        let path = Path::new(&self.rel);
        match self.op {
            PatchOp::Delete => sb.remove_file(path).await,
            PatchOp::Update(_) | PatchOp::Add(_) => sb.write_atomic(path, &self.bytes, None).await,
        }
    }

    /// Put the file back as it was before this step.
    async fn undo(&self, sb: &dyn Sandbox) -> Result<(), SandboxError> {
        let path = Path::new(&self.rel);
        match &self.original {
            Some(original) => {
                let mode = self.mode.map(|m| m & 0o7777);
                sb.write_atomic(path, original, mode).await
            }
            None => sb.remove_file(path).await,
        }
    }
}

async fn run_patch(a: PatchArgs, sb: &dyn Sandbox, cx: &ToolCx) -> ToolResult {
    let touched = match patch::touched(&a.patch) {
        Ok(t) => t,
        Err(e) => return ToolResult::error_kind(e.to_string(), patch_error_kind(&e)),
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
                    Err(e) => return ToolResult::error_kind(e.to_string(), kind::IO),
                };
                match sb.metadata(&full).await {
                    Ok(_) => {
                        texts.insert(t.path.clone(), String::new());
                    }
                    Err(SandboxError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return ToolResult::error_kind(format!("{rel}: {e}"), kind::IO),
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
            Err(e) => return e,
        }
    }
    // Two spellings of one path would otherwise be written twice.
    if let Some((i, _)) = loaded
        .iter()
        .enumerate()
        .find(|(i, (p, _))| loaded[..*i].iter().any(|(q, _)| q == p))
    {
        return ToolResult::error_kind(
            format!(
                "{}: appears more than once in the patch",
                touched[i].path.display()
            ),
            kind::INVALID_PATCH,
        );
    }

    let applied = match patch::apply_staged(&|p| texts.get(p).cloned(), &a.patch) {
        Ok(v) => v,
        Err(e) => return ToolResult::error_kind(e.to_string(), patch_error_kind(&e)),
    };

    let mut steps: Vec<Step> = Vec::with_capacity(applied.len());
    let mut notes: Vec<String> = Vec::new();
    for (ap, (full, file)) in applied.iter().zip(loaded) {
        let rel = ap.path.to_string_lossy().into_owned();
        if let Some(f) = &file {
            for r in ap.repairs.iter().filter(|r| needs_seen(r.stage)) {
                if !seen_now(cx, f, r.lines) {
                    let subject = format!("{rel}: hunk {}", r.hunk);
                    return ToolResult::error_kind(
                        unseen_repair(&subject, r.stage, r.lines),
                        kind::STALE,
                    );
                }
            }
        }
        let (original, mode) = match file {
            Some(f) => (Some(f.original), Some(f.meta.mode)),
            None => (None, None),
        };
        let bytes = match (&ap.op, &original) {
            (PatchOp::Update(text), Some(orig)) => atomic::restore(orig, text),
            (PatchOp::Add(text), _) => text.clone().into_bytes(),
            _ => Vec::new(),
        };
        let warning = match &ap.op {
            PatchOp::Update(text) => {
                let before = texts.get(&ap.path).map_or("", String::as_str);
                syntax_warning(&rel, before, text)
            }
            PatchOp::Add(text) => syntax_warning(&rel, "", text),
            PatchOp::Delete => None,
        };
        for r in &ap.repairs {
            notes.push(format!(
                "{rel}: hunk {} applied via {}",
                r.hunk,
                r.stage.describe()
            ));
        }
        steps.push(Step {
            rel,
            full,
            op: ap.op.clone(),
            original,
            mode,
            bytes,
            warning,
            repaired: !ap.repairs.is_empty(),
        });
    }

    // All hunks are validated; now write. A failure undoes the files already written.
    if cx.cancel().is_cancelled() {
        return ToolResult::error(crate::CANCELLED_BEFORE_WRITE);
    }
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
        return ToolResult::error_kind(msg, kind::IO);
    }

    let mut lines = Vec::with_capacity(steps.len() + notes.len());
    let mut warnings = Vec::new();
    let mut effects = Vec::with_capacity(steps.len());
    for step in steps {
        let post_hash = match step.op {
            PatchOp::Delete => None,
            PatchOp::Update(_) | PatchOp::Add(_) => {
                let h = record_written(
                    sb,
                    cx,
                    &step.full,
                    step.original.as_deref(),
                    &step.bytes,
                    step.repaired,
                )
                .await;
                Some(h)
            }
        };
        let verb = match step.op {
            PatchOp::Delete => "deleted",
            PatchOp::Add(_) => "added",
            PatchOp::Update(_) => "updated",
        };
        lines.push(format!("{verb} {}", step.rel));
        warnings.extend(step.warning);
        // The path as the patch names it; a deleted file is dropped from the index.
        warnings.extend(reindex(cx, &step.rel).await);
        let pre_mode = step.mode.map(|m| m & 0o7777);
        effects.push(match post_hash {
            Some(post_hash) => AppliedEffect::Edited {
                path: step.full,
                pre_image: step.original,
                pre_mode,
                post_hash,
            },
            // Only an existing file can be deleted, so it has a pre-image and a mode.
            None => AppliedEffect::Deleted {
                path: step.full,
                pre_image: step.original.unwrap_or_default(),
                pre_mode: pre_mode.unwrap_or(0o644),
            },
        });
    }
    lines.extend(notes);
    lines.extend(warnings);
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
        MatchError::MixedArtifacts => format!(
            "{subject}: the replacement has copied `read` line numbers or anchors on some \
             lines but not all; remove them (write only the file's text) and retry"
        ),
    }
}

#[cfg(test)]
mod kind_tests {
    use super::kind;
    use crate::persist::ERROR_KINDS;

    /// `ToolMeta` reads error kinds back by looking them up in `ERROR_KINDS`, so a kind
    /// missing there would come back as `unknown`.
    #[test]
    fn every_edit_error_kind_is_deserializable() {
        for k in [
            kind::BAD_ARGS,
            kind::IO,
            kind::UNDECODABLE,
            kind::NOT_READ,
            kind::STALE,
            kind::NOT_FOUND,
            kind::AMBIGUOUS,
            kind::MIXED_ARTIFACTS,
            kind::SYNTAX,
            kind::STALE_ANCHOR,
            kind::INVALID_OPS,
            kind::INVALID_PATCH,
        ] {
            assert!(ERROR_KINDS.contains(&k), "{k} missing from ERROR_KINDS");
        }
    }
}
