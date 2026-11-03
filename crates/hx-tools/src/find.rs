//! `find`: one ranked search over symbols, paths and text (spec 7.10).

use std::path::Path;

use async_trait::async_trait;
use hx_provider::{Capabilities, ToolSpec};
use hx_sandbox::{Effect, Sandbox};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::cap::{OUTPUT_CAP, cap_text};
use crate::grep::{SearchSpec, search};
use crate::index_tools::fallback_header;
use crate::spec::{display_rel, live_index, parse_args, tool_spec, walk_files};
use crate::{Tool, ToolCx, ToolError, ToolResult};

const DEFAULT_LIMIT: usize = 20;
const MAX_LIMIT: usize = 50;
/// Text matches reported per file.
const TEXT_PER_FILE: usize = 5;
/// Longest snippet, in characters.
const SNIPPET_CHARS: usize = 160;

pub struct FindTool;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    query: String,
    kind: Option<Kind>,
    limit: Option<usize>,
}

#[derive(Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum Kind {
    Symbol,
    Path,
    Text,
}

/// One result row: `loc  kind  snippet`.
#[derive(Debug)]
struct Row {
    /// `path:line`.
    loc: String,
    kind: String,
    snippet: String,
}

#[async_trait]
impl Tool for FindTool {
    fn name(&self) -> &'static str {
        "find"
    }

    fn spec(&self, _caps: &Capabilities) -> ToolSpec {
        tool_spec(
            "find",
            "Search symbol definitions, file paths (fuzzy) and file text at once. Results are \
             ranked: exact symbol definitions, then path matches, then text matches, as \
             `path:line  kind  snippet`. Use this first when you do not know where something is.",
            json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "A symbol name, part of a path, or text."},
                    "kind": {
                        "type": "string",
                        "enum": ["symbol", "path", "text"],
                        "description": "Search only this kind of match."
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": MAX_LIMIT,
                        "description": "Most results to return (default 20)."
                    }
                },
                "required": ["query"],
                "additionalProperties": false
            }),
        )
    }

    fn effect(&self, args: &Value) -> Result<Effect, ToolError> {
        parse_args::<Args>(args)?;
        Ok(Effect::ReadOnly)
    }

    async fn run(&self, args: Value, sb: &dyn Sandbox, cx: &ToolCx) -> ToolResult {
        let a: Args = match parse_args(&args) {
            Ok(a) => a,
            Err(e) => return ToolResult::error(e.to_string()),
        };
        let query = a.query.trim();
        if query.is_empty() {
            return ToolResult::error("query must not be empty");
        }
        let limit = a.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
        let wants = |k: Kind| a.kind.is_none_or(|w| w == k);

        let (symbols, paths, texts) = tokio::join!(
            async {
                if wants(Kind::Symbol) {
                    symbol_rows(query, limit, a.kind == Some(Kind::Symbol), sb, cx).await
                } else {
                    Ok(Vec::new())
                }
            },
            async {
                if wants(Kind::Path) {
                    path_rows(query, limit + 1, sb, cx).await
                } else {
                    Ok(Vec::new())
                }
            },
            async {
                if wants(Kind::Text) {
                    text_rows(query, limit, sb, cx).await
                } else {
                    Ok(Vec::new())
                }
            },
        );
        let (symbols, paths, texts) = match (symbols, paths, texts) {
            (Ok(s), Ok(p), Ok(t)) => (s, p, t),
            (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => return ToolResult::error(e),
        };

        // A definition found as a symbol is not repeated as a text hit.
        let mut rows = symbols;
        let defined: Vec<String> = rows.iter().map(|r| r.loc.clone()).collect();
        rows.extend(paths);
        rows.extend(texts.into_iter().filter(|r| !defined.contains(&r.loc)));

        let header = fallback_header(cx);
        let mut out = header.clone().unwrap_or_default();
        if rows.is_empty() {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&format!("no matches for `{query}`"));
            return ToolResult::ok(out);
        }
        let more = rows.len() > limit;
        rows.truncate(limit);
        for row in &rows {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&format!("{}  {}  {}", row.loc, row.kind, row.snippet));
        }
        if more {
            out.push_str("\n[more results not shown; raise limit or narrow with kind]");
        }
        ToolResult::ok(cap_text(&out, OUTPUT_CAP).0)
    }
}

/// Definitions named exactly `query`. Without a ready index the best available answer is
/// every whole-word occurrence, labelled as such; when text is searched too (no `kind`)
/// those occurrences are already among the text hits, so none are added here.
async fn symbol_rows(
    query: &str,
    limit: usize,
    symbol_only: bool,
    sb: &dyn Sandbox,
    cx: &ToolCx,
) -> Result<Vec<Row>, String> {
    if let Some(index) = cx.index().filter(|i| i.is_ready()) {
        let rows = index
            .find_symbol(query, None)
            .map_err(|e| format!("index error: {e}"))?;
        return Ok(rows
            .into_iter()
            .take(limit + 1)
            .map(|r| Row {
                loc: format!("{}:{}", r.path.display(), r.start_line),
                kind: r.kind.as_str().to_string(),
                snippet: snippet(&r.signature),
            })
            .collect());
    }
    if symbol_only {
        word_rows(query, limit, sb, cx).await
    } else {
        Ok(Vec::new())
    }
}

async fn word_rows(
    query: &str,
    limit: usize,
    sb: &dyn Sandbox,
    cx: &ToolCx,
) -> Result<Vec<Row>, String> {
    let pattern = format!(r"\b{}\b", regex::escape(query));
    let spec = SearchSpec {
        pattern: &pattern,
        path: None,
        glob: None,
        case_insensitive: false,
        max_matches: limit + 1,
        per_file: TEXT_PER_FILE,
        max_bytes: usize::MAX,
    };
    let found = search(&spec, sb, cx).await?;
    Ok(found
        .matches
        .into_iter()
        .map(|m| Row {
            loc: format!("{}:{}", m.rel, m.line),
            kind: "mention".into(),
            snippet: snippet(&m.text),
        })
        .collect())
}

