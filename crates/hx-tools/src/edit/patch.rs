//! `patch`: Codex apply-patch envelopes and unified diffs (spec 5.3, 7.9).
//!
//! Hunk context is located with the tolerant applier ([`locate_lines`]: exact first, then
//! stages 1-4, ambiguity is an error). Hunks of one file are all located in the original
//! text and spliced in from the bottom, so they may not overlap. Nothing here touches the
//! filesystem: [`apply`] works on the file contents it is given.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;

use super::str_replace::MatchError;
use super::tolerant::{RepairStage, adapt_new_text, locate_lines};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatchOp {
    /// The file's new content (LF line endings).
    Update(String),
    /// A new file's content.
    Add(String),
    Delete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Update,
    Add,
    Delete,
}

/// A file a patch touches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Touched {
    pub path: PathBuf,
    pub kind: Kind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatchError {
    /// Malformed patch text.
    Format(String),
    /// Valid syntax the tool does not support (renames).
    Unsupported(String),
    DuplicateFile(PathBuf),
    NotFound(PathBuf),
    AlreadyExists(PathBuf),
    /// Hunk `hunk` (1-based) of `path` could not be placed.
    Hunk {
        path: PathBuf,
        hunk: usize,
        error: MatchError,
    },
    /// Hunks `a` and `b` (1-based) of `path` cover overlapping lines.
    Overlap {
        path: PathBuf,
        a: usize,
        b: usize,
    },
}

impl fmt::Display for PatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PatchError::Format(m) => write!(f, "invalid patch: {m}"),
            PatchError::Unsupported(m) => write!(f, "unsupported patch: {m}"),
            PatchError::DuplicateFile(p) => write!(
                f,
                "{}: appears more than once in the patch; put all its hunks under one entry",
                p.display()
            ),
            PatchError::NotFound(p) => write!(f, "{}: file does not exist", p.display()),
            PatchError::AlreadyExists(p) => write!(
                f,
                "{}: already exists; use Update File instead of Add File",
                p.display()
            ),
            PatchError::Hunk { path, hunk, error } => f.write_str(&super::match_error_message(
                &format!("{}: hunk {hunk}", path.display()),
                "its context and `-` lines",
                error,
            )),
            PatchError::Overlap { path, a, b } => {
                write!(f, "{}: hunks {a} and {b} overlap", path.display())
            }
        }
    }
}

impl std::error::Error for PatchError {}

/// One file's result, with the hunks (1-based) that needed a tolerant stage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    pub path: PathBuf,
    pub op: PatchOp,
    pub repairs: Vec<(usize, RepairStage)>,
}

/// The files a patch touches, in order, without applying anything.
pub fn touched(patch: &str) -> Result<Vec<Touched>, PatchError> {
    Ok(parse(patch)?
        .into_iter()
        .map(|s| Touched {
            path: s.path,
            kind: match s.body {
                Body::Update(_) => Kind::Update,
                Body::Add(_) => Kind::Add,
                Body::Delete => Kind::Delete,
            },
        })
        .collect())
}

/// Apply `patch`. `files` returns the current LF-text content of an existing file
/// (`None`: no such file). Nothing is written; the caller applies the returned ops.
pub fn apply(
    files: &dyn Fn(&Path) -> Option<String>,
    patch: &str,
) -> Result<Vec<(PathBuf, PatchOp)>, PatchError> {
    apply_staged(files, patch).map(|v| v.into_iter().map(|a| (a.path, a.op)).collect())
}

/// Like [`apply`], also reporting repairs.
pub fn apply_staged(
    files: &dyn Fn(&Path) -> Option<String>,
    patch: &str,
) -> Result<Vec<Applied>, PatchError> {
    let sections = parse(patch)?;
    let mut out: Vec<Applied> = Vec::with_capacity(sections.len());
    for s in sections {
        if out.iter().any(|a| a.path == s.path) {
            return Err(PatchError::DuplicateFile(s.path));
        }
        let current = files(&s.path);
        let (op, repairs) = match s.body {
            Body::Delete => {
                current.ok_or_else(|| PatchError::NotFound(s.path.clone()))?;
                (PatchOp::Delete, Vec::new())
            }
            Body::Add(content) => {
                if current.is_some() {
                    return Err(PatchError::AlreadyExists(s.path));
                }
                (PatchOp::Add(content), Vec::new())
            }
            Body::Update(hunks) => {
                let text = current.ok_or_else(|| PatchError::NotFound(s.path.clone()))?;
                let (new, repairs) = apply_hunks(&s.path, &text, &hunks)?;
                (PatchOp::Update(new), repairs)
            }
        };
        out.push(Applied {
            path: s.path,
            op,
            repairs,
        });
    }
    Ok(out)
}

