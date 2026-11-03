use std::path::{Path, PathBuf};

use async_trait::async_trait;
use hx_index::SymbolKind;
use hx_provider::{Capabilities, EditFormat, ToolSpec};
use hx_sandbox::{Effect, Sandbox};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::cap::{OUTPUT_CAP, cap_text};
use crate::spec::{decode_text, parse_args, tool_spec};
use crate::{AppliedEffect, Tool, ToolCx, ToolError, ToolResult};

const MAX_LINES: usize = 2000;
/// Most targets in one batched read.
const MAX_TARGETS: usize = 10;

const BOTH: &str = "pass either `path` or `targets`, not both";
const NEITHER: &str = "pass `path`, or `targets` to read several";

pub struct ReadTool;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    path: Option<String>,
    start_line: Option<usize>,
    end_line: Option<usize>,
    targets: Option<Vec<Target>>,
}

/// One item of a batched read.
#[derive(Deserialize)]
#[serde(untagged)]
enum Target {
    Path(PathTarget),
    Symbol(SymbolTarget),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PathTarget {
    path: String,
    start_line: Option<usize>,
    end_line: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SymbolTarget {
    symbol: String,
    kind: Option<KindArg>,
}

#[derive(Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
enum KindArg {
    Function,
    Method,
    Type,
    Constant,
    Module,
}

impl From<KindArg> for SymbolKind {
    fn from(k: KindArg) -> Self {
        match k {
            KindArg::Function => SymbolKind::Function,
            KindArg::Method => SymbolKind::Method,
            KindArg::Type => SymbolKind::Type,
            KindArg::Constant => SymbolKind::Constant,
            KindArg::Module => SymbolKind::Module,
        }
    }
}

/// The lines of one file that a read selected, before any output cap.
struct Lines {
    full: PathBuf,
    hash: blake3::Hash,
    /// 1-based inclusive range actually read.
    start: usize,
    end: usize,
    /// Lines after `end` that were left out by the 2,000-line default window.
    remaining: usize,
    /// The numbered (or hashline-tagged) lines, newline-joined.
    text: String,
}

impl Lines {
    /// The output for this read within `cap` bytes, with continuation notes.
    fn render(&self, cap: usize) -> String {
        let (mut output, truncated) = cap_text(&self.text, cap);
        if truncated {
            output.push_str("\n[output truncated; request a narrower line range]");
        }
        if self.remaining > 0 {
            output.push_str(&format!(
                "\n[{} more lines; use start_line={} to continue]",
                self.remaining,
                self.end + 1
            ));
        }
        output
    }
}

/// Read `path` (optionally a line range) and record it in the read tracker. The one code
/// path behind both single and batched reads.
async fn read_lines(
    path: &str,
    start_line: Option<usize>,
    end_line: Option<usize>,
    sb: &dyn Sandbox,
    cx: &ToolCx,
) -> Result<Lines, String> {
    let full = sb.resolve(Path::new(path)).map_err(|e| e.to_string())?;
    let (meta, bytes) = match (sb.metadata(&full).await, sb.read(&full).await) {
        (Ok(m), Ok(b)) => (m, b),
        (Err(e), _) | (_, Err(e)) => return Err(format!("{path}: {e}")),
    };
    let text = decode_text(&bytes).map_err(|why| format!("{path}: {why}"))?;
    let hash = blake3::hash(&bytes);
    cx.reads().record(&full, &meta, hash);

    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    let start = start_line.unwrap_or(1);
    if start == 0 || end_line == Some(0) {
        return Err("line numbers are 1-based".into());
    }
    let mut end = end_line.unwrap_or(total).min(total);
    if total > 0 && start > end {
        return Err(format!(
            "start_line {start} is past the end ({total} lines) or after end_line"
        ));
    }
    let mut remaining = 0;
    // Line numbers come from the model and may be near usize::MAX.
    if end_line.is_none() && end.saturating_add(1) > start.saturating_add(MAX_LINES) {
        // No overflow: start + MAX_LINES < end + 1 <= total + 1 here.
        end = start + MAX_LINES - 1;
        remaining = total - end;
    }
    let hashline = cx.edit_format() == EditFormat::Hashline;
    let mut out: Vec<String> = Vec::new();
    for (i, line) in lines.iter().enumerate().take(end).skip(start - 1) {
        let n = i + 1;
        if hashline {
            let tag = &blake3::hash(line.as_bytes()).to_hex()[..4];
            out.push(format!("{n}:{tag}|{line}"));
        } else {
            out.push(format!("{n}\t{line}"));
        }
    }
    Ok(Lines {
        full,
        hash,
        start,
        end,
        remaining,
        text: out.join("\n"),
    })
}

/// One target's section of a batched read.
struct Section {
    header: String,
    /// The read lines, or the reason this target could not be read.
    body: Result<Lines, String>,
}

async fn read_target(target: &Target, sb: &dyn Sandbox, cx: &ToolCx) -> Section {
    match target {
        Target::Path(t) => {
            let ranged = t.start_line.is_some() || t.end_line.is_some();
            let body = read_lines(&t.path, t.start_line, t.end_line, sb, cx).await;
            let header = match &body {
                Ok(l) if ranged => format!("{}:{}-{}", t.path, l.start, l.end),
                _ => t.path.clone(),
            };
            Section {
                header: format!("== {header} =="),
                body,
            }
        }
        Target::Symbol(t) => symbol_section(t, sb, cx).await,
    }
}

async fn symbol_section(t: &SymbolTarget, sb: &dyn Sandbox, cx: &ToolCx) -> Section {
    let mut section = Section {
        header: format!("== symbol {} ==", t.symbol),
        body: Err(String::new()),
    };
    let Some(index) = cx.index().filter(|i| i.is_ready()) else {
        section.body = Err(
            "symbol targets need the code index, which is not available; \
                            read the file by path"
                .into(),
        );
        return section;
    };
    let rows = match index.find_symbol(&t.symbol, t.kind.map(SymbolKind::from)) {
        Ok(r) => r,
        Err(e) => {
            section.body = Err(format!("index error: {e}"));
            return section;
        }
    };
    match rows.as_slice() {
        [] => section.body = Err(format!("no definition of `{}` found", t.symbol)),
        [row] => {
            let path = row.path.display().to_string();
            let (s, e) = (row.start_line as usize, row.end_line as usize);
            section.body = read_lines(&path, Some(s), Some(e), sb, cx).await;
            if section.body.is_ok() {
                section.header = format!("== symbol {} ({path}:{s}-{e}) ==", t.symbol);
            }
        }
        many => {
            let places: Vec<String> = many
                .iter()
                .map(|r| {
                    format!(
                        "{}:{}-{} {}",
                        r.path.display(),
                        r.start_line,
                        r.end_line,
                        r.kind.as_str()
                    )
                })
                .collect();
            section.body = Err(format!(
                "{} definitions of `{}`; narrow with `kind` or read one by path and range:\n{}",
                many.len(),
                t.symbol,
                places.join("\n")
            ));
        }
    }
    section
}

/// Room kept in a truncated section for the notes the renderer appends.
const NOTE_ALLOWANCE: usize = 120;

impl Section {
    /// The section's text within `body_cap` bytes of body.
    fn body_text(&self, body_cap: usize) -> String {
        match &self.body {
            Ok(lines) => lines.render(body_cap),
            Err(why) => cap_text(&format!("[error] {why}"), body_cap).0,
        }
    }
}

/// Concatenate the sections under the shared output cap by water-filling: the smallest
/// sections get their full size, and what is left is split evenly among the larger ones,
/// so the result does not depend on the order of the targets.
fn assemble(sections: &[Section]) -> String {
    const SEPARATOR: &str = "\n\n";
    let full: Vec<String> = sections.iter().map(|s| s.body_text(usize::MAX)).collect();
    let size = |i: usize| sections[i].header.len() + 1 + full[i].len();
    let mut budget = OUTPUT_CAP.saturating_sub(SEPARATOR.len() * (sections.len() - 1));
    let mut order: Vec<usize> = (0..sections.len()).collect();
    order.sort_by_key(|&i| size(i));
    let mut allowed = vec![0; sections.len()];
    for (k, &i) in order.iter().enumerate() {
        let share = budget / (sections.len() - k);
        allowed[i] = size(i).min(share);
        budget -= allowed[i];
    }
    let mut out = String::new();
    for (i, section) in sections.iter().enumerate() {
        if i > 0 {
            out.push_str(SEPARATOR);
        }
        out.push_str(&section.header);
        out.push('\n');
        if size(i) <= allowed[i] {
            out.push_str(&full[i]);
        } else {
            let cap = allowed[i].saturating_sub(section.header.len() + 1 + NOTE_ALLOWANCE);
            out.push_str(&section.body_text(cap));
        }
    }
    out
}

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &'static str {
        "read"
    }