/// Lines containing `query` (ignoring case unless the query has capitals), at most five
/// per file, through the same index-backed search as `grep`.
async fn text_rows(
    query: &str,
    limit: usize,
    sb: &dyn Sandbox,
    cx: &ToolCx,
) -> Result<Vec<Row>, String> {
    let pattern = regex::escape(query);
    let spec = SearchSpec {
        pattern: &pattern,
        path: None,
        glob: None,
        case_insensitive: !query.chars().any(char::is_uppercase),
        // Room for hits that duplicate symbol rows and are dropped later.
        max_matches: 2 * limit + 1,
        per_file: TEXT_PER_FILE,
        max_bytes: usize::MAX,
    };
    let found = search(&spec, sb, cx).await?;
    Ok(found
        .matches
        .into_iter()
        .map(|m| Row {
            loc: format!("{}:{}", m.rel, m.line),
            kind: "text".into(),
            snippet: snippet(&m.text),
        })
        .collect())
}

/// The best `n` fuzzy path matches, best first.
async fn path_rows(
    query: &str,
    n: usize,
    sb: &dyn Sandbox,
    cx: &ToolCx,
) -> Result<Vec<Row>, String> {
    let paths: Vec<String> = match live_index(cx) {
        Some(index) => index
            .tree()
            .glob("*", Path::new("."))
            .iter()
            .map(|p| p.display().to_string())
            .collect(),
        None => walk_files(sb, Path::new("."))
            .await?
            .iter()
            .map(|p| display_rel(sb, p))
            .collect(),
    };
    let needle: Vec<char> = query
        .chars()
        .filter(|c| !c.is_whitespace())
        .map(lower)
        .collect();
    let mut scored: Vec<(i32, &String)> = paths
        .iter()
        .filter_map(|p| fuzzy_score(&needle, p).map(|s| (s, p)))
        .collect();
    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then(a.1.len().cmp(&b.1.len()))
            .then(a.1.cmp(b.1))
    });
    Ok(scored
        .into_iter()
        .take(n)
        .map(|(_, p)| Row {
            loc: format!("{p}:1"),
            kind: "path".into(),
            snippet: "(file)".into(),
        })
        .collect())
}

/// The first line of `text`, trimmed and cut to a readable length.
fn snippet(text: &str) -> String {
    let line = text.lines().next().unwrap_or("").trim();
    match line.char_indices().nth(SNIPPET_CHARS) {
        Some((i, _)) => format!("{}…", &line[..i]),
        None => line.to_string(),
    }
}

fn lower(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

const MATCH: i32 = 1;
const SEGMENT_START: i32 = 10;
const IN_FILE_NAME: i32 = 3;
const CONSECUTIVE: i32 = 6;
const GAP: i32 = 1;
const FILE_STEM_EXACT: i32 = 20;
/// Below this a state is unreachable.
const UNREACHABLE: i32 = i32::MIN / 2;

/// How well `needle` (lowercase) matches `path` as a subsequence, or None if it does not.
/// Each matched character scores a point, more at the start of a path segment (after
/// `/ . _ -` or a camelCase hump) or inside the file name, more again when it directly
/// follows the previous match; skipped characters between matches cost a little. A file
/// whose stem is the whole needle gets a bonus.
fn fuzzy_score(needle: &[char], path: &str) -> Option<i32> {
    if needle.is_empty() {
        return None;
    }
    let text: Vec<char> = path.chars().collect();
    let low: Vec<char> = text.iter().copied().map(lower).collect();
    // Cheap rejection before the table.
    let mut it = low.iter();
    if !needle.iter().all(|n| it.any(|c| c == n)) {
        return None;
    }
    let name_start = text.iter().rposition(|&c| c == '/').map_or(0, |i| i + 1);
    let gain = |j: usize| -> i32 {
        let boundary = j == 0
            || matches!(text[j - 1], '/' | '.' | '_' | '-')
            || (text[j - 1].is_lowercase() && text[j].is_uppercase());
        MATCH
            + if boundary { SEGMENT_START } else { 0 }
            + if j >= name_start { IN_FILE_NAME } else { 0 }
    };

    let mut prev = vec![UNREACHABLE; text.len()];
    for (i, &want) in needle.iter().enumerate() {
        let mut cur = vec![UNREACHABLE; text.len()];
        // Best score of the previous needle character ending at any earlier position,
        // less the gap penalty for the characters skipped since.
        let mut via_gap = UNREACHABLE;
        for j in 0..text.len() {
            if i > 0 && j > 0 {
                via_gap = (via_gap - GAP).max(prev[j - 1]);
            }
            if low[j] != want {
                continue;
            }
            let before = if i == 0 {
                0
            } else {
                let adjacent = if j > 0 {
                    prev[j - 1] + CONSECUTIVE
                } else {
                    UNREACHABLE
                };
                via_gap.max(adjacent)
            };
            if before > UNREACHABLE / 2 {
                cur[j] = before + gain(j);
            }
        }
        prev = cur;
    }
    let best = prev.into_iter().filter(|&s| s > UNREACHABLE / 2).max()?;
    let stem = text[name_start..].iter().position(|&c| c == '.');
    let stem = &low[name_start..name_start + stem.unwrap_or(text.len() - name_start)];
    Some(best + if stem == needle { FILE_STEM_EXACT } else { 0 })
}
