//! The entry file: YAML frontmatter that Obsidian shows as properties (spec 7.4), then the
//! markdown body.
//!
//! Written by hand, so the output is stable and minimal: block lists for `tags` and
//! `anchors`, plain scalars where YAML cannot misread them, double quotes elsewhere, and
//! `created`/`verified` as ISO date-times (UTC, no suffix, as Obsidian's date-time
//! property writes them).
//!
//! Read with `serde-saphyr` under a tight budget (events, nodes, depth, aliases, scalar
//! bytes), since entry files come from the repository and may be hostile; the parser is
//! panic-free by design and is also run under `catch_unwind`. Files of the first version
//! (`key: <JSON>` lines) are valid YAML; a hand-edited one YAML refuses (a plain `: ` in a
//! title) is read by the old line parser.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::time::{Duration, SystemTime};

use serde::Deserialize;
use serde_json::Value;

use crate::entry::{normalize_rel, valid_id};
use crate::vault::{clean_tags, parse_links};
use crate::{MemAnchor, MemoryEntry, MemoryKind};

/// The fields of a frontmatter, however it was written.
#[derive(Default)]
struct Raw {
    id: Option<String>,
    title: Option<String>,
    kind: Option<String>,
    tags: Vec<String>,
    anchors: Vec<RawAnchor>,
    created: Option<String>,
    verified: Option<String>,
}

#[derive(Deserialize)]
struct RawAnchor {
    path: String,
    #[serde(default)]
    symbol: Option<String>,
    hash: String,
}

pub(crate) fn render(e: &MemoryEntry) -> String {
    let mut out = String::from("---\n");
    out.push_str(&format!("id: {}\n", plain_or_quoted(&e.id)));
    out.push_str(&format!("title: {}\n", quoted(&e.title)));
    out.push_str(&format!("kind: {}\n", e.kind.as_str()));
    if !e.tags.is_empty() {
        out.push_str("tags:\n");
        for t in &e.tags {
            out.push_str(&format!("  - {}\n", plain_or_quoted(t)));
        }
    }
    if e.anchors.is_empty() {
        out.push_str("anchors: []\n");
    } else {
        out.push_str("anchors:\n");
        for a in &e.anchors {
            out.push_str(&format!(
                "  - path: {}\n",
                plain_or_quoted(&a.path.to_string_lossy())
            ));
            if let Some(s) = &a.symbol {
                out.push_str(&format!("    symbol: {}\n", quoted(s)));
            }
            out.push_str(&format!("    hash: {}\n", quoted(&a.hash)));
        }
    }
    out.push_str(&format!("created: {}\n", format_time(e.created)));
    if let Some(v) = e.verified {
        out.push_str(&format!("verified: {}\n", format_time(v)));
    }
    out.push_str("---\n");
    out.push_str(&e.body);
    out.push('\n');
    out
}

pub(crate) fn parse(text: &str) -> Result<MemoryEntry, String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let rest = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))
        .ok_or("missing `---` frontmatter")?;
    let (front, body) = split_frontmatter(rest).ok_or("unterminated frontmatter")?;
    let raw = match yaml(front) {
        Some(value) => from_yaml(value)?,
        None => legacy(front)?,
    };
    let id = raw.id.ok_or("missing `id`")?;
    if !valid_id(&id) {
        return Err(format!("invalid id `{id}`"));
    }
    let kind = raw.kind.ok_or("missing `kind`")?;
    let kind = MemoryKind::parse(kind.trim()).ok_or_else(|| format!("unknown kind `{kind}`"))?;
    let mut anchors = Vec::with_capacity(raw.anchors.len());
    for a in raw.anchors {
        let path = normalize_rel(Path::new(&a.path))
            .map_err(|why| format!("bad anchor path `{}`: {why}", a.path))?;
        anchors.push(MemAnchor {
            path,
            symbol: a.symbol,
            hash: a.hash,
        });
    }
    let body = body.strip_suffix('\n').unwrap_or(body);
    let body = body.strip_suffix('\r').unwrap_or(body).to_string();
    Ok(MemoryEntry {
        id,
        title: raw.title.ok_or("missing `title`")?,
        kind,
        tags: clean_tags(raw.tags),
        anchors,
        created: parse_time(&raw.created.ok_or("missing `created`")?)?,
        verified: raw.verified.as_deref().map(parse_time).transpose()?,
        links: parse_links(&body),
        body,
        stale: false,
    })
}

