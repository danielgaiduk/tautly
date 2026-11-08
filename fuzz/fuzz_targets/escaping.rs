//! `printable` and `printable_multiline`: no raw control or invisible character
//! survives; `printable` is injective (decodes back to its input); `printable_multiline`
//! is idempotent (its output has nothing left to escape).
#![no_main]

use libfuzzer_sys::fuzz_target;
use tautly_mcp::{printable, printable_multiline};

/// What a terminal or renderer acts on: controls, and the bidi and zero-width format
/// characters (a sample of `invisible`, enough to catch a regression).
fn raw(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{200b}'..='\u{200f}'
                | '\u{202a}'..='\u{202e}'
                | '\u{2066}'..='\u{2069}'
                | '\u{feff}'
                | '\u{e0000}'..='\u{e007f}'
                | '\u{2028}'
                | '\u{2029}'
        )
}

/// `printable`'s inverse.
fn decode(s: &str) -> Option<String> {
    let mut out = String::new();
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next()? {
            '\\' => out.push('\\'),
            'r' => out.push('\r'),
            'n' => out.push('\n'),
            't' => out.push('\t'),
            'u' => {
                if it.next()? != '{' {
                    return None;
                }
                let hex: String = it.by_ref().take_while(|&c| c != '}').collect();
                out.push(char::from_u32(u32::from_str_radix(&hex, 16).ok()?)?);
            }
            _ => return None,
        }
    }
    Some(out)
}

fuzz_target!(|data: &[u8]| {
    let s = String::from_utf8_lossy(data);
    let p = printable(&s);
    assert!(!p.chars().any(raw), "printable left a raw character: {p:?}");
    assert_eq!(
        decode(&p).as_deref(),
        Some(&*s),
        "printable is not reversible"
    );

    let m = printable_multiline(&s);
    assert!(
        !m.chars().any(|c| c != '\n' && c != '\t' && raw(c)),
        "printable_multiline left a raw character: {m:?}"
    );
    assert_eq!(
        printable_multiline(&m),
        m,
        "printable_multiline is not idempotent"
    );
});
