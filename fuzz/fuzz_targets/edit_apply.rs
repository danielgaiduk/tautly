//! The edit tool's appliers. Input: `text \0 old \0 new \0 patch`, `\0`-separated. Never
//! panics, and:
//! - `locate` returns a range of `text` on character boundaries;
//! - an exact `str_replace` match of an `old` found once is `text` with that one
//!   occurrence replaced (by `new` with CRLF made LF);
//! - a `hashline` deletion of a valid range removes exactly that many lines;
//! - a patch applies only to the file it was given, and its repairs name lines of it.
#![no_main]

use std::path::Path;

use libfuzzer_sys::fuzz_target;
use tautly_tools::edit::hashline::{self, Anchor, HashlineOp};
use tautly_tools::edit::patch::{self, PatchOp};
use tautly_tools::edit::str_replace;
use tautly_tools::edit::tolerant::{self, RepairStage};

fuzz_target!(|data: &[u8]| {
    let Ok(input) = std::str::from_utf8(data) else {
        return;
    };
    if input.len() > 2048 {
        return;
    }
    let mut parts = input.splitn(4, '\0');
    let text = parts.next().unwrap_or("");
    let old = parts.next().unwrap_or("");
    let new = parts.next().unwrap_or("");
    let patch_text = parts.next().unwrap_or("");

    if let Ok(loc) = tolerant::locate(text, old) {
        let r = &loc.range;
        assert!(
            r.start <= r.end && r.end <= text.len(),
            "{r:?} out of {}",
            text.len()
        );
        assert!(text.is_char_boundary(r.start) && text.is_char_boundary(r.end));
    }

    match str_replace::apply_staged(text, old, new) {
        Ok((out, RepairStage::Exact)) if !old.is_empty() && text.matches(old).count() == 1 => {
            // `new` arrives with its CRLFs made LF (the file text is LF).
            let new = new.replace("\r\n", "\n");
            assert_eq!(out, text.replacen(old, &new, 1), "exact replace of {old:?}");
        }
        _ => {}
    }

    // Hashline anchors written by hand: every one is checked before anything changes.
    let anchors: Vec<Anchor> = old
        .split_whitespace()
        .filter_map(|a| Anchor::parse(a).ok())
        .collect();
    if let [start, end, ..] = anchors.as_slice() {
        let delete = [HashlineOp::Delete {
            start: start.clone(),
            end: end.clone(),
        }];
        if let Ok(out) = hashline::apply(text, &delete) {
            assert_eq!(
                out.lines().count() + (end.line - start.line + 1),
                text.lines().count(),
                "deleting {}..={} of {text:?} gave {out:?}",
                start.line,
                end.line
            );
        }
        let insert = [HashlineOp::InsertAfter {
            at: start.clone(),
            lines: new.lines().map(str::to_string).collect(),
        }];
        let _ = hashline::apply(text, &insert);
    }

    let files = |p: &Path| (p == Path::new("a.txt")).then(|| text.to_string());
    let _ = patch::touched(patch_text);
    if let Ok(applied) = patch::apply_staged(&files, patch_text) {
        let total = text.lines().count().max(1);
        for a in applied {
            if matches!(a.op, PatchOp::Update(_) | PatchOp::Delete) {
                assert_eq!(
                    a.path,
                    Path::new("a.txt"),
                    "changed a file it was not given"
                );
            }
            for r in a.repairs {
                assert!(
                    r.lines.0 >= 1 && r.lines.0 <= r.lines.1 && r.lines.1 <= total,
                    "repair names lines {:?} of a {total}-line file",
                    r.lines
                );
            }
        }
    }
});