    fn spec(&self, _caps: &Capabilities) -> ToolSpec {
        tool_spec(
            "read",
            "Read a text file with line numbers. Use start_line/end_line (1-based, inclusive) for a range. \
             To read several files or definitions in one call, pass `targets` (at most 10) instead of `path`.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path."},
                    "start_line": {"type": "integer", "minimum": 1, "description": "First line to read."},
                    "end_line": {"type": "integer", "minimum": 1, "description": "Last line to read."},
                    "targets": {
                        "type": "array",
                        "maxItems": MAX_TARGETS,
                        "description": "Read several things at once instead of `path`: each item is a file with an optional line range, or a symbol.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "path": {"type": "string", "description": "File path."},
                                "start_line": {"type": "integer", "minimum": 1},
                                "end_line": {"type": "integer", "minimum": 1},
                                "symbol": {"type": "string", "description": "Exact name of a definition to read instead of a file."},
                                "kind": {
                                    "type": "string",
                                    "enum": ["function", "method", "type", "constant", "module"],
                                    "description": "With `symbol`: only definitions of this kind."
                                }
                            },
                            "additionalProperties": false
                        }
                    }
                },
                "additionalProperties": false
            }),
        )
    }

    fn effect(&self, args: &Value) -> Result<Effect, ToolError> {
        let a: Args = parse_args(args)?;
        match (&a.path, &a.targets) {
            (Some(_), Some(_)) => Err(ToolError::BadArgs(BOTH.into())),
            (None, None) => Err(ToolError::BadArgs(NEITHER.into())),
            _ => Ok(Effect::ReadOnly),
        }
    }

    async fn run(&self, args: Value, sb: &dyn Sandbox, cx: &ToolCx) -> ToolResult {
        let a: Args = match parse_args(&args) {
            Ok(a) => a,
            Err(e) => return ToolResult::error(e.to_string()),
        };
        match (&a.path, &a.targets) {
            (Some(_), Some(_)) => ToolResult::error("pass either `path` or `targets`, not both"),
            (None, None) => ToolResult::error("pass `path`, or `targets` to read several"),
            (Some(path), None) => match read_lines(path, a.start_line, a.end_line, sb, cx).await {
                Ok(lines) => {
                    let mut r = ToolResult::ok(lines.render(OUTPUT_CAP));
                    r.effects.push(AppliedEffect::Read(lines.full, lines.hash));
                    r
                }
                Err(e) => ToolResult::error(e),
            },
            (None, Some(targets)) => {
                if a.start_line.is_some() || a.end_line.is_some() {
                    return ToolResult::error(
                        "start_line/end_line belong on each target when using `targets`",
                    );
                }
                read_batch(targets, sb, cx).await
            }
        }
    }
}

async fn read_batch(targets: &[Target], sb: &dyn Sandbox, cx: &ToolCx) -> ToolResult {
    if targets.is_empty() {
        return ToolResult::error("`targets` is empty");
    }
    if targets.len() > MAX_TARGETS {
        return ToolResult::error(format!(
            "at most {MAX_TARGETS} targets per read, got {}",
            targets.len()
        ));
    }
    let mut sections = Vec::with_capacity(targets.len());
    for target in targets {
        if cx.cancel().is_cancelled() {
            return ToolResult::error("cancelled");
        }
        sections.push(read_target(target, sb, cx).await);
    }
    let mut result = if sections.iter().all(|s| s.body.is_err()) {
        ToolResult::error(assemble(&sections))
    } else {
        ToolResult::ok(assemble(&sections))
    };
    for section in sections {
        if let Ok(lines) = section.body {
            result
                .effects
                .push(AppliedEffect::Read(lines.full, lines.hash));
        }
    }
    result
}
