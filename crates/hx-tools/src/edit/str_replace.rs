//! `str_replace`: locate `old` (tolerantly) and splice in `new`.

use super::tolerant::{RepairStage, adapt_new_text, locate, reindent};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MatchError {
    /// No stage matched. `candidates` are 1-based start lines of similar windows;
    /// `nearest` is the best window (start line, line-numbered text).
    NotFound {
        candidates: Vec<usize>,
        nearest: Option<(usize, String)>,
    },
    /// More than one match; 1-based start lines.
    Ambiguous { lines: Vec<usize> },
}

/// Replace the single place `old` matches in `text` with `new`.
pub fn apply(text: &str, old: &str, new: &str) -> Result<String, MatchError> {
    apply_staged(text, old, new).map(|(out, _)| out)
}

/// Like [`apply`], also reporting which stage located the match.
pub fn apply_staged(text: &str, old: &str, new: &str) -> Result<(String, RepairStage), MatchError> {
    let loc = locate(text, old)?;
    let matched = &text[loc.range.clone()];
    let mut replacement = adapt_new_text(old, new, matched, loc.stage);
    if let Some(offset) = loc.reindent {
        replacement = reindent(&replacement, offset, matched);
    } else if matches!(loc.stage, RepairStage::Whitespace | RepairStage::Anchor) {
        // These ranges start after the first line's indentation, which stays in place.
        replacement = replacement.trim_start_matches([' ', '\t']).to_string();
    }
    let mut out = String::with_capacity(text.len() + replacement.len());
    out.push_str(&text[..loc.range.start]);
    out.push_str(&replacement);
    out.push_str(&text[loc.range.end..]);
    Ok((out, loc.stage))
}