// ------------------------------------------------------------------ model

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tag {
    Context,
    Remove,
    Add,
}

#[derive(Debug, Default)]
struct Hunk {
    /// Text after `@@` in an envelope: a line that precedes the change.
    header: Option<String>,
    lines: Vec<(Tag, String)>,
    /// Unified diffs: the old-file line a context-free addition follows (0: the top).
    insert_after: Option<usize>,
}

impl Hunk {
    fn side(&self, keep: Tag) -> Vec<&str> {
        self.lines
            .iter()
            .filter(|(t, _)| *t == Tag::Context || *t == keep)
            .map(|(_, l)| l.as_str())
            .collect()
    }
}

enum Body {
    Update(Vec<Hunk>),
    Add(String),
    Delete,
}

struct Section {
    path: PathBuf,
    body: Body,
}

// ------------------------------------------------------------------ parsing

fn format_err(msg: impl Into<String>) -> PatchError {
    PatchError::Format(msg.into())
}

const BEGIN: &str = "*** Begin Patch";
const END: &str = "*** End Patch";

fn parse(patch: &str) -> Result<Vec<Section>, PatchError> {
    let normalized = patch.replace("\r\n", "\n");
    let mut lines: Vec<&str> = normalized.split('\n').collect();
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    let first = lines.iter().position(|l| !l.trim().is_empty());
    match first {
        Some(i) if lines[i].trim() == BEGIN => parse_envelope(&lines[i + 1..], i + 2),
        Some(_) if is_unified(&lines) => parse_unified(&lines),
        _ => Err(format_err(
            "expected `*** Begin Patch ... *** End Patch` or a unified diff (`--- a/x`, `+++ b/x`, `@@ ... @@`)",
        )),
    }
}

fn is_unified(lines: &[&str]) -> bool {
    lines
        .windows(2)
        .any(|w| w[0].starts_with("--- ") && w[1].starts_with("+++ "))
}

fn directive<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    line.strip_prefix("*** ")?
        .strip_prefix(name)?
        .strip_prefix(':')
        .map(str::trim)
}

fn file_path(raw: &str, line_no: usize) -> Result<PathBuf, PatchError> {
    if raw.is_empty() {
        return Err(format_err(format!("line {line_no}: missing file path")));
    }
    Ok(PathBuf::from(raw))
}

/// Parse a hunk body line; `None` for anything that is not context, removal or addition.
fn body_line(line: &str) -> Option<(Tag, String)> {
    match line.chars().next() {
        None => Some((Tag::Context, String::new())),
        Some(' ') => Some((Tag::Context, line[1..].to_string())),
        Some('-') => Some((Tag::Remove, line[1..].to_string())),
        Some('+') => Some((Tag::Add, line[1..].to_string())),
        _ => None,
    }
}

/// `first_line_no` is the 1-based number of `lines[0]`, for messages.
fn parse_envelope(lines: &[&str], first_line_no: usize) -> Result<Vec<Section>, PatchError> {
    let mut sections = Vec::new();
    let mut i = 0;
    loop {
        let Some(&line) = lines.get(i) else {
            return Err(format_err(format!("missing `{END}`")));
        };
        let no = first_line_no + i;
        i += 1;
        if line.trim() == END {
            break;
        }
        if line.trim().is_empty() {
            continue;
        }
        if let Some(p) = directive(line, "Delete File") {
            sections.push(Section {
                path: file_path(p, no)?,
                body: Body::Delete,
            });
        } else if let Some(p) = directive(line, "Add File") {
            let path = file_path(p, no)?;
            let mut content = String::new();
            while let Some(&l) = lines.get(i).filter(|l| !l.starts_with("*** ")) {
                let Some(added) = l.strip_prefix('+') else {
                    return Err(format_err(format!(
                        "line {}: lines of an added file must start with `+`",
                        first_line_no + i
                    )));
                };
                content.push_str(added);
                content.push('\n');
                i += 1;
            }
            sections.push(Section {
                path,
                body: Body::Add(content),
            });
        } else if let Some(p) = directive(line, "Update File") {
            let path = file_path(p, no)?;
            let mut hunks: Vec<Hunk> = Vec::new();
            while let Some(&l) = lines.get(i) {
                let no = first_line_no + i;
                if l.trim() == "*** End of File" {
                    i += 1;
                } else if directive(l, "Move to").is_some() {
                    return Err(PatchError::Unsupported(
                        "renames (`*** Move to:`) are not supported; delete the old file and add the new one".into(),
                    ));
                } else if l.starts_with("*** ") {
                    break;
                } else if let Some(rest) = l.strip_prefix("@@") {
                    let header = rest.trim();
                    // A unified-style `@@ -1,2 +1,2 @@` carries no anchor text.
                    let header = (!header.is_empty() && !HUNK_HEADER.is_match(l))
                        .then(|| header.to_string());
                    hunks.push(Hunk {
                        header,
                        ..Hunk::default()
                    });
                    i += 1;
                } else {
                    let Some(parsed) = body_line(l) else {
                        return Err(format_err(format!(
                            "line {no}: expected a line starting with ` `, `-` or `+`, or `@@`"
                        )));
                    };
                    // The first hunk of a file may omit its `@@`.
                    if hunks.is_empty() {
                        hunks.push(Hunk::default());
                    }
                    hunks.last_mut().expect("just pushed").lines.push(parsed);
                    i += 1;
                }
            }
            if hunks.is_empty() {
                return Err(format_err(format!(
                    "{}: `Update File` has no hunks",
                    path.display()
                )));
            }
            sections.push(Section {
                path,
                body: Body::Update(hunks),
            });
        } else {
            return Err(format_err(format!(
                "line {no}: expected `*** Update File:`, `*** Add File:`, `*** Delete File:` or `{END}`"
            )));
        }
    }
    if sections.is_empty() {
        return Err(format_err("the patch changes no files"));
    }
    Ok(sections)
}

