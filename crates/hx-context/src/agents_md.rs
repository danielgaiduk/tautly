//! AGENTS.md discovery and auto-pinning of `Constraints` / `Rules` sections.

use std::path::Path;

use crate::pinned::{self, PinSource, Pinned};

const FILE_NAME: &str = "AGENTS.md";

/// AGENTS.md files from `root` up to the filesystem root, concatenated outermost first (the
/// nearest file is last), and a pin for each `Constraints` / `Rules` section. Unreadable or
/// empty files are skipped.
pub fn load(root: &Path) -> (Option<String>, Vec<Pinned>) {
    // Normalize first so `.` / `a/..` do not yield the same directory under several names.
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let mut found: Vec<String> = root
        .ancestors()
        .filter_map(|dir| std::fs::read_to_string(dir.join(FILE_NAME)).ok())
        .filter(|text| !text.trim().is_empty())
        .collect();
    found.reverse();
    let mut pins = Vec::new();
    for body in found.iter().flat_map(|text| pinned_sections(text)) {
        pinned::add(&mut pins, Pinned::new(body, PinSource::AgentsMd));
    }
    let text = (!found.is_empty()).then(|| {
        found
            .iter()
            .map(|t| t.trim())
            .collect::<Vec<_>>()
            .join("\n\n")
    });
    (text, pins)
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
