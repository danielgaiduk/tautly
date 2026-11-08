//! `str_replace`: locate `old` (tolerantly) and splice in `new`.

use std::path::Path;

use super::tolerant::{Located, RepairStage, adapt_new_text, line_span, locate, locate_structural};

/// Why the text to replace could not be placed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MatchError {
    /// No stage matched. `candidates` are 1-based start lines of similar windows;
    /// `nearest` is the best window (start line, line-numbered text).
    NotFound {
        /// 1-based start lines of similar windows.
        candidates: Vec<usize>,
        /// The most similar window: start line and line-numbered text.
        nearest: Option<(usize, String)>,
    },
    /// More than one match; 1-based start lines.
    Ambiguous {
        /// The 1-based start line of each match.
        lines: Vec<usize>,
    },
    /// A repaired match whose `new` text carries copied `read` artifacts (line numbers or
    /// hashline anchors) on some lines but not all: stripping them would guess, keeping them
    /// would write them into the file.
    MixedArtifacts,
}

/// Replace the single place `old` matches in `text` with `new`.
pub fn apply(text: &str, old: &str, new: &str) -> Result<String, MatchError> {
    apply_staged(text, old, new).map(|(out, _)| out)
}

/// An edit located and spliced into the text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spliced {
    /// The new text.
    pub text: String,
    /// The stage that located the match.
    pub stage: RepairStage,
    /// 1-based first and last lines of the original text that were replaced.
    pub lines: (usize, usize),
}

/// Like [`apply`], also reporting which stage located the match.
pub fn apply_staged(text: &str, old: &str, new: &str) -> Result<(String, RepairStage), MatchError> {
    splice(text, old, new, locate(text, old)?).map(|s| (s.text, s.stage))
}

/// [`apply_staged`] for the file at `path`: when stages 1-4 find nothing, stage 5 (the
/// structural fallback, spec 7.9) may locate a symbol by the definition `old` starts with.
/// A structural result may still be rejected by the caller's syntax check.
pub fn apply_staged_for(
    path: &Path,
    text: &str,
    old: &str,
    new: &str,
) -> Result<Spliced, MatchError> {
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

fn splice(text: &str, old: &str, new: &str, loc: Located) -> Result<Spliced, MatchError> {
    let matched = &text[loc.range.clone()];
    let replacement = adapt_new_text(old, new, matched, loc.stage)?;
    let mut out = String::with_capacity(text.len() + replacement.len());
    out.push_str(&text[..loc.range.start]);
    out.push_str(&replacement);
    out.push_str(&text[loc.range.end..]);
    Ok(Spliced {
        text: out,
        stage: loc.stage,
        lines: line_span(text, &loc.range),
    })
}
