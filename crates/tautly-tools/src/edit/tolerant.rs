//! Tolerant applier stages 1-5 (spec 7.9).
//!
//! [`locate`] finds the single place the model's `old` text refers to. Stages run in
//! order and each needs exactly one candidate: none moves on to the next stage, more
//! than one is an error. Stage 5 ([`locate_structural`], str_replace only) is tried by the
//! caller after these fail.
//!
//! Stages 2 and 3 share one candidate search: two blocks equal after removing common
//! indentation are also equal after trimming every line, so an indentation-relative
//! match can never succeed where the whitespace-insensitive one found nothing. The
//! single window is therefore labelled `Indentation` (with the offset to re-indent the
//! replacement by) when the model's indentation differs from the file's by one uniform
//! offset, and `Whitespace` otherwise.

use std::ops::Range;
use std::path::Path;
use std::time::Duration;

use similar::TextDiff;
use tautly_index::Lang;

use super::str_replace::MatchError;

/// Lines of the nearest window shown on failure.
const MAX_WINDOW_LINES: usize = 20;
/// Interior line similarity needed for an anchor match.
const ANCHOR_SIMILARITY: f32 = 0.9;
/// Window similarity needed to be offered as a candidate location.
const CANDIDATE_SCORE: f32 = 0.6;
const MAX_CANDIDATES: usize = 5;
/// Symbol similarity needed for the structural fallback.
const STRUCTURAL_SIMILARITY: f32 = 0.8;
/// Longest a similarity comparison may run before settling for an approximate ratio.
const SIMILARITY_TIMEOUT: Duration = Duration::from_millis(200);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepairStage {
    Exact,
    Normalized,
    Whitespace,
    Indentation,
    Anchor,
    Structural,
}

/// As its [`name`](RepairStage::name).
impl serde::Serialize for RepairStage {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.name())
    }
}

impl RepairStage {
    /// Every stage, in the order they are tried.
    pub const ALL: [RepairStage; 6] = [
        RepairStage::Exact,
        RepairStage::Normalized,
        RepairStage::Whitespace,
        RepairStage::Indentation,
        RepairStage::Anchor,
        RepairStage::Structural,
    ];

