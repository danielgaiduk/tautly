use std::path::{Path, PathBuf};

use async_trait::async_trait;
use hx_index::SymbolKind;
use hx_provider::{Capabilities, EditFormat, ToolSpec};
use hx_sandbox::{Effect, Sandbox};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::cap::{OUTPUT_CAP, cap_text};
use crate::reread::small_diff;
use crate::spec::{decode_text, parse_args, tool_spec};
use crate::{
    AppliedEffect, LineRange, MAX_READ_LINES, PrevRead, Tool, ToolCx, ToolError, ToolResult,
};

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
    /// Return full content even when the range was read before.
    full: Option<bool>,
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
    /// The range as requested, which identifies the read for re-read tracking.
    range: LineRange,
    /// 1-based inclusive range actually read.
    start: usize,
    end: usize,
    /// Lines after `end` that were left out by the 2,000-line default window.
    remaining: usize,
    /// The numbered (or hashline-tagged) lines, newline-joined.
    text: String,
    /// The baseline to commit once the result is about to be returned.
    pending: Option<Pending>,
}

/// What the tracker records for a read, committed only if the model gets all of the output.
/// `None`: the content could not be kept, so the commit only drops overlapping baselines.
struct Pending(Option<PrevRead>);

impl Lines {
    /// Make this read the baseline for the next read of its range. Synchronous (all the
    /// I/O was done by `read_lines`), so it either happens whole or not at all.
    fn commit(&self, cx: &ToolCx) {
        if let Some(Pending(prev)) = &self.pending {
            cx.reads().supersede(&self.full, self.range, prev.clone());
        }
    }

    /// The output for this read within `cap` bytes, with continuation notes.
    fn render(&self, cap: usize) -> String {
        self.render_flagged(cap).0
    }

    /// Like [`render`](Self::render), also saying whether the byte cap cut the text.
    fn render_flagged(&self, cap: usize) -> (String, bool) {
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
        (output, truncated)
    }
}

