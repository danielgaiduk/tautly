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

/// The targets of the `[[target]]`, `[[target|label]]` and `[[target#heading]]` links in
/// `body`, as written, each once, at most [`MAX_LINKS`].
pub fn parse_links(body: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut rest = body;
    while let Some(open) = rest.find("[[") {
        let after = &rest[open + 2..];
        let Some(close) = after.find("]]") else {
            break;
        };
        let inner = &after[..close];
        // `[[a [[b]]`: the link is the inner one.
        if let Some(nested) = inner.rfind("[[") {
            rest = &after[nested..];
            continue;
        }
        rest = &after[close + 2..];
        if inner.contains('\n') {
            continue;
        }
        let target = inner.split('|').next().unwrap_or("");
        let target = target.split(['#', '^']).next().unwrap_or("").trim();
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
    } else {
        slug.to_string()
    }
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
}