/// The frontmatter as YAML, or None when it is not YAML within the budget.
fn yaml(front: &str) -> Option<Value> {
    let mut budget = serde_saphyr::Budget::default();
    budget.max_events = 20_000;
    budget.max_nodes = 10_000;
    budget.max_depth = 16;
    budget.flow_nesting_limit = 16;
    budget.max_aliases = 64;
    budget.max_anchors = 64;
    budget.max_recorded_anchor_events = 10_000;
    budget.max_recorded_anchor_bytes = 64 * 1024;
    budget.max_documents = 1;
    budget.max_total_scalar_bytes = 64 * 1024;
    budget.max_merge_keys = 16;
    let mut options = serde_saphyr::Options::default();
    options.budget = Some(budget);
    options.alias_limits.max_total_replayed_events = 10_000;
    options.alias_limits.max_replay_stack_depth = 16;
    options.alias_limits.max_alias_expansions_per_anchor = 64;
    let parsed = catch_unwind(AssertUnwindSafe(|| {
        serde_saphyr::from_str_with_options::<Value>(front, options)
    }));
    match parsed {
        Ok(Ok(v)) => Some(v),
        _ => None,
    }
}

fn from_yaml(value: Value) -> Result<Raw, String> {
    let Value::Object(map) = value else {
        return Err("the frontmatter is not a set of properties".into());
    };
    let mut raw = Raw::default();
    for (key, v) in map {
        match key.as_str() {
            "id" => raw.id = text(&key, &v)?,
            "title" => raw.title = text(&key, &v)?,
            "kind" => raw.kind = text(&key, &v)?,
            "created" => raw.created = text(&key, &v)?,
            "verified" => raw.verified = text(&key, &v)?,
            "tags" => raw.tags = tag_list(&v)?,
            "anchors" => raw.anchors = anchor_list(v)?,
            _ => {} // Other properties (aliases, cssclasses, ...) are the user's.
        }
    }
    Ok(raw)
}

/// A scalar property as text; null is absent.
fn text(key: &str, v: &Value) -> Result<Option<String>, String> {
    match v {
        Value::Null => Ok(None),
        Value::String(s) => Ok(Some(s.clone())),
        Value::Number(n) => Ok(Some(n.to_string())),
        Value::Bool(b) => Ok(Some(b.to_string())),
        _ => Err(format!("`{key}` must be a single value")),
    }
}

/// `tags`: a list, or one string of tags separated by commas or spaces (Obsidian takes both).
fn tag_list(v: &Value) -> Result<Vec<String>, String> {
    match v {
        Value::Null => Ok(Vec::new()),
        Value::String(s) => Ok(s
            .split(|c: char| c == ',' || c.is_whitespace())
            .filter(|t| !t.is_empty())
            .map(str::to_string)
            .collect()),
        Value::Array(items) => items
            .iter()
            .filter_map(|t| text("tags", t).transpose())
            .collect(),
        _ => Err("`tags` must be a list".into()),
    }
}

fn anchor_list(v: Value) -> Result<Vec<RawAnchor>, String> {
    let Value::Array(items) = v else {
        return match v {
            Value::Null => Ok(Vec::new()),
            _ => Err("`anchors` must be a list".into()),
        };
    };
    items
        .into_iter()
        .map(|item| {
            let Value::Object(m) = item else {
                return Err("each anchor must have a `path` and a `hash`".to_string());
            };
            let field = |k: &str| m.get(k).map_or(Ok(None), |v| text(k, v));
            Ok(RawAnchor {
                path: field("path")?.ok_or("an anchor has no `path`")?,
                symbol: field("symbol")?,
                hash: field("hash")?.ok_or("an anchor has no `hash`")?,
            })
        })
        .collect()
}

/// The first version's reader: one `key: value` per line, values JSON or plain scalars,
/// `anchors` a JSON list.
fn legacy(front: &str) -> Result<Raw, String> {
    let mut raw = Raw::default();
    for line in front.lines().filter(|l| !l.trim().is_empty()) {
        let (key, value) = line
            .split_once(':')
            .ok_or_else(|| format!("bad frontmatter line `{line}`"))?;
        match key.trim() {
            "id" => raw.id = Some(scalar(value)?),
            "title" => raw.title = Some(scalar(value)?),
            "kind" => raw.kind = Some(scalar(value)?),
            "created" => raw.created = Some(scalar(value)?),
            "verified" => raw.verified = Some(scalar(value)?),
            "anchors" => {
                raw.anchors =
                    serde_json::from_str(value.trim()).map_err(|e| format!("bad anchors: {e}"))?
            }
            _ => {}
        }
    }
    Ok(raw)
}

/// A scalar of the old format: double-quoted (JSON escapes), single-quoted, or plain.
fn scalar(raw: &str) -> Result<String, String> {
    let raw = raw.trim();
    if raw.starts_with('"') {
        return serde_json::from_str(raw).map_err(|e| format!("bad string {raw}: {e}"));
    }
    if raw.len() >= 2 && raw.starts_with('\'') && raw.ends_with('\'') {
        return Ok(raw[1..raw.len() - 1].replace("''", "'"));
    }
    Ok(raw.to_string())
}

/// The text before the closing `---` line, and the text after it.
fn split_frontmatter(rest: &str) -> Option<(&str, &str)> {
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim_end() == "---" {
            return Some((&rest[..offset], &rest[offset + line.len()..]));
        }
        offset += line.len();
    }
    None
}