/// Read `path` (optionally a line range) and record it in the read tracker. The one code
/// path behind both single and batched reads.
async fn read_lines(
    path: &str,
    start_line: Option<usize>,
    end_line: Option<usize>,
    force_full: bool,
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
    // The stale-read guard sees this read even if the range below turns out to be invalid.
    cx.reads().refresh(&full, &meta, hash);
    let range = LineRange {
        start: start_line,
        end: end_line,
    };
    // A partial read cannot refresh the guard past a change to the rest of the file while a
    // whole-file baseline of the older content still stands.
    if range != LineRange::ALL {
        cx.reads().drop_stale_whole(&full, hash);
    }

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
    if end_line.is_none() && end.saturating_add(1) > start.saturating_add(MAX_READ_LINES) {
        // No overflow: start + MAX_READ_LINES < end + 1 <= total + 1 here.
        end = start + MAX_READ_LINES - 1;
        remaining = total - end;
    }
    let shown = if end >= start {
        &lines[start - 1..end]
    } else {
        &[]
    };
    let content = shown.join("\n");
    let hashline = cx.edit_format() == EditFormat::Hashline;
    let mut text = shown
        .iter()
        .enumerate()
        .map(|(i, line)| {
            let n = start + i;
            if hashline {
                let tag = &blake3::hash(line.as_bytes()).to_hex()[..4];
                format!("{n}:{tag}|{line}")
            } else {
                format!("{n}\t{line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n");

    let mut pending = None;
    if remaining > 0 {
        // The window cut the range: the model sees only part of it, so neither answer
        // `unchanged` or with a diff (which would drop the continuation note), nor
        // remember it.
        cx.reads().forget(&full, range);
    } else {
        let prev = cx
            .reads()
            .previous(&full, range, cx.edit_format())
            .filter(|_| !force_full);
        let mut unchanged = false;
        if let Some(prev) = prev {
            let turn = prev.turn;
            if prev.content_hash == blake3::hash(content.as_bytes()) {
                // The earlier content is still in context; say nothing more.
                unchanged = true;
                text = format!("unchanged since turn {turn}");
            } else {
                // A hashline edit needs the anchors of the current lines, which a diff would
                // lack, so in that format a change always returns the full content.
                let diff = if hashline {
                    None
                } else {
                    diff_against(sb, &prev, shown, start).await
                };
                text = match diff {
                    Some(diff) => format!("changed since turn {turn}:\n{diff}"),
                    None => format!("changed since turn {turn} (full content):\n{text}"),
                };
            }
        }
        // An unchanged read keeps its baseline, and with it the turn the content was shown.
        // The commit waits until the caller knows the output was delivered whole.
        if !unchanged {
            // Spilled now (content-addressed, harmless if never committed).
            let kept = sb
                .spill(content.as_bytes())
                .await
                .ok()
                .map(|content_path| PrevRead {
                    hash,
                    format: cx.edit_format(),
                    content_hash: blake3::hash(content.as_bytes()),
                    turn: cx.turn(),
                    mtime: meta.mtime,
                    content_path,
                });
            pending = Some(Pending(kept));
        }
    }
    Ok(Lines {
        full,
        hash,
        range,
        start,
        end,
        remaining,
        text,
        pending,
    })
}

/// A small diff from what `prev` returned to `new` (the lines of the same range, starting
/// at file line `first_line`). `None` when the old content is gone from disk or altered,
/// or when too much changed for a diff to be worth it: the caller then returns full content.
async fn diff_against(
    sb: &dyn Sandbox,
    prev: &PrevRead,
    new: &[&str],
    first_line: usize,
) -> Option<String> {
    let bytes = sb.read(&prev.content_path).await.ok()?;
    if blake3::hash(&bytes) != prev.content_hash {
        return None;
    }
    let old = String::from_utf8(bytes).ok()?;
    let old: Vec<&str> = if old.is_empty() {
        Vec::new()
    } else {
        old.split('\n').collect()
    };
    small_diff(&old, new, first_line)
}

/// One target's section of a batched read.
struct Section {
    header: String,
    /// The read lines, or the reason this target could not be read.
    body: Result<Lines, String>,
}

async fn read_target(target: &Target, force_full: bool, sb: &dyn Sandbox, cx: &ToolCx) -> Section {
    match target {
        Target::Path(t) => {
            let ranged = t.start_line.is_some() || t.end_line.is_some();
            let body = read_lines(&t.path, t.start_line, t.end_line, force_full, sb, cx).await;
            let header = match &body {
                Ok(l) if ranged => format!("{}:{}-{}", t.path, l.start, l.end),
                _ => t.path.clone(),
            };
            Section {
                header: format!("== {header} =="),
                body,
            }
        }
        Target::Symbol(t) => symbol_section(t, force_full, sb, cx).await,
    }
}

async fn symbol_section(
    t: &SymbolTarget,
    force_full: bool,
    sb: &dyn Sandbox,
    cx: &ToolCx,
) -> Section {
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
            section.body = read_lines(&path, Some(s), Some(e), force_full, sb, cx).await;
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
///
/// Also returns, per section, whether its body was cut by the cap.
fn assemble(sections: &[Section]) -> (String, Vec<bool>) {
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
    let mut cut = Vec::with_capacity(sections.len());
    for (i, section) in sections.iter().enumerate() {
        if i > 0 {
            out.push_str(SEPARATOR);
        }
        out.push_str(&section.header);
        out.push('\n');
        cut.push(size(i) > allowed[i]);
        if size(i) <= allowed[i] {
            out.push_str(&full[i]);
        } else {
            let cap = allowed[i].saturating_sub(section.header.len() + 1 + NOTE_ALLOWANCE);
            out.push_str(&section.body_text(cap));
        }
    }
    (out, cut)
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
             To read several files or definitions in one call, pass `targets` (at most 10) instead of `path`. \
             Reading a range again returns `unchanged since turn N` or a diff of what changed; pass `full: true` to get the whole content.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path."},
                    "start_line": {"type": "integer", "minimum": 1, "description": "First line to read."},
                    "end_line": {"type": "integer", "minimum": 1, "description": "Last line to read."},
                    "full": {"type": "boolean", "description": "Return the full content even if this range was read before. By default a repeated read says `unchanged since turn N` or shows a diff."},
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
        let full = a.full.unwrap_or(false);
        match (&a.path, &a.targets) {
            (Some(_), Some(_)) => ToolResult::error("pass either `path` or `targets`, not both"),
            (None, None) => ToolResult::error("pass `path`, or `targets` to read several"),
            (Some(path), None) => {
                match read_lines(path, a.start_line, a.end_line, full, sb, cx).await {
                    Ok(lines) => {
                        let (output, truncated) = lines.render_flagged(OUTPUT_CAP);
                        if truncated {
                            // The model did not see all of it; show it again next time.
                            cx.reads().forget(&lines.full, lines.range);
                        } else if cx.cancel().is_cancelled() {
                            return ToolResult::error("cancelled");
                        } else {
                            lines.commit(cx);
                        }
                        let mut r = ToolResult::ok(output);
                        r.effects.push(AppliedEffect::Read(lines.full, lines.hash));
                        r
                    }
                    Err(e) => ToolResult::error(e),
                }
            }
            (None, Some(targets)) => {
                if a.start_line.is_some() || a.end_line.is_some() {
                    return ToolResult::error(
                        "start_line/end_line belong on each target when using `targets`",
                    );
                }
                read_batch(targets, full, sb, cx).await
            }
        }
    }
}

async fn read_batch(
    targets: &[Target],
    force_full: bool,
    sb: &dyn Sandbox,
    cx: &ToolCx,
) -> ToolResult {
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
        sections.push(read_target(target, force_full, sb, cx).await);
    }
    if cx.cancel().is_cancelled() {
        return ToolResult::error("cancelled");
    }
    let (output, cut) = assemble(&sections);
    // Baselines are committed only now that the output is about to be returned.
    for (section, cut) in sections.iter().zip(&cut) {
        if let Ok(lines) = &section.body {
            if *cut {
                cx.reads().forget(&lines.full, lines.range);
            } else {
                lines.commit(cx);
            }
        }
    }
    let mut result = if sections.iter().all(|s| s.body.is_err()) {
        ToolResult::error(output)
    } else {
        ToolResult::ok(output)
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
