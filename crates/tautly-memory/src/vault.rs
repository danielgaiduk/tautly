//! Obsidian conventions (spec 7.4): tags, `[[wikilinks]]` and readable file names.

/// Tags an entry keeps at most.
const MAX_TAGS: usize = 16;
/// Longest tag kept, in characters.
const MAX_TAG_CHARS: usize = 64;
/// Link targets an entry keeps at most.
const MAX_LINKS: usize = 32;
/// Longest link target kept, in bytes.
const MAX_LINK_BYTES: usize = 200;
/// Longest file-name slug, in characters (before `-N.md`).
const MAX_SLUG_CHARS: usize = 60;

/// `raw` as Obsidian tags: a leading `#` dropped, spaces as `-`, only letters, digits and
/// `_-/` kept, at least one non-digit, each once, at most [`MAX_TAGS`].
pub(crate) fn clean_tags(raw: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for t in raw {
        let t = t.trim().trim_start_matches('#');
        let tag: String = t
            .chars()
            .filter_map(|c| match c {
                c if c.is_whitespace() => Some('-'),
                c if c.is_alphanumeric() || matches!(c, '_' | '-' | '/') => Some(c),
                _ => None,
            })
            .take(MAX_TAG_CHARS)
            .collect();
        let tag = tag.trim_matches(['-', '/']).to_string();
        if tag
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, '-' | '/'))
            || out.contains(&tag)
        {
            continue;
        }
        out.push(tag);
        if out.len() == MAX_TAGS {
            break;
        }
    }
    out
}

/// The `[[...]]` links of `body` in order: each link's inner text with its byte range
/// (inside the brackets). A link spans one line; in `[[a [[b]]` the link is the inner one.
fn links(body: &str) -> impl Iterator<Item = (std::ops::Range<usize>, &str)> {
    let mut at = 0;
    std::iter::from_fn(move || {
        loop {
            let open = at + body[at..].find("[[")?;
            let start = open + 2;
            let close = start + body[start..].find("]]")?;
            let inner = &body[start..close];
            if let Some(nested) = inner.rfind("[[") {
                at = start + nested;
                continue;
            }
            at = close + 2;
            if !inner.contains('\n') {
                return Some((start..close, inner));
            }
        }
    })
}

/// A link's inner text as its target (trimmed), the `#heading` or `^block` after it, and
/// its label.
fn split_link(inner: &str) -> (&str, &str, Option<&str>) {
    let (dest, label) = match inner.split_once('|') {
        Some((d, l)) => (d, Some(l)),
        None => (inner, None),
    };
    let cut = dest.find(['#', '^']).unwrap_or(dest.len());
    (dest[..cut].trim(), &dest[cut..], label)
}

/// The targets of the `[[target]]`, `[[target|label]]` and `[[target#heading]]` links in
/// `body`, as written, each once, at most [`MAX_LINKS`].
pub fn parse_links(body: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (_, inner) in links(body) {
        let (target, _, _) = split_link(inner);
        if target.is_empty() || target.len() > MAX_LINK_BYTES || out.iter().any(|t| t == target) {
            continue;
        }
        out.push(target.to_string());
        if out.len() == MAX_LINKS {
            break;
        }
    }
    out
}

/// `body` with each link whose target `resolve` knows written as Obsidian resolves it, by
/// file name: `[[<stem><#heading>|<label>]]`, the label kept or else the entry's title.
/// `resolve` gives the target's `(file stem, title)`, or None to keep the link as written
/// (an unknown target, or one that already names a file).
pub(crate) fn link_by_file_name(
    body: &str,
    resolve: impl Fn(&str) -> Option<(String, String)>,
) -> String {
    let mut out = String::with_capacity(body.len());
    let mut done = 0;
    for (range, inner) in links(body) {
        let (target, sub, label) = split_link(inner);
        if target.is_empty() || target.len() > MAX_LINK_BYTES {
            continue;
        }
        let Some((stem, title)) = resolve(target) else {
            continue;
        };
        let label = match label {
            Some(l) => l.to_string(),
            None => link_label(&title),
        };
        out.push_str(&body[done..range.start]);
        out.push_str(&format!("{stem}{sub}|{label}"));
        done = range.end;
    }
    out.push_str(&body[done..]);
    out
}

