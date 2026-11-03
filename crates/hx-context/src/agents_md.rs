//! AGENTS.md discovery and auto-pinning of `Constraints` / `Rules` sections.

use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use hx_sandbox::{Sandbox, SandboxError};

use crate::pinned::{self, PinSource, Pinned};

const FILE_NAME: &str = "AGENTS.md";

/// What [`load`] found: the text for the `<agents_md>` block, the pins of its `Rules` /
/// `Constraints` sections, and warnings for the session's `ContextStatus`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentsMd {
    pub text: Option<String>,
    pub pins: Vec<Pinned>,
    pub warnings: Vec<String>,
}

/// Bytes kept of one AGENTS.md file.
pub const MAX_FILE_BYTES: usize = 32 * 1024;
/// Bytes kept of all AGENTS.md files together (about 16k tokens).
pub const MAX_TOTAL_BYTES: usize = 64 * 1024;

/// AGENTS.md files from the sandbox root up to the filesystem root, concatenated outermost
/// first (the nearest file is last), and a pin for each `Constraints` / `Rules` section.
/// Missing or empty files are skipped.
///
/// This runs at session start, before any tool call or approval, and its text goes into
/// the system prompt, so the repository's file is read like any confined path:
///
/// - The root's `AGENTS.md` is resolved through the sandbox (the policy's resolver), so a
///   link inside the repository (`AGENTS.md -> docs/agents.md`) works and a link out of it
///   is refused with a warning.
/// - Every file is opened without blocking (a FIFO cannot hang start-up) and must be a
///   regular file (no devices); the root's is opened `O_NOFOLLOW` after resolving.
///   Files in directories above the root are the user's own and may be links.
/// - Each file is cut to [`MAX_FILE_BYTES`] and all of them together to
///   [`MAX_TOTAL_BYTES`], nearest files first; a cut file ends in
///   `[AGENTS.md truncated at N KB]` and adds a warning.
pub fn load(sb: &dyn Sandbox) -> AgentsMd {
    let root = sb.root();
    let mut warnings = Vec::new();
    // Nearest first.
    let mut found: Vec<(PathBuf, String)> = Vec::new();
    match sb.resolve(Path::new(FILE_NAME)) {
        Ok(path) => {
            if let Some(text) = read_capped(&path, true, &mut warnings) {
                found.push((path, text));
            }
        }
        Err(SandboxError::OutsideRoot(_)) => warnings.push(format!(
            "{} links outside the project root; it was not loaded",
            root.join(FILE_NAME).display()
        )),
        Err(e) => warnings.push(format!(
            "{} could not be resolved ({e}); it was not loaded",
            root.join(FILE_NAME).display()
        )),
    }
    for dir in root.ancestors().skip(1) {
        let path = dir.join(FILE_NAME);
        if let Some(text) = read_capped(&path, false, &mut warnings) {
            found.push((path, text));
        }
    }

    // The total cap, spent on the nearest files first.
    let mut left = MAX_TOTAL_BYTES;
    let mut kept: Vec<String> = Vec::new();
    for (path, text) in found {
        if text.len() <= left {
            left -= text.len();
            kept.push(text);
        } else if left > 0 {
            kept.push(truncate(&text, left));
            left = 0;
            warnings.push(format!(
                "{} was truncated: all AGENTS.md files together are capped at {} KB",
                path.display(),
                MAX_TOTAL_BYTES / 1024
            ));
        } else {
            warnings.push(format!(
                "{} was not loaded: all AGENTS.md files together are capped at {} KB",
                path.display(),
                MAX_TOTAL_BYTES / 1024
            ));
        }
    }
    kept.reverse();

    let mut pins = Vec::new();
    for body in kept.iter().flat_map(|text| pinned_sections(text)) {
        pinned::add(&mut pins, Pinned::new(body, PinSource::AgentsMd));
    }
    let text = (!kept.is_empty()).then(|| {
        kept.iter()
            .map(|t| t.trim())
            .collect::<Vec<_>>()
            .join("\n\n")
    });
    AgentsMd {
        text,
        pins,
        warnings,
    }
}

