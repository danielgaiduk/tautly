//! AGENTS.md discovery and auto-pinning of `Constraints` / `Rules` sections.

use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use hx_sandbox::{Sandbox, SandboxError, is_protected};

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
/// Bytes of all pinned `Rules` / `Constraints` sections together; each one is also capped
/// at [`pinned::MAX_PIN_BYTES`]. The full sections stay in the `<agents_md>` text.
pub const MAX_PINNED_BYTES: usize = 4 * 1024;

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
/// - An `AGENTS.md` above the root may be a link only to a file under the directory that
///   holds it (stow-style `~/AGENTS.md -> ~/dotfiles/AGENTS.md`): when hx starts in a
///   subdirectory of a clone, the clone's top-level file is just as repository-controlled.
/// - An `AGENTS.md` above the root must also look like the user's own: the file owned by
///   the current user (or root) and not writable by group or others, in a directory that
///   is not world-writable unless it is sticky (like `/tmp`) and the file is the current
///   user's. Otherwise anyone who can write there could plant instructions.
/// - A link whose target is a protected path (`.git/config`, `.hx/...`; relative to the
///   directory holding the file) is refused with a warning.
/// - Every file is opened `O_NOFOLLOW` (after resolving) and without blocking (a FIFO
///   cannot hang start-up), and must be a regular file (no devices).
/// - Each file is cut to [`MAX_FILE_BYTES`] and all of them together to
///   [`MAX_TOTAL_BYTES`], nearest files first; a cut file ends in
///   `[AGENTS.md truncated at N KB]` and adds a warning.
/// - Each pinned section is cut to [`pinned::MAX_PIN_BYTES`] and all of them together to
///   [`MAX_PINNED_BYTES`], nearest files first, with a marker and a warning.
pub fn load(sb: &dyn Sandbox) -> AgentsMd {
    let root = sb.root();
    let mut warnings = Vec::new();
    // Nearest first.
    let mut found: Vec<(PathBuf, String)> = Vec::new();
    match sb.resolve(Path::new(FILE_NAME)) {
        Ok(path) => {
            if !refuse_protected(root, &path, &mut warnings)
                && let Some(text) = read_capped(&path, None, &mut warnings)
            {
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
        let target = match std::fs::canonicalize(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                warnings.push(format!("{} could not be resolved: {e}", path.display()));
                continue;
            }
        };
        // `dir` is canonical: the ancestors of the canonical root.
        if !target.starts_with(dir) {
            warnings.push(format!(
                "{} links outside the directory that holds it; it was not loaded",
                path.display()
            ));
            continue;
        }
        if refuse_protected(dir, &target, &mut warnings) {
            continue;
        }
        if let Some(text) = read_capped(&target, Some(dir), &mut warnings) {
            found.push((path, text));
        }
    }

    // The total cap, spent on the nearest files first.
    let mut left = MAX_TOTAL_BYTES;
    let mut kept: Vec<(PathBuf, String)> = Vec::new();
    for (path, text) in found {
        if text.len() <= left {
            left -= text.len();
            kept.push((path, text));
        } else if left > 0 {
            kept.push((path.clone(), truncate(&text, left)));
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

    // Pins, spent on the nearest files first like the text; outermost first again after.
    let mut left = MAX_PINNED_BYTES;
    let mut pins: Vec<Pinned> = Vec::new();
    for (path, text) in &kept {
        let mut file_pins = Vec::new();
        for body in pinned_sections(text) {
            if pins
                .iter()
                .chain(&file_pins)
                .any(|p: &Pinned| p.text == body)
            {
                continue;
            }
            let cap = left.min(pinned::MAX_PIN_BYTES);
            if cap == 0 {
                warnings.push(format!(
                    "a Rules/Constraints section of {} was not pinned: pinned AGENTS.md \
                     sections are capped at {} KB together (it is still in the AGENTS.md text)",
                    path.display(),
                    MAX_PINNED_BYTES / 1024
                ));
                continue;
            }
            let body = if body.len() > cap {
                warnings.push(format!(
                    "a Rules/Constraints section of {} was pinned only in part ({cap} bytes; \
                     the full section is in the AGENTS.md text)",
                    path.display()
                ));
                cut_pin(&body, cap)
            } else {
                body
            };
            left = left.saturating_sub(body.len());
            file_pins.push(Pinned::new(body, PinSource::AgentsMd));
        }
        pins.splice(0..0, file_pins);
    }
    kept.reverse();
    let text = (!kept.is_empty()).then(|| {
        kept.iter()
            .map(|(_, t)| t.trim())
            .collect::<Vec<_>>()
            .join("\n\n")
    });
    AgentsMd {
        text,
        pins,
        warnings,
    }
}

/// Whether `target` (resolved, under `base`) is a protected path relative to `base`; if so
/// a warning says it was refused.
fn refuse_protected(base: &Path, target: &Path, warnings: &mut Vec<String>) -> bool {
    let Ok(rel) = target.strip_prefix(base) else {
        return false;
    };
    if !is_protected(rel) {
        return false;
    }
    warnings.push(format!(
        "{} resolves to the protected path {}; it was not loaded",
        base.join(FILE_NAME).display(),
        rel.display()
    ));
    true
}

/// Why an AGENTS.md above the root (`meta` of the open file, held in `dir`) is not
/// trusted as the user's own, or None when it is.
fn ancestor_untrusted(meta: &std::fs::Metadata, dir: &Path) -> Option<String> {
    // SAFETY: geteuid has no preconditions and cannot fail.
    let me = unsafe { libc::geteuid() };
    if meta.uid() != me && meta.uid() != 0 {
        return Some(format!("is owned by another user (uid {})", meta.uid()));
    }
    if meta.mode() & 0o022 != 0 {
        return Some("is writable by group or others".into());
    }
    let dir_mode = match std::fs::metadata(dir) {
        Ok(m) => m.mode(),
        Err(e) => return Some(format!("is in a directory that cannot be checked ({e})")),
    };
    if dir_mode & 0o002 != 0 && (dir_mode & 0o1000 == 0 || meta.uid() != me) {
        return Some(
            "is in a world-writable directory that is not sticky or the file is not yours".into(),
        );
    }
    None
}

/// `body` cut at a character boundary to at most `max` bytes, with a marker.
fn cut_pin(body: &str, max: usize) -> String {
    let mut end = max.min(body.len());
    while !body.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n[pinned section truncated at {end} bytes; the full section is in <agents_md>]",
        body[..end].trim_end()
    )
}

/// The text of the AGENTS.md at `path` (already resolved), cut to [`MAX_FILE_BYTES`];
/// None when it is missing, empty or unusable (with a warning unless it is simply
/// missing). Opened `O_NOFOLLOW`, so a link swapped in after resolving is refused. Never
/// blocks on a FIFO, never reads a device. `ancestor`: the directory holding a file above
/// the root, whose ownership and modes [`ancestor_untrusted`] checks on the open file.
fn read_capped(path: &Path, ancestor: Option<&Path>, warnings: &mut Vec<String>) -> Option<String> {
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            warnings.push(format!("{} could not be opened: {e}", path.display()));
            return None;
        }
    };
    let meta = match file.metadata() {
        Ok(m) if m.is_file() => m,
        _ => {
            warnings.push(format!(
                "{} is not a regular file; it was not loaded",
                path.display()
            ));
            return None;
        }
    };
    if let Some(dir) = ancestor
        && let Some(why) = ancestor_untrusted(&meta, dir)
    {
        warnings.push(format!("{} {why}; it was not loaded", path.display()));
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