static HUNK_HEADER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@").expect("valid regex")
});

fn dev_null(p: &str) -> bool {
    p == "/dev/null"
}

/// File name from a `---`/`+++` line: no timestamp suffix, no surrounding quotes.
fn diff_path(line: &str, marker: &str) -> String {
    let rest = line[marker.len()..].split('\t').next().unwrap_or("").trim();
    rest.trim_matches('"').to_string()
}

fn parse_unified(lines: &[&str]) -> Result<Vec<Section>, PatchError> {
    let at_file_header = |i: usize| {
        lines.get(i).is_some_and(|l| l.starts_with("--- "))
            && lines.get(i + 1).is_some_and(|l| l.starts_with("+++ "))
    };
    let mut sections = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if !at_file_header(i) {
            // `diff --git`, `index`, mode lines and any preamble.
            i += 1;
            continue;
        }
        let old = diff_path(lines[i], "--- ");
        let new = diff_path(lines[i + 1], "+++ ");
        i += 2;
        let mut hunks: Vec<Hunk> = Vec::new();
        while i < lines.len() && !at_file_header(i) && !lines[i].starts_with("diff ") {
            let l = lines[i];
            if let Some(caps) = HUNK_HEADER.captures(l) {
                let old_start: usize = caps[1].parse().unwrap_or(0);
                hunks.push(Hunk {
                    insert_after: Some(old_start),
                    ..Hunk::default()
                });
            } else if l.starts_with("@@") {
                return Err(format_err(format!("malformed hunk header `{l}`")));
            } else if l.starts_with('\\') {
                // `\ No newline at end of file`: line endings are preserved from the file.
            } else if let Some(h) = hunks.last_mut() {
                match body_line(l) {
                    Some(parsed) => h.lines.push(parsed),
                    None => {
                        return Err(format_err(format!(
                            "unexpected line in hunk: `{l}` (hunk lines start with ` `, `-` or `+`)"
                        )));
                    }
                }
            } else if !l.trim().is_empty() {
                return Err(format_err(format!(
                    "unexpected line before first hunk: `{l}`"
                )));
            }
            i += 1;
        }
        sections.push(unified_section(&old, &new, hunks)?);
    }
    Ok(sections)
}

fn strip_prefix_once<'a>(p: &'a str, prefix: &str) -> &'a str {
    p.strip_prefix(prefix).unwrap_or(p)
}

fn unified_section(old: &str, new: &str, hunks: Vec<Hunk>) -> Result<Section, PatchError> {
    if dev_null(old) {
        if dev_null(new) {
            return Err(format_err("both sides of a diff are /dev/null"));
        }
        let mut content = String::new();
        for h in &hunks {
            for (tag, l) in &h.lines {
                if *tag != Tag::Add {
                    return Err(format_err(format!(
                        "{new}: a new file's hunks may only add lines"
                    )));
                }
                content.push_str(l);
                content.push('\n');
            }
        }
        return Ok(Section {
            path: PathBuf::from(strip_prefix_once(new, "b/")),
            body: Body::Add(content),
        });
    }
    if dev_null(new) {
        return Ok(Section {
            path: PathBuf::from(strip_prefix_once(old, "a/")),
            body: Body::Delete,
        });
    }
    // Git style `a/x` and `b/x`: strip both only as a pair, so every path has exactly one
    // reading and the permission check sees the path that is touched.
    let (old, new) = match (old.strip_prefix("a/"), new.strip_prefix("b/")) {
        (Some(o), Some(n)) => (o, n),
        _ => (old, new),
    };
    if old != new {
        return Err(PatchError::Unsupported(format!(
            "renames ({old} -> {new}) are not supported; delete the old file and add the new one"
        )));
    }
    if hunks.is_empty() {
        return Err(format_err(format!("{new}: diff has no hunks")));
    }
    Ok(Section {
        path: PathBuf::from(new),
        body: Body::Update(hunks),
    })
}