/// The text of the AGENTS.md at `path`, cut to [`MAX_FILE_BYTES`]; None when it is
/// missing, empty or unusable (with a warning unless it is simply missing). `nofollow`
/// refuses a symlink at `path` itself. Never blocks on a FIFO, never reads a device.
fn read_capped(path: &Path, nofollow: bool, warnings: &mut Vec<String>) -> Option<String> {
    let mut flags = libc::O_NONBLOCK;
    if nofollow {
        flags |= libc::O_NOFOLLOW;
    }
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(flags)
        .open(path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            warnings.push(format!("{} could not be opened: {e}", path.display()));
            return None;
        }
    };
    let regular = file.metadata().is_ok_and(|m| m.is_file());
    if !regular {
        warnings.push(format!(
            "{} is not a regular file; it was not loaded",
            path.display()
        ));
        return None;
    }
    let mut bytes = Vec::new();
    if let Err(e) = file.take(MAX_FILE_BYTES as u64 + 1).read_to_end(&mut bytes) {
        warnings.push(format!("{} could not be read: {e}", path.display()));
        return None;
    }
    let over = bytes.len() > MAX_FILE_BYTES;
    if over {
        bytes.truncate(MAX_FILE_BYTES);
    }
    let text = match String::from_utf8(bytes) {
        Ok(t) => t,
        // The cut may split a character: keep what precedes it.
        Err(e) if over && e.utf8_error().error_len().is_none() => {
            let valid = e.utf8_error().valid_up_to();
            let mut bytes = e.into_bytes();
            bytes.truncate(valid);
            String::from_utf8(bytes).unwrap_or_default()
        }
        Err(_) => {
            warnings.push(format!(
                "{} is not UTF-8 text; it was not loaded",
                path.display()
            ));
            return None;
        }
    };
    if text.trim().is_empty() {
        return None;
    }
    if over {
        warnings.push(format!(
            "{} is over {} KB and was truncated",
            path.display(),
            MAX_FILE_BYTES / 1024
        ));
        return Some(format!("{text}\n\n{}", marker(MAX_FILE_BYTES)));
    }
    Some(text)
}

/// `text` cut at a character boundary to at most `max` bytes, ending in the marker.
fn truncate(text: &str, max: usize) -> String {
    let mut end = max.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n\n{}", text[..end].trim_end(), marker(max))
}

fn marker(bytes: usize) -> String {
    format!("[AGENTS.md truncated at {} KB]", bytes / 1024)
}

/// Bodies of the sections headed `Constraints` or `Rules` (any level, any case). A section
/// runs to the next heading of the same or a higher level; sub-sections belong to it.
/// Headings inside code fences do not count.
fn pinned_sections(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    // Level of the heading that opened the section being collected, and its lines so far.
    let mut open: Option<(usize, Vec<&str>)> = None;
    let mut fence: Option<&str> = None;
    let mut finish = |open: &mut Option<(usize, Vec<&str>)>| {
        if let Some((_, lines)) = open.take() {
            let body = lines.join("\n");
            if !body.trim().is_empty() {
                out.push(body.trim().to_string());
            }
        }
    };
    for line in text.lines() {
        let trimmed = line.trim_start();
        let marker = ["```", "~~~"].into_iter().find(|m| trimmed.starts_with(m));
        if let Some(m) = marker {
            match fence {
                None => fence = Some(m),
                Some(open_m) if open_m == m => fence = None,
                Some(_) => {}
            }
        } else if fence.is_none()
            && let Some((level, title)) = heading(line)
        {
            if open.as_ref().is_some_and(|(l, _)| level <= *l) {
                finish(&mut open);
            }
            if open.is_none() {
                let t = title.trim().to_ascii_lowercase();
                if t == "constraints" || t == "rules" {
                    open = Some((level, Vec::new()));
                }
                continue;
            }
        }
        if let Some((_, lines)) = open.as_mut() {
            lines.push(line);
        }
    }
    finish(&mut open);
    out
}

/// `(level, title)` of an ATX heading line (up to three leading spaces are allowed).
fn heading(line: &str) -> Option<(usize, &str)> {
    let indent = line.bytes().take_while(|&b| b == b' ').count();
    if indent > 3 {
        return None;
    }
    let line = &line[indent..];
    let level = line.bytes().take_while(|&b| b == b'#').count();
    if !(1..=6).contains(&level) {
        return None;
    }
    let rest = &line[level..];
    // A heading needs a space after the hashes (`#hashtag` is not one).
    rest.starts_with([' ', '\t'])
        .then(|| (level, rest.trim().trim_end_matches('#').trim()))
}
