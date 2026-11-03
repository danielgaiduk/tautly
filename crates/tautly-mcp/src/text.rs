//! Text from servers and repositories, made safe to show (to a user) and to read (by
//! the model).
//!
//! A terminal acts on control characters (ESC sequences can erase or redraw what is
//! shown, CR returns to the start of the line) and a renderer acts on invisible format
//! characters (bidirectional overrides reorder text, zero-width characters hide it). So a
//! command line from a repository could look harmless while being something else.

/// Characters that change how text is laid out or shown without being visible: bidi
/// embeddings, overrides and isolates, zero-width characters, line and paragraph
/// separators, the byte-order mark, soft hyphen and interlinear annotation marks.
fn invisible(c: char) -> bool {
    matches!(
        c,
        '\u{00ad}'
            | '\u{061c}'
            | '\u{180e}'
            | '\u{200b}'..='\u{200f}'
            | '\u{2028}'..='\u{202e}'
            | '\u{2060}'..='\u{2069}'
            | '\u{feff}'
            | '\u{fff9}'..='\u{fffb}'
    )
}

/// `s` for display: control and invisible characters escaped (`\r`, `\n`, `\t`, else
/// `\u{..}`), everything else as it is.
pub fn printable(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
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

/// `s` cut to at most `cap` bytes on a character boundary, with `…` when it was cut.
pub(crate) fn cut(s: &str, cap: usize) -> String {
    if s.len() <= cap {
        return s.to_string();
    }
    let mut end = cap;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}