/// Words YAML reads as something other than a string.
const RESERVED: [&str; 10] = [
    "true", "false", "null", "yes", "no", "on", "off", "y", "n", "nan",
];

/// `s` as a plain scalar when no YAML reader can take it for anything else (it starts with
/// a letter and has only letters, digits and `_-./`), else double-quoted.
fn plain_or_quoted(s: &str) -> String {
    let plain = s
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/'))
        && !RESERVED.contains(&s.to_ascii_lowercase().as_str());
    if plain { s.to_string() } else { quoted(s) }
}

/// A YAML double-quoted scalar; control characters and the characters YAML 1.1 readers
/// take as line breaks or byte-order marks are escaped.
fn quoted(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if c.is_control() || matches!(c, '\u{2028}' | '\u{2029}' | '\u{feff}') => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// `2026-11-03T10:00:00` (UTC).
fn format_time(t: SystemTime) -> String {
    let s = humantime::format_rfc3339_seconds(t).to_string();
    s.strip_suffix('Z').map(str::to_string).unwrap_or(s)
}

/// An ISO date or date-time: `2026-11-03`, `2026-11-03T10:00`, `...T10:00:00`, with
/// optional fractional seconds, a space for the `T`, and `Z` or `±HH:MM`; without a zone
/// it is UTC.
fn parse_time(s: &str) -> Result<SystemTime, String> {
    let bad = |why: String| format!("bad timestamp `{s}`: {why}");
    let t = s.trim();
    if !t.is_ascii() {
        return Err(bad("not an ISO date".into()));
    }
    let (base, offset) = split_zone(t).ok_or_else(|| bad("bad time zone".into()))?;
    let base = match base.len() {
        10 => format!("{base}T00:00:00"),
        16 => format!("{base}:00"),
        _ => base.to_string(),
    };
    let utc = humantime::parse_rfc3339_weak(&base).map_err(|e| bad(e.to_string()))?;
    let shifted = if offset >= 0 {
        utc.checked_sub(Duration::from_secs(offset.unsigned_abs()))
    } else {
        utc.checked_add(Duration::from_secs(offset.unsigned_abs()))
    };
    shifted.ok_or_else(|| bad("out of range".into()))
}

/// The date-time without its zone, and the zone's offset east of UTC in seconds.
fn split_zone(t: &str) -> Option<(&str, i64)> {
    if let Some(base) = t.strip_suffix(['Z', 'z']) {
        return Some((base, 0));
    }
    // A zone only follows a time: `T` or a space at byte 10.
    if t.len() > 16 {
        let (base, zone) = t.split_at(t.len() - 6);
        let z = zone.as_bytes();
        if matches!(z[0], b'+' | b'-') && z[3] == b':' {
            let hours: i64 = zone[1..3].parse().ok()?;
            let minutes: i64 = zone[4..6].parse().ok()?;
            let secs = hours * 3600 + minutes * 60;
            return Some((base, if z[0] == b'-' { -secs } else { secs }));
        }
    }
    Some((t, 0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::UNIX_EPOCH;

    #[test]
    fn times_parse_in_every_iso_form() {
        let at = |s: &str| {
            parse_time(s)
                .unwrap()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs()
        };
        let ten = 1_793_700_000;
        assert_eq!(at("2026-11-03T10:00:00"), ten);
        assert_eq!(at("2026-11-03T10:00:00Z"), ten);
        assert_eq!(at("2026-11-03 10:00:00"), ten);
        assert_eq!(at("2026-11-03T10:00"), ten);
        assert_eq!(at("2026-11-03T10:00:00.250"), ten);
        assert_eq!(at("2026-11-03T12:00:00+02:00"), ten);
        assert_eq!(at("2026-11-03T05:30:00-04:30"), ten);
        assert_eq!(at("2026-11-03"), ten - 10 * 3600);
        for bad in [
            "yesterday",
            "2026-13-01",
            "2026-11-03T10:00:00+2",
            "",
            "２０２６",
        ] {
            assert!(parse_time(bad).is_err(), "{bad}");
        }
        assert_eq!(
            format_time(UNIX_EPOCH + Duration::from_secs(ten)),
            "2026-11-03T10:00:00"
        );
    }

    #[test]
    fn plain_scalars_only_when_unambiguous() {
        for plain in ["use-pnpm-1a2b", "src/lib.rs", "js/tooling", "_x"] {
            assert_eq!(plain_or_quoted(plain), plain);
        }
        for q in ["true", "No", "1a", "", "a b", "a:b", "a#b", "-x", "é"] {
            assert!(plain_or_quoted(q).starts_with('"'), "{q}");
        }
        assert_eq!(
            quoted("a\"b\\c\n\u{7}\u{2028}"),
            "\"a\\\"b\\\\c\\n\\u0007\\u2028\""
        );
    }
}
