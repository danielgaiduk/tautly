//! Tolerant applier stages 1-4 (spec 7.9).
//!
//! [`locate`] finds the single place the model's `old` text refers to. Stages run in
//! order and each needs exactly one candidate: none moves on to the next stage, more
//! than one is an error. Stage 5 (structural, via the code index) is added separately.
//!
//! Stages 2 and 3 share one candidate search: two blocks equal after removing common
//! indentation are also equal after trimming every line, so an indentation-relative
//! match can never succeed where the whitespace-insensitive one found nothing. The
//! single window is therefore labelled `Indentation` (with the offset to re-indent the
//! replacement by) when the model's indentation differs from the file's by one uniform
//! offset, and `Whitespace` otherwise.

use std::ops::Range;

use similar::TextDiff;

use super::str_replace::MatchError;

/// Lines of the nearest window shown on failure.
const MAX_WINDOW_LINES: usize = 20;
/// Interior line similarity needed for an anchor match.
const ANCHOR_SIMILARITY: f32 = 0.9;
/// Window similarity needed to be offered as a candidate location.
const CANDIDATE_SCORE: f32 = 0.6;
/// Window similarity needed to be shown as the nearest window.
const NEAREST_SCORE: f32 = 0.3;
const MAX_CANDIDATES: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepairStage {
    Exact,
    Normalized,
    Whitespace,
    Indentation,
    Anchor,
    Structural,
}

impl RepairStage {
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
    /// Indentation columns to add (or, when negative, remove) on every non-blank line of
    /// the replacement so it fits where the match was found.
    pub reindent: Option<isize>,
}

/// Find the one place `old` refers to in `text` (LF line endings), trying stages 1-4.
pub fn locate(text: &str, old: &str) -> Result<Located, MatchError> {
    if old.is_empty() {
        return Err(no_match());
    }
    // Exact match first (spec 5.4 step 2).
    if let Some(range) = unique(text, find_all(text, old), old.len())? {
        return Ok(exact(range, RepairStage::Exact));
    }
    // Stage 1: normalize the model's text and retry.
    let norm = normalize_old(old).text;
    if norm.trim().is_empty() {
        return Err(no_match());
    }
    if norm != old {
        if let Some(range) = unique(text, find_all(text, &norm), norm.len())? {
            return Ok(exact(range, RepairStage::Normalized));
        }
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
                let first = file.start(s);
                // Re-indented replacements carry their own indentation; otherwise the
                // first line's indentation stays in place.
                let start = if offset.is_some() {
                    first
                } else {
                    first + leading_ws_len(file.line(s))
                };
                return Ok(Located {
                    range: start..file.end_of(s + m - 1, had_newline),
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
                if j > i
                    && interior_similarity(&file, i + 1..j, &want, 1..m - 1) >= ANCHOR_SIMILARITY
                {
                    return Ok(Located {
                        range: file.start(i) + leading_ws_len(file.line(i))
                            ..file.end_of(j, had_newline),
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

/// The replacement text to splice in for a match found at `stage`. The model's text is
/// brought in line with how its `old` text was repaired: CRLF always becomes LF (the
/// file is matched and edited as LF), copied line-number artifacts are stripped when
/// they were stripped from `old`, and typographic quotes/dashes become ASCII when `old`
/// had them but the matched file text has none.
pub fn adapt_new_text(old: &str, new: &str, matched: &str, stage: RepairStage) -> String {
    let mut out = new.replace("\r\n", "\n");
    if stage == RepairStage::Exact {
        return out;
    }
    if normalize_old(old).stripped_artifacts {
        if let Some(stripped) = strip_artifacts(&out) {
            out = stripped;
        }
    }
    if old.chars().any(is_typographic) && !matched.chars().any(is_typographic) {
        out = fold_typography(&out);
    }
    out
}

/// Shift the indentation of every non-blank line of `text` by `offset` columns. Added
/// indentation uses the character (tab or space) the `matched` file text indents with.
pub fn reindent(text: &str, offset: isize, matched: &str) -> String {
    let pad = matched
        .lines()
        .find(|l| !l.trim().is_empty())
        .and_then(|l| l.chars().next())
        .filter(|c| *c == '\t')
        .unwrap_or(' ');
    let amount = offset.unsigned_abs();
    text.split('\n')
        .map(|line| {
            if line.trim().is_empty() {
                line.to_string()
            } else if offset > 0 {
                format!("{}{line}", pad.to_string().repeat(amount))
            } else {
                let strip = leading_ws_len(line).min(amount);
                line[strip..].to_string()
            }
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

/// Stage 1: CRLF to LF, typographic characters to ASCII, trailing whitespace stripped
/// per line, `read` artifacts removed.
fn normalize_old(old: &str) -> Normalized {
    let text = fold_typography(&old.replace("\r\n", "\n"));
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

/// The indentation offset, in columns, between the model's block and the file window at
/// `start`, if every non-blank line differs by the same non-zero amount.
fn uniform_offset(file: &Lines<'_>, start: usize, want: &Lines<'_>) -> Option<isize> {
    let mut offset = None;
    for i in 0..want.len() {
        let (old, new) = (want.line(i), file.line(start + i));
        if old.trim().is_empty() {
            continue;
        }
        let d = leading_ws_len(new) as isize - leading_ws_len(old) as isize;
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

    let nearest = scores
        .first()
        .filter(|(_, score)| *score >= NEAREST_SCORE)
        .map(|&(s, _)| {
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