    /// A stable name, as `ToolMeta::repair` is persisted and sent.
    pub fn name(self) -> &'static str {
        match self {
            RepairStage::Exact => "exact",
            RepairStage::Normalized => "normalized",
            RepairStage::Whitespace => "whitespace",
            RepairStage::Indentation => "indentation",
            RepairStage::Anchor => "anchor",
            RepairStage::Structural => "structural",
        }
    }

    /// Human-readable name, as in `applied via indentation-relative match`.
    pub fn describe(self) -> &'static str {
        match self {
            RepairStage::Exact => "exact match",
            RepairStage::Normalized => "normalized-text match",
            RepairStage::Whitespace => "whitespace-insensitive match",
            RepairStage::Indentation => "indentation-relative match",
            RepairStage::Anchor => "anchor match",
            RepairStage::Structural => "structural match",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Located {
    /// Byte range in the searched text to replace.
    pub range: Range<usize>,
    pub stage: RepairStage,
    /// Set when the model's block is indented by one uniform number of columns more
    /// (negative: fewer) than the file's matched lines. [`adapt_new_text`] already applies
    /// this to the replacement, so it is informational.
    pub reindent: Option<isize>,
}

/// Find the one place `old` refers to in `text` (LF line endings), trying stages 1-4.
pub fn locate(text: &str, old: &str) -> Result<Located, MatchError> {
    locate_with(text, old, false)
}

/// Like [`locate`], but the exact and normalized stages only accept matches that cover
/// whole lines, so a block of lines (a patch hunk) can never match inside a longer line
/// and the indentation stages still get their turn when the file indents more.
pub fn locate_lines(text: &str, old: &str) -> Result<Located, MatchError> {
    locate_with(text, old, true)
}

fn locate_with(text: &str, old: &str, whole_lines: bool) -> Result<Located, MatchError> {
    if old.is_empty() {
        return Err(no_match());
    }
    // Exact match first (spec 5.4 step 2).
    if let Some(range) = unique(text, find_hits(text, old, whole_lines), old.len())? {
        return Ok(exact(range, RepairStage::Exact));
    }
    // Stage 1: normalize the model's text and retry.
    let norm = normalize_old(old).text;
    if norm.trim().is_empty() {
        return Err(no_match());
    }
    if norm != old
        && let Some(range) = unique(text, find_hits(text, &norm, whole_lines), norm.len())?
    {
        return Ok(exact(range, RepairStage::Normalized));
    }

    let file = Lines::new(text);
    let want = Lines::new(&norm);
    let (n, m) = (file.len(), want.len());
    let had_newline = norm.ends_with('\n');

    // Stages 2 and 3: whole-line windows equal after trimming.
    if m <= n {
        let hits: Vec<usize> = (0..=n - m)
            .filter(|&s| (0..m).all(|i| file.line(s + i).trim() == want.line(i).trim()))
            .collect();
        match hits.as_slice() {
            [] => {}
            &[s] => {
                let offset = uniform_offset(&file, s, &want);
                return Ok(Located {
                    range: file.start(s)..file.end_of(s + m - 1, had_newline),
                    stage: if offset.is_some() {
                        RepairStage::Indentation
                    } else {
                        RepairStage::Whitespace
                    },
                    reindent: offset,
                });
            }
            many => {
                return Err(MatchError::Ambiguous {
                    lines: many.iter().map(|s| s + 1).collect(),
                });
            }
        }
    }

    // Stage 4: unique first and last lines, near-identical interior.
    if m >= 3 {
        let (first, last) = (want.line(0).trim(), want.line(m - 1).trim());
        if !first.is_empty() && !last.is_empty() {
            let firsts: Vec<usize> = (0..n).filter(|&i| file.line(i).trim() == first).collect();
            let lasts: Vec<usize> = (0..n).filter(|&i| file.line(i).trim() == last).collect();
            if let (&[i], &[j]) = (firsts.as_slice(), lasts.as_slice()) {
                // The interiors must have the same number of lines: `new` was written for
                // the model's block, so replacing a longer file block would delete the lines
                // the model left out (and a shorter one would duplicate its extra lines).
                if j > i
                    && j - i == m - 1
                    && interior_similarity(&file, i + 1..j, &want, 1..m - 1) >= ANCHOR_SIMILARITY
                {
                    return Ok(Located {
                        range: file.start(i)..file.end_of(j, had_newline),
                        stage: RepairStage::Anchor,
                        reindent: None,
                    });
                }
            }
        }
    }

    let (candidates, nearest) = nearest_windows(&file, &want);
    Err(MatchError::NotFound {
        candidates,
        nearest,
    })
}

/// Stage 5: the symbol the first definition line of `old` names, located in `text` by
/// parsing it (never a stale index). The symbol's lines are the range to replace when their
/// source resembles `old` (ratio >= [`STRUCTURAL_SIMILARITY`], and the non-blank lines pair up one-to-one, each
/// pair that alike) and exactly one same-named symbol does, provided `old` is exactly one symbol. `Ok(None)` when `old` is not exactly one definition
/// or nothing resembles it.
pub fn locate_structural(
    path: &Path,
    text: &str,
    old: &str,
) -> Result<Option<Located>, MatchError> {
    let Some(lang) = Lang::from_path(path).filter(|l| l.has_symbols()) else {
        return Ok(None);
    };
    let norm = normalize_old(old).text;
    let model_lines: Vec<&str> = norm.split('\n').collect();
    let (Some(first), Some(last)) = (
        model_lines.iter().position(|l| !l.trim().is_empty()),
        model_lines.iter().rposition(|l| !l.trim().is_empty()),
    ) else {
        return Ok(None);
    };
    // Parse the model's block on its own, without the indentation it was nested in.
    let wanted = tautly_index::extract(lang, &dedent(&norm));
    let Some(def) = wanted
        .symbols
        .iter()
        .filter(|s| s.start_line as usize == first + 1)
        .max_by_key(|s| s.end_line)
    else {
        return Ok(None);
    };
    // The block must be exactly one symbol: replacing the symbol's lines with new text
    // written for a longer or shorter block would drop or duplicate code. A trailing
    // neighbour is a further symbol starting after the first one ends.
    if def.end_line as usize != last + 1
        || wanted.symbols.iter().any(|s| s.start_line > def.end_line)
    {
        return Ok(None);
    }
    let want_text = squash(&norm);
    let want_lines: Vec<&str> = model_lines
        .iter()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect();

    let file = Lines::new(text);
    let hits: Vec<(usize, usize)> = tautly_index::extract(lang, text)
        .symbols
        .iter()
        .filter(|s| s.name == def.name)
        .map(|s| (s.start_line as usize - 1, s.end_line as usize - 1))
        .filter(|&(start, end)| end < file.len() && start <= end)
        .filter(|&(start, end)| {
            let source: Vec<&str> = (start..=end).map(|i| file.line(i)).collect();
            // A truncated block can still parse as a complete shorter symbol (Python), and a
            // repeated line can fool an end check, so the blocks must line up one-to-one:
            // same number of non-blank lines, each pair alike, so a typo repairs but any
            // missing or extra line does not.
            let theirs: Vec<&str> = source
                .iter()
                .map(|l| l.trim())
                .filter(|l| !l.is_empty())
                .collect();
            theirs.len() == want_lines.len()
                && theirs
                    .iter()
                    .zip(&want_lines)
                    .all(|(a, b)| similarity(a, b) >= STRUCTURAL_SIMILARITY)
                && similarity(&squash(&source.join("\n")), &want_text) >= STRUCTURAL_SIMILARITY
        })
        .collect();
    match hits.as_slice() {
        [] => Ok(None),
        &[(start, end)] => Ok(Some(Located {
            range: file.start(start)..file.end_of(end, norm.ends_with('\n')),
            stage: RepairStage::Structural,
            reindent: None,
        })),
        many => Err(MatchError::Ambiguous {
            lines: many.iter().map(|(start, _)| start + 1).collect(),
        }),
    }
}

/// Lines trimmed, blank lines dropped: what similarity is measured on.
fn squash(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn similarity(a: &str, b: &str) -> f32 {
    TextDiff::configure()
        .timeout(SIMILARITY_TIMEOUT)
        .diff_chars(a, b)
        .ratio()
}

/// `text` with the indentation common to all its non-blank lines removed.
fn dedent(text: &str) -> String {
    let common = text
        .split('\n')
        .filter(|l| !l.trim().is_empty())
        .map(|l| leading_ws_len(l).min(l.len()))
        .min()
        .unwrap_or(0);
    text.split('\n')
        .map(|l| l.get(common..).unwrap_or(l.trim_start()))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The replacement text to splice in for a match found at `stage`. The model's text is
/// brought in line with how its `old` text was repaired: CRLF always becomes LF (the
/// file is matched and edited as LF), copied line-number artifacts and BOMs are stripped
/// when they were stripped from `old`, typographic quotes/dashes become ASCII when `old`
/// had them but the matched file text has none, and for the line-based stages the block
/// is re-indented to where it sits in the file ([`reindent_to_match`]).
///
/// After any repair, a `new` text with copied artifacts on some lines but not all is
/// refused ([`MatchError::MixedArtifacts`]): the stripped lines and the unmarked ones cannot
/// be told apart from text that merely starts with digits. An exact match keeps `new` as
/// written (the model's `old` text was the file's own text, so `new` is too).
pub fn adapt_new_text(
    old: &str,
    new: &str,
    matched: &str,
    stage: RepairStage,
) -> Result<String, MatchError> {
    let mut out = new.replace("\r\n", "\n");
    if stage == RepairStage::Exact {
        return Ok(out);
    }
    let norm = normalize_old(old);
    match strip_artifacts(&out) {
        Some(stripped) if norm.stripped_artifacts => out = stripped,
        Some(_) => {}
        None if has_artifact_lines(&out) => return Err(MatchError::MixedArtifacts),
        None => {}
    }
    if old.contains('\u{FEFF}') {
        out = out.replace('\u{FEFF}', "");
    }
    if old.chars().any(is_typographic) && !matched.chars().any(is_typographic) {
        out = fold_typography(&out);
    }
    if matches!(
        stage,
        RepairStage::Whitespace
            | RepairStage::Indentation
            | RepairStage::Anchor
            | RepairStage::Structural
    ) {
        out = reindent_to_match(&out, &norm.text, matched);
    }
    Ok(out)
}

/// The 1-based first and last lines of `text` that the byte `range` covers (a newline that
/// ends the range belongs to its last line).
pub fn line_span(text: &str, range: &Range<usize>) -> (usize, usize) {
    let first = text[..range.start].matches('\n').count() + 1;
    let end = if range.end > range.start && text.as_bytes()[range.end - 1] == b'\n' {
        range.end - 1
    } else {
        range.end
    };
    let last = first
        + text[range.start..end.max(range.start)]
            .matches('\n')
            .count();
    (first, last)
}

/// Columns a tab counts for when comparing indentation.
const TAB_COLS: usize = 4;

fn leading_ws(line: &str) -> &str {
    &line[..leading_ws_len(line)]
}

fn indent_cols(ws: &str) -> usize {
    ws.chars()
        .map(|c| if c == '\t' { TAB_COLS } else { 1 })
        .sum()
}

fn first_content_line(text: &str) -> Option<&str> {
    text.split('\n').find(|l| !l.trim().is_empty())
}

/// Whether the first indented non-blank line among `texts` is tab-indented.
fn indents_with_tabs(texts: &[&str]) -> Option<bool> {
    texts
        .iter()
        .flat_map(|t| t.split('\n'))
        .filter(|l| !l.trim().is_empty())
        .map(leading_ws)
        .find(|ws| !ws.is_empty())
        .map(|ws| ws.starts_with('\t'))
}

/// The smallest step between distinct indentation widths used in `texts`.
fn indent_unit(texts: &[&str]) -> Option<usize> {
    let mut widths: Vec<usize> = texts
        .iter()
        .flat_map(|t| t.split('\n'))
        .filter(|l| !l.trim().is_empty())
        .map(|l| indent_cols(leading_ws(l)))
        .collect();
    widths.sort_unstable();
    widths.dedup();
    widths.windows(2).map(|w| w[1] - w[0]).min()
}

/// Re-indent `new` (the model's replacement, indented as the model believed `old_norm`
/// to be) to where `matched` sits in the file. Each line keeps its indentation relative
/// to the first line of the model's `old` block, applied to the indentation of the
/// file's first matched line, and is written in the file's style: tabs or spaces,
/// converting whole indentation levels when the model used the other one.
pub fn reindent_to_match(new: &str, old_norm: &str, matched: &str) -> String {
    let (Some(old_first), Some(file_first)) =
        (first_content_line(old_norm), first_content_line(matched))
    else {
        return new.to_string();
    };
    let old_base = indent_cols(leading_ws(old_first)) as isize;
    let file_base = indent_cols(leading_ws(file_first));
    let model_tabs = indents_with_tabs(&[old_norm, new]);
    let file_tabs = indents_with_tabs(&[matched]);
    // With no indentation anywhere in the file block, follow the model's style.
    let to_tabs = file_tabs.or(model_tabs).unwrap_or(false);
    let convert = model_tabs.is_some_and(|m| m != to_tabs);
    let model_unit = match model_tabs {
        Some(true) => TAB_COLS,
        _ => indent_unit(&[old_norm, new]).unwrap_or(TAB_COLS),
    };
    let file_unit = if to_tabs {
        TAB_COLS
    } else {
        indent_unit(&[matched]).unwrap_or(model_unit)
    };

    new.split('\n')
        .map(|line| {
            if line.trim().is_empty() {
                return line.to_string();
            }
            let rel = indent_cols(leading_ws(line)) as isize - old_base;
            let cols = if convert {
                // Whole levels: model levels become the file's level width.
                let levels = (rel as f64 / model_unit as f64).round() as isize;
                file_base as isize + levels * file_unit as isize
            } else {
                file_base as isize + rel
            };
            let cols = cols.max(0) as usize;
            let indent = if to_tabs {
                format!(
                    "{}{}",
                    "\t".repeat(cols / TAB_COLS),
                    " ".repeat(cols % TAB_COLS)
                )
            } else {
                " ".repeat(cols)
            };
            format!("{indent}{}", &line[leading_ws_len(line)..])
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ------------------------------------------------------------------ exact stages

fn exact(range: Range<usize>, stage: RepairStage) -> Located {
    Located {
        range,
        stage,
        reindent: None,
    }
}

/// Byte offsets of every (overlapping) occurrence of `pat` in `text`.
fn find_all(text: &str, pat: &str) -> Vec<usize> {
    let mut hits = Vec::new();
    let mut from = 0;
    while let Some(i) = text[from..].find(pat) {
        let at = from + i;
        hits.push(at);
        from = at + text[at..].chars().next().map_or(1, char::len_utf8);
    }
    hits
}

/// [`find_all`], keeping only hits that start and end on line boundaries when asked.
fn find_hits(text: &str, pat: &str, whole_lines: bool) -> Vec<usize> {
    let mut hits = find_all(text, pat);
    if whole_lines {
        let bytes = text.as_bytes();
        hits.retain(|&at| {
            let end = at + pat.len();
            (at == 0 || bytes[at - 1] == b'\n') && (end == bytes.len() || bytes[end - 1] == b'\n')
        });
    }
    hits
}

/// `Ok(Some(range))` for exactly one hit, `Ok(None)` for none, an ambiguity error for more.
fn unique(text: &str, hits: Vec<usize>, len: usize) -> Result<Option<Range<usize>>, MatchError> {
    match hits.as_slice() {
        [] => Ok(None),
        &[at] => Ok(Some(at..at + len)),
        many => Err(MatchError::Ambiguous {
            lines: many
                .iter()
                .map(|&at| text[..at].matches('\n').count() + 1)
                .collect(),
        }),
    }
}

// ------------------------------------------------------------------ normalization

struct Normalized {
    text: String,
    stripped_artifacts: bool,
}

/// Stage 1: CRLF to LF, BOMs dropped (the file is matched without its BOM), typographic characters to ASCII, trailing whitespace stripped
/// per line, `read` artifacts removed.
fn normalize_old(old: &str) -> Normalized {
    let text = fold_typography(&old.replace("\r\n", "\n").replace('\u{FEFF}', ""));
    let text = text
        .split('\n')
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n");
    match strip_artifacts(&text) {
        Some(text) => Normalized {
            text,
            stripped_artifacts: true,
        },
        None => Normalized {
            text,
            stripped_artifacts: false,
        },
    }
}

fn is_typographic(c: char) -> bool {
    matches!(
        c,
        '\u{2018}'..='\u{201F}' | '\u{2010}'..='\u{2015}' | '\u{2212}' | '\u{00A0}'
    )
}

fn fold_typography(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '\u{2018}'..='\u{201B}' => '\'',
            '\u{201C}'..='\u{201F}' => '"',
            '\u{2010}'..='\u{2015}' | '\u{2212}' => '-',
            '\u{00A0}' => ' ',
            c => c,
        })
        .collect()
}

#[derive(PartialEq)]
enum Artifact {
    /// `<n>\t` (or a bare `<n>` once its tab was stripped as trailing whitespace).
    Number { has_tab: bool },
    /// `<n>:<4 hex>|`
    Hash,
}

/// Length of a leading `read` artifact on `line`, if it has one.
fn artifact(line: &str) -> Option<(usize, Artifact)> {
    let digits = line.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    let rest = &line.as_bytes()[digits..];
    match rest.first() {
        None => Some((digits, Artifact::Number { has_tab: false })),
        Some(b'\t') => Some((digits + 1, Artifact::Number { has_tab: true })),
        Some(b':') if rest.len() >= 6 && rest[5] == b'|' => rest[1..5]
            .iter()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
            .then_some((digits + 6, Artifact::Hash)),
        _ => None,
    }
}

/// Whether any line of `text` starts with an unambiguous copied artifact: a tabbed line
/// number or a hashline anchor (a bare number is ordinary text).
fn has_artifact_lines(text: &str) -> bool {
    text.split('\n').any(|l| {
        matches!(
            artifact(l),
            Some((_, Artifact::Number { has_tab: true } | Artifact::Hash))
        )
    })
}

/// Remove copied `<n>\t` line numbers or `<n>:<hash>|` anchors, but only when every line
/// carries one (so ordinary text that merely starts with digits is left alone).
fn strip_artifacts(text: &str) -> Option<String> {
    let mut lines: Vec<&str> = text.split('\n').collect();
    let trailing_empty = text.ends_with('\n');
    if trailing_empty {
        lines.pop();
    }
    let found: Vec<(usize, Artifact)> = lines.iter().map(|l| artifact(l)).collect::<Option<_>>()?;
    let numbers = found
        .iter()
        .all(|(_, a)| matches!(a, Artifact::Number { .. }));
    let tabbed = found
        .iter()
        .any(|(_, a)| *a == Artifact::Number { has_tab: true });
    let hashes = found.iter().all(|(_, a)| *a == Artifact::Hash);
    if found.is_empty() || !(hashes || (numbers && tabbed)) {
        return None;
    }
    let mut out: Vec<&str> = lines
        .iter()
        .zip(&found)
        .map(|(l, (len, _))| &l[*len..])
        .collect();
    if trailing_empty {
        out.push("");
    }
    Some(out.join("\n"))
}

// ------------------------------------------------------------------ line tables

/// Line boundaries of a text; a final newline does not start another line.
struct Lines<'a> {
    text: &'a str,
    spans: Vec<Range<usize>>,
}

impl<'a> Lines<'a> {
    fn new(text: &'a str) -> Self {
        let mut spans = Vec::new();
        let mut start = 0;
        for piece in text.split('\n') {
            spans.push(start..start + piece.len());
            start += piece.len() + 1;
        }
        if text.ends_with('\n') || text.is_empty() {
            spans.pop();
        }
        Self { text, spans }
    }

    fn len(&self) -> usize {
        self.spans.len()
    }

    fn line(&self, i: usize) -> &'a str {
        &self.text[self.spans[i].clone()]
    }

    fn start(&self, i: usize) -> usize {
        self.spans[i].start
    }

    /// End of line `i`, including its newline when `with_newline` and one follows.
    fn end_of(&self, i: usize, with_newline: bool) -> usize {
        let end = self.spans[i].end;
        if with_newline && end < self.text.len() {
            end + 1
        } else {
            end
        }
    }
}

fn leading_ws_len(line: &str) -> usize {
    line.len() - line.trim_start_matches([' ', '\t']).len()
}

/// The indentation offset, in columns, of the file window at `start` relative to the
/// model's block, if every non-blank line differs by the same non-zero amount.
fn uniform_offset(file: &Lines<'_>, start: usize, want: &Lines<'_>) -> Option<isize> {
    let mut offset = None;
    for i in 0..want.len() {
        let (model, actual) = (want.line(i), file.line(start + i));
        if model.trim().is_empty() {
            continue;
        }
        let d = indent_cols(leading_ws(actual)) as isize - indent_cols(leading_ws(model)) as isize;
        if *offset.get_or_insert(d) != d {
            return None;
        }
    }
    offset.filter(|&d| d != 0)
}

// ------------------------------------------------------------------ anchor stage

fn interior_similarity(
    file: &Lines<'_>,
    file_range: Range<usize>,
    want: &Lines<'_>,
    want_range: Range<usize>,
) -> f32 {
    let join = |lines: &Lines<'_>, range: Range<usize>| -> String {
        range
            .map(|i| format!("{}\n", lines.line(i).trim()))
            .collect()
    };
    TextDiff::from_lines(&join(file, file_range), &join(want, want_range)).ratio()
}

// ------------------------------------------------------------------ nearest window

fn line_similarity(a: &str, b: &str) -> f32 {
    let (a, b) = (a.trim(), b.trim());
    if a == b {
        return 1.0;
    }
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    TextDiff::from_chars(a, b).ratio()
}

/// Start lines of windows that resemble the model's text, and the single best one with
/// its line-numbered content (at most [`MAX_WINDOW_LINES`] lines).
fn nearest_windows(file: &Lines<'_>, want: &Lines<'_>) -> (Vec<usize>, Option<(usize, String)>) {
    let w = want.len().min(MAX_WINDOW_LINES).min(file.len());
    if w == 0 {
        return (Vec::new(), None);
    }
    let mut scores: Vec<(usize, f32)> = (0..=file.len() - w)
        .map(|s| {
            let sum: f32 = (0..w)
                .map(|i| line_similarity(file.line(s + i), want.line(i)))
                .sum();
            (s, sum / w as f32)
        })
        .collect();
    scores.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));

    let mut candidates: Vec<usize> = Vec::new();
    for &(s, score) in &scores {
        if score < CANDIDATE_SCORE || candidates.len() == MAX_CANDIDATES {
            break;
        }
        if candidates.iter().all(|&c| c.abs_diff(s) >= w) {
            candidates.push(s);
        }
    }
    candidates.sort_unstable();

    // Always offered (spec 7.9), however weak the resemblance.
    let nearest = scores.first().map(|&(s, _)| {
        let shown = (s..s + w)
            .map(|i| format!("{}\t{}", i + 1, file.line(i)))
            .collect::<Vec<_>>()
            .join("\n");
        (s + 1, shown)
    });
    (candidates.into_iter().map(|s| s + 1).collect(), nearest)
}

fn no_match() -> MatchError {
    MatchError::NotFound {
        candidates: Vec::new(),
        nearest: None,
    }
}
