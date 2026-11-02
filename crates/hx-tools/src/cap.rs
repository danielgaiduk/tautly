//! Output capping shared by the read-side tools.

/// Cap for tool output, per the 30 KB project limit.
pub(crate) const OUTPUT_CAP: usize = 30 * 1024;

/// Keep the head and tail of `full` within `cap` bytes, cutting on char boundaries and
/// replacing the middle with an omitted-bytes marker. Returns `(text, truncated)`.
pub fn cap_text(full: &str, cap: usize) -> (String, bool) {
    if full.len() <= cap {
        return (full.to_string(), false);
    }
    let mut head_end = cap / 2;
    while !full.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = full.len() - (cap - cap / 2);
    while !full.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    let omitted = tail_start - head_end;
    let out = format!(
        "{}\n… [{omitted} bytes omitted] …\n{}",
        &full[..head_end],
        &full[tail_start..]
    );
    (out, true)
}