// ------------------------------------------------------------------ applying hunks

/// A located hunk: replace `range` of the (newline-terminated) text with `replacement`.
struct Splice {
    hunk: usize,
    range: std::ops::Range<usize>,
    replacement: String,
}

fn join_lines(lines: &[&str]) -> String {
    let mut s = lines.join("\n");
    if !lines.is_empty() {
        s.push('\n');
    }
    s
}

fn not_found() -> MatchError {
    MatchError::NotFound {
        candidates: Vec::new(),
        nearest: None,
    }
}

/// Byte offset just after line number `n` (1-based; 0 is the top).
fn offset_after_line(text: &str, n: usize) -> Option<usize> {
    if n == 0 {
        return Some(0);
    }
    let mut off = 0;
    for _ in 0..n {
        off += text[off..].find('\n')? + 1;
    }
    Some(off)
}

/// Byte range (newline included) of the one line equal to `header` when trimmed.
fn unique_line(text: &str, header: &str) -> Option<std::ops::Range<usize>> {
    let mut found = None;
    let mut off = 0;
    for line in text.split_inclusive('\n') {
        let range = off..off + line.len();
        off = range.end;
        if line.trim() == header.trim() {
            if found.is_some() {
                return None;
            }
            found = Some(range);
        }
    }
    found
}

/// Where a hunk without context or removals goes.
fn insertion_point(text: &str, hunk: &Hunk) -> Result<usize, MatchError> {
    match (&hunk.header, hunk.insert_after) {
        (Some(h), _) => unique_line(text, h).map(|r| r.end).ok_or_else(not_found),
        (None, Some(n)) => offset_after_line(text, n).ok_or_else(not_found),
        (None, None) => Ok(text.len()),
    }
}

fn locate_hunk(
    text: &str,
    old: &str,
    header: Option<&str>,
) -> Result<super::tolerant::Located, MatchError> {
    match locate_lines(text, old) {
        // A header line that occurs once narrows an otherwise ambiguous hunk to the text
        // from there on.
        Err(e @ MatchError::Ambiguous { .. }) => {
            let scope = header.and_then(|h| unique_line(text, h));
            let Some(start) = scope.map(|r| r.start) else {
                return Err(e);
            };
            let mut loc = locate_lines(&text[start..], old).map_err(|_| e)?;
            loc.range = loc.range.start + start..loc.range.end + start;
            Ok(loc)
        }
        other => other,
    }
}

fn apply_hunks(
    path: &Path,
    text: &str,
    hunks: &[Hunk],
) -> Result<(String, Vec<(usize, RepairStage)>), PatchError> {
    // Every line newline-terminated; the writer restores the file's own final-newline state.
    let mut work = text.to_string();
    if !work.is_empty() && !work.ends_with('\n') {
        work.push('\n');
    }
    let fail = |hunk: usize, error: MatchError| PatchError::Hunk {
        path: path.to_path_buf(),
        hunk,
        error,
    };

    let mut splices: Vec<Splice> = Vec::with_capacity(hunks.len());
    let mut repairs = Vec::new();
    for (idx, hunk) in hunks.iter().enumerate() {
        let n = idx + 1;
        let new = join_lines(&hunk.side(Tag::Add));
        if hunk.lines.iter().all(|(t, _)| *t == Tag::Add) {
            // Nothing to match against: a pure addition.
            let at = insertion_point(&work, hunk).map_err(|e| fail(n, e))?;
            splices.push(Splice {
                hunk: n,
                range: at..at,
                replacement: new,
            });
            continue;
        }
        let old = join_lines(&hunk.side(Tag::Remove));
        let loc = locate_hunk(&work, &old, hunk.header.as_deref()).map_err(|e| fail(n, e))?;
        let replacement = adapt_new_text(&old, &new, &work[loc.range.clone()], loc.stage);
        if loc.stage != RepairStage::Exact {
            repairs.push((n, loc.stage));
        }
        splices.push(Splice {
            hunk: n,
            range: loc.range,
            replacement,
        });
    }

    splices.sort_by_key(|s| (s.range.start, s.hunk));
    for pair in splices.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        if a.range.end > b.range.start {
            return Err(PatchError::Overlap {
                path: path.to_path_buf(),
                a: a.hunk.min(b.hunk),
                b: a.hunk.max(b.hunk),
            });
        }
    }
    for s in splices.iter().rev() {
        work.replace_range(s.range.clone(), &s.replacement);
    }
    Ok((work, repairs))
}
