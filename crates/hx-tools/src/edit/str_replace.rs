//! `str_replace`: locate `old` (tolerantly) and splice in `new`.

use std::path::Path;

use super::tolerant::{Located, RepairStage, adapt_new_text, locate, locate_structural};

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
    splice(text, old, new, locate(text, old)?)
}

/// [`apply_staged`] for the file at `path`: when stages 1-4 find nothing, stage 5 (the
/// structural fallback, spec 7.9) may locate a symbol by the definition `old` starts with.
/// A structural result may still be rejected by the caller's syntax check.
pub fn apply_staged_for(
    path: &Path,
    text: &str,
    old: &str,
    new: &str,
) -> Result<(String, RepairStage), MatchError> {
    let loc = match locate(text, old) {
        Ok(loc) => loc,
        Err(e @ MatchError::NotFound { .. }) => match locate_structural(path, text, old)? {
            Some(loc) => loc,
            None => return Err(e),
        },
        Err(e) => return Err(e),
    };
    splice(text, old, new, loc)
}

fn splice(
    text: &str,
    old: &str,
    new: &str,
    loc: Located,
) -> Result<(String, RepairStage), MatchError> {
    let matched = &text[loc.range.clone()];
    let replacement = adapt_new_text(old, new, matched, loc.stage);
    let mut out = String::with_capacity(text.len() + replacement.len());
    out.push_str(&text[..loc.range.start]);
    out.push_str(&replacement);
    out.push_str(&text[loc.range.end..]);
    Ok((out, loc.stage))
}
