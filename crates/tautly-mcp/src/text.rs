//! Text from servers and repositories, made safe to show (to a user) and to read (by
//! the model).
//!
//! A terminal acts on control characters (ESC sequences can erase or redraw what is
//! shown, CR returns to the start of the line) and a renderer acts on invisible format
//! characters (bidirectional overrides reorder text, zero-width characters hide it). So a
//! command line from a repository could look harmless while being something else, and a
//! tool description could carry instructions a reader never sees (tag characters spell
//! ASCII invisibly: "ASCII smuggling").

/// Characters that are not shown, or that change how other text is shown: every format
/// (`Cf`) character (bidi controls, zero-width characters, the byte-order mark, soft
/// hyphen, the tag block U+E0000–E007F, …), the deprecated format characters
/// U+206A–206F, line and paragraph separators, the Hangul fillers, variation selectors
/// (U+180B–180D, U+FE00–FE0F, U+E0100–E01EF), the combining grapheme joiner U+034F and
/// the Khmer inherent vowels U+17B4–17B5.
fn invisible(c: char) -> bool {
    matches!(
        c,
        // Cf (Unicode 16), by block.
        '\u{00ad}'
            | '\u{0600}'..='\u{0605}'
            | '\u{061c}'
            | '\u{06dd}'
            | '\u{070f}'
            | '\u{0890}'..='\u{0891}'
            | '\u{08e2}'
            | '\u{180e}'
            | '\u{200b}'..='\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{206f}'
            | '\u{feff}'
            | '\u{fff9}'..='\u{fffb}'
            | '\u{110bd}'
            | '\u{110cd}'
            | '\u{13430}'..='\u{1343f}'
            | '\u{1bca0}'..='\u{1bca3}'
            | '\u{1d173}'..='\u{1d17a}'
            | '\u{e0000}'..='\u{e007f}'
            // Not Cf, but invisible or layout-changing all the same.
            | '\u{034f}'
            | '\u{115f}'..='\u{1160}'
            | '\u{17b4}'..='\u{17b5}'
            | '\u{180b}'..='\u{180d}'
            | '\u{180f}'
            | '\u{2028}'..='\u{2029}'
            | '\u{3164}'
            | '\u{fe00}'..='\u{fe0f}'
            | '\u{ffa0}'
            | '\u{e0100}'..='\u{e01ef}'
    )
}

/// `s` for display: `\` doubled, CR, LF and tab as `\r`, `\n`, `\t`, every other control
/// and invisible character as `\u{..}`, everything else as it is. So no two different
/// strings print the same (a literal `\r` shows as `\\r`).
pub fn printable(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\r' => out.push_str("\\r"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() || invisible(c) => {
                out.push_str(&format!("\\u{{{:x}}}", c as u32));
            }
            c => out.push(c),
        }
    }
    out
}

/// `s` for the model: control and invisible characters removed, except newlines and
/// tabs.
pub(crate) fn strip_control(s: &str) -> String {
    s.chars()
        .filter(|&c| c == '\n' || c == '\t' || !(c.is_control() || invisible(c)))
        .collect()
}

/// Every string in `v` (an input schema: descriptions, titles, enum values, defaults)
/// through [`strip_control`]; keys are left as they are, since they are argument names.
pub(crate) fn strip_control_json(v: &mut serde_json::Value) {
    match v {
        serde_json::Value::String(s) => *s = strip_control(s),
        serde_json::Value::Array(a) => a.iter_mut().for_each(strip_control_json),
        serde_json::Value::Object(o) => o.values_mut().for_each(strip_control_json),
        _ => {}
    }
}

/// `s` when at most `head + tail` bytes, else its first `head` and last `tail` bytes (on
/// character boundaries) around `…`, with `total` (the length of what `s` displays) at
/// the end: whatever follows padding stays visible.
pub(crate) fn head_tail(s: &str, head: usize, tail: usize, total: usize) -> String {
    if s.len() <= head + tail {
        return s.to_string();
    }
    let mut h = head;
    while !s.is_char_boundary(h) {
        h -= 1;
    }
    let mut t = s.len() - tail;
    while !s.is_char_boundary(t) {
        t += 1;
    }
    format!("{}…{} ({total} bytes)", &s[..h], &s[t..])
}
