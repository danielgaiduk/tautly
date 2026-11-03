//! Diff-on-re-read (spec 7.2): rendering what changed in a range the model already read.

use similar::{DiffTag, TextDiff};

/// Lines of context around each changed hunk.
const CONTEXT: usize = 2;

/// A unified diff from `old` to `new` (both the lines of the same requested range, whose
/// first line is line `first_line` of the file), or `None` when half or more of the lines
/// changed and the full content is the better answer. Hunk headers carry file line numbers.
pub(crate) fn small_diff(old: &[&str], new: &[&str], first_line: usize) -> Option<String> {
    let diff = TextDiff::from_slices(old, new);
    let (mut deleted, mut inserted) = (0, 0);
    for op in diff.ops() {
        let (tag, old_range, new_range) = op.as_tag_tuple();
        if matches!(tag, DiffTag::Delete | DiffTag::Replace) {
            deleted += old_range.len();
        }
        if matches!(tag, DiffTag::Insert | DiffTag::Replace) {
            inserted += new_range.len();
        }
    }
    if deleted.max(inserted) * 2 >= old.len().max(new.len()) {
        return None;
    }
    // Unified-diff convention: an empty side is named by the line before it.
    let number = |idx: usize, len: usize| first_line - 1 + idx + usize::from(len > 0);
    let mut out: Vec<String> = Vec::new();
    for group in diff.grouped_ops(CONTEXT) {
        let (Some(head), Some(tail)) = (group.first(), group.last()) else {
            continue;
        };
        let (old_from, old_to) = (head.old_range().start, tail.old_range().end);
        let (new_from, new_to) = (head.new_range().start, tail.new_range().end);
        let (old_len, new_len) = (old_to - old_from, new_to - new_from);
        out.push(format!(
            "@@ -{},{old_len} +{},{new_len} @@",
            number(old_from, old_len),
            number(new_from, new_len)
        ));
        for op in &group {
            for change in diff.iter_changes(op) {
                let sign = match change.tag() {
                    similar::ChangeTag::Equal => ' ',
                    similar::ChangeTag::Delete => '-',
                    similar::ChangeTag::Insert => '+',
                };
                out.push(format!("{sign}{}", change.value()));
            }
        }
    }
    Some(out.join("\n"))
}
