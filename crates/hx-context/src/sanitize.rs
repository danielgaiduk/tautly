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

/// `s` as the body of a JSON string: backslash and quote are escaped (`\\`, `\"`) and every
/// unsafe character is written as a JSON escape (`\n`, `\r`, `\t`, `\u003c`, ...), so the
/// result is unambiguous (a literal backslash-`n` differs from a newline) and parses back to
/// `s` as a JSON string. For names that must stay visible, such as the files the engine
/// tracks. No length cap.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if is_unsafe(c) => out.push_str(&format!("\\u{:04x}", c as u32)),
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

    /// The escaped text read back as a JSON string.
    fn unescape(e: &str) -> String {
        serde_json::from_str(&format!("\"{e}\"")).unwrap()
    }

    #[test]
    fn escape_leaves_nothing_unsafe() {
        let input = "a\nb\r\tc<d>\u{2028}";
        let e = escape(input);
        assert_eq!(e, "a\\nb\\r\\tc\\u003cd\\u003e\\u2028");
        assert!(!e.chars().any(is_unsafe));
        assert_eq!(escape("src/ok.rs"), "src/ok.rs");
        assert_eq!(unescape(&e), input);
    }

    #[test]
    fn escape_is_unambiguous_and_round_trips() {
        // A literal backslash followed by `n` is not a newline.
        let literal = r"a\nb";
        let newline = "a\nb";
        assert_eq!(escape(literal), r"a\\nb");
        assert_eq!(escape(newline), r"a\nb");
        assert_ne!(escape(literal), escape(newline));
        assert_eq!(unescape(&escape(literal)), literal);
        assert_eq!(unescape(&escape(newline)), newline);
        for s in [
            "dir\\sub\\file.rs",
            "q\"uote",
            "\\u003c",
            "\0\u{1}\u{8}\u{c}\u{1f}\u{7f}\u{85}",
            "\u{2029}é日本\\",
        ] {
            let e = escape(s);
            assert!(!e.chars().any(is_unsafe), "{e:?}");
            assert_eq!(unescape(&e), s, "{s:?}");
        }
        // Control characters without a short form use the four-digit JSON form.
        assert_eq!(escape("\u{1}"), "\\u0001");
        assert_eq!(escape("\u{8}\u{c}"), "\\b\\f");
    }
}
