//! Repo-controlled strings (file and directory names, manifest keys, symbol names) end up in
//! prompt blocks. A name must not be able to carry a line break or a tag that ends the block
//! and starts instructions, so every such string goes through here first.

/// Longest name [`clean`] keeps; longer ones are cut and end in `…`.
pub const MAX_NAME_CHARS: usize = 120;

/// Characters that could break a block's shape: controls (`\n`, `\r`, `\t`, ...), the Unicode
/// line and paragraph separators, and angle brackets (tags).
fn is_unsafe(c: char) -> bool {
    c.is_control() || matches!(c, '\u{2028}' | '\u{2029}' | '<' | '>')
}

/// `name` capped to [`MAX_NAME_CHARS`] (cut with a trailing `…`), or None if it contains an
/// unsafe character: such a name is skipped rather than shown in a mangled form.
pub fn clean(name: &str) -> Option<String> {
    if name.is_empty() || name.chars().any(is_unsafe) {
        return None;
    }
    if name.chars().count() <= MAX_NAME_CHARS {
        return Some(name.to_string());
    }
    let mut out: String = name.chars().take(MAX_NAME_CHARS - 1).collect();
    out.push('…');
    Some(out)
}

/// `s` with every unsafe character written as an escape (`\n`, `\r`, `\t`, `\u{3c}`, ...).
/// For names that must stay visible, such as the files the engine tracks. No length cap.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if is_unsafe(c) => out.push_str(&format!("\\u{{{:x}}}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_skips_unsafe_and_caps_length() {
        assert_eq!(clean("src/lib.rs").as_deref(), Some("src/lib.rs"));
        for bad in [
            "a\nb",
            "a\rb",
            "a\tb",
            "a\u{2028}b",
            "a\u{2029}b",
            "<b>",
            "a>b",
            "a\0b",
            "",
        ] {
            assert_eq!(clean(bad), None, "{bad:?}");
        }
        let long = clean(&"x".repeat(3000)).unwrap();
        assert_eq!(long.chars().count(), MAX_NAME_CHARS);
        assert!(long.ends_with('…'));
        assert_eq!(
            clean(&"é".repeat(MAX_NAME_CHARS)).unwrap().chars().count(),
            MAX_NAME_CHARS
        );
    }

    #[test]
    fn escape_leaves_nothing_unsafe() {
        let e = escape("a\nb\r\tc<d>\u{2028}");
        assert_eq!(e, "a\\nb\\r\\tc\\u{3c}d\\u{3e}\\u{2028}");
        assert!(!e.chars().any(is_unsafe));
        assert_eq!(escape("src/ok.rs"), "src/ok.rs");
    }
}