/// `title` as a link label: the characters that would end or split the link replaced.
fn link_label(title: &str) -> String {
    title
        .chars()
        .map(|c| match c {
            '|' => '-',
            '[' => '(',
            ']' => ')',
            c => c,
        })
        .collect()
}

/// The file-name stem for an entry titled `title`: its words, lowercased, joined by `-`
/// (letters and digits of any script), else `entry`.
pub(crate) fn file_slug(title: &str) -> String {
    let mut slug = String::new();
    for c in title.chars() {
        if c.is_alphanumeric() {
            slug.extend(c.to_lowercase());
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug: String = slug
        .trim_end_matches('-')
        .chars()
        .take(MAX_SLUG_CHARS)
        .collect();
    let slug = slug.trim_end_matches('-');
    if slug.is_empty() {
        "entry".into()
    } else if is_windows_device_name(slug) {
        // Such a file cannot be checked out on Windows, where Obsidian also runs.
        format!("{slug}-note")
    } else {
        slug.to_string()
    }
}

/// `con`, `prn`, `aux`, `nul`, `com1`-`com9` and `lpt1`-`lpt9`, in any case.
fn is_windows_device_name(slug: &str) -> bool {
    let s = slug.to_ascii_lowercase();
    matches!(s.as_str(), "con" | "prn" | "aux" | "nul")
        || ((s.starts_with("com") || s.starts_with("lpt"))
            && s.len() == 4
            && matches!(s.as_bytes()[3], b'1'..=b'9'))
}

/// Whether the store reads a file named `name`: a visible `.md` file (Obsidian and the
/// store's own temporary files start with a dot).
pub(crate) fn is_entry_file(name: &str) -> bool {
    !name.starts_with('.') && name.ends_with(".md") && name.len() > 3
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_tags_and_slugs() {
        assert_eq!(
            parse_links(
                "a [[X]] [[Y|why]] [[X#h]] [[ ]] [[Z^b]] [[broken\nline]] [[a [[N]] ![[E]]"
            ),
            ["X", "Y", "Z", "N", "E"]
        );
        assert!(parse_links("[[unclosed").is_empty());
        assert_eq!(
            clean_tags([
                "#a b".into(),
                "a-b".into(),
                "123".into(),
                "x/y/".into(),
                "é!".into()
            ]),
            ["a-b", "x/y", "é"]
        );
        assert_eq!(clean_tags((0..40).map(|i| format!("t{i}"))).len(), MAX_TAGS);
        assert_eq!(file_slug("  Use pnpm, not npm! "), "use-pnpm-not-npm");
        assert_eq!(file_slug("Ünïcode Straße"), "ünïcode-straße");
        assert_eq!(file_slug("***"), "entry");
        assert_eq!(file_slug(&"ab ".repeat(100)).chars().count(), 59);
        assert!(is_entry_file("a.md") && !is_entry_file(".a.md") && !is_entry_file(".md"));
    }

    #[test]
    fn windows_device_names_get_a_suffix() {
        for (title, slug) in [
            ("Con", "con-note"),
            ("PRN", "prn-note"),
            ("aux", "aux-note"),
            ("Nul!", "nul-note"),
            ("COM1", "com1-note"),
            ("com9", "com9-note"),
            ("LPT1", "lpt1-note"),
            ("lpt9", "lpt9-note"),
        ] {
            assert_eq!(file_slug(title), slug, "{title}");
        }
        for title in ["com10", "com0", "lpt", "console", "con fig", "auxiliary"] {
            assert!(!file_slug(title).ends_with("-note"), "{title}");
        }
    }
}
