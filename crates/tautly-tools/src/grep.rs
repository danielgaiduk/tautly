use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use regex::RegexBuilder;
use serde::Deserialize;
use serde_json::{Value, json};
use tautly_index::Candidates;
use tautly_provider::{Capabilities, ToolSpec};
use tautly_sandbox::{Effect, Sandbox};

use crate::cap::{OUTPUT_CAP, cap_text};
use crate::spec::{
    decode_text, display_rel, live_index, parse_args, tool_spec, tree_covers, walk_files,
};
use crate::{Tool, ToolCx, ToolError, ToolResult};

const MAX_MATCHES: usize = 500;
const MAX_FILE_BYTES: u64 = 10 * 1024 * 1024;

/// `grep`: searches file contents with a regular expression.
pub struct GrepTool;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    pattern: String,
    path: Option<String>,
    glob: Option<String>,
    #[serde(default)]
    case_insensitive: bool,
}

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &'static str {
        "grep"
    }

    fn spec(&self, _caps: &Capabilities) -> ToolSpec {
        tool_spec(
            "grep",
            "Search file contents with a regex. Respects .gitignore. Returns path:line:text.",
            json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "Regex to search for."},
                    "path": {"type": "string", "description": "Directory or file to search. Defaults to the project root."},
                    "glob": {"type": "string", "description": "Only search files matching this glob, e.g. *.rs."},
                    "case_insensitive": {"type": "boolean", "description": "Ignore case."}
                },
                "required": ["pattern"],
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
        let spec = SearchSpec {
            pattern: &a.pattern,
            path: a.path.as_deref(),
            glob: a.glob.as_deref(),
            case_insensitive: a.case_insensitive,
            max_matches: MAX_MATCHES,
            per_file: usize::MAX,
            max_bytes: 2 * OUTPUT_CAP,
        };
        let found = match search(&spec, sb, cx).await {
            Ok(f) => f,
            Err(e) => return ToolResult::error(e),
        };
        let note = if found.skipped > 0 {
            format!(
                "\n[skipped {} files: binary/unreadable/too large]",
                found.skipped
            )
        } else {
            String::new()
        };
        if found.matches.is_empty() {
            return ToolResult::ok(format!("no matches{note}"));
        }
        let mut out = found
            .matches
            .iter()
            .map(Match::entry)
            .collect::<Vec<_>>()
            .join("");
        out.pop();
        let (mut out, capped) = cap_text(&out, OUTPUT_CAP);
        if found.truncated || capped {
            out.push_str(&format!(
                "\n[truncated at {} matches; narrow the pattern, path or glob]",
                found.matches.len()
            ));
        }
        out.push_str(&note);
        ToolResult::ok(out)
    }
}

/// One text search: what the `grep` tool runs, and what `find` reuses for its text hits.
pub(crate) struct SearchSpec<'a> {
    pub pattern: &'a str,
    /// Directory or file to search; the project root when absent.
    pub path: Option<&'a str>,
    pub glob: Option<&'a str>,
    pub case_insensitive: bool,
    /// Stop (and report truncation) once this many matches are collected.
    pub max_matches: usize,
    /// Matches kept per file; further ones in the same file are dropped silently.
    pub per_file: usize,
    /// Stop (and report truncation) once the formatted matches exceed this many bytes.
    pub max_bytes: usize,
}

pub(crate) struct Match {
    /// Root-relative path.
    pub rel: String,
    pub line: usize,
    pub text: String,
}

impl Match {
    /// The `path:line:text` row of the grep output, with its newline.
    fn entry(&self) -> String {
        format!("{}:{}:{}\n", self.rel, self.line, self.text)
    }
}

pub(crate) struct Found {
    pub matches: Vec<Match>,
    pub truncated: bool,
    /// Files that were binary, unreadable or too large.
    pub skipped: usize,
}

/// Search with the regex in `spec`, in path order. Files come from the trigram index when
/// it can narrow the pattern and from the walker otherwise; the result is the same.
pub(crate) async fn search(
    spec: &SearchSpec<'_>,
    sb: &dyn Sandbox,
    cx: &ToolCx,
) -> Result<Found, String> {
    let re = RegexBuilder::new(spec.pattern)
        .case_insensitive(spec.case_insensitive)
        .build()
        .map_err(|e| format!("invalid regex: {e}"))?;
    let matcher = match spec.glob.map(crate::glob::build_glob) {
        None => None,
        Some(Ok(m)) => Some(m),
        Some(Err(e)) => return Err(format!("invalid glob: {e}")),
    };
    let base = spec.path.unwrap_or(".");
    let (files, from_index) = match index_candidates(spec, base, sb, cx).await? {
        Some(files) => (files, true),
        None => (walk_files(sb, Path::new(base)).await?, false),
    };

    let mut found = Found {
        matches: Vec::new(),
        truncated: false,
        skipped: 0,
    };
    let mut bytes = 0usize;
    'files: for file in files {
        if cx.cancel().is_cancelled() {
            return Err("cancelled".into());
        }
        // Regex work is per-file and short; yield so other tasks keep running.
        tokio::task::yield_now().await;
        let rel = display_rel(sb, &file);
        if matcher.as_ref().is_some_and(|m| !m.is_match(&rel)) {
            continue;
        }
        let meta = sb.metadata(&file).await;
        if from_index && meta.is_err() {
            // The file vanished since it was indexed: the walker would not have met it.
            continue;
        }
        let bytes_read = match meta.map(|m| m.len > MAX_FILE_BYTES) {
            Ok(false) => sb.read(&file).await.ok(),
            _ => None,
        };
        let Some(text) = bytes_read.as_deref().and_then(|b| decode_text(b).ok()) else {
            found.skipped += 1;
            continue;
        };
        let mut in_file = 0;
        for (i, line) in text.lines().enumerate() {
            if !re.is_match(line) {
                continue;
            }
            if in_file >= spec.per_file {
                break;
            }
            let m = Match {
                rel: rel.clone(),
                line: i + 1,
                text: line.to_string(),
            };
            if found.matches.len() >= spec.max_matches || bytes > spec.max_bytes {
                found.truncated = true;
                break 'files;
            }
            bytes += m.entry().len();
            found.matches.push(m);
            in_file += 1;
        }
    }
    Ok(found)
}

/// The files the trigram index says can match, restricted to `base` and in the order the
/// walker would visit them; None when the index cannot narrow the search.
async fn index_candidates(
    spec: &SearchSpec<'_>,
    base: &str,
    sb: &dyn Sandbox,
    cx: &ToolCx,
) -> Result<Option<Vec<PathBuf>>, String> {
    let Some(index) = live_index(cx) else {
        return Ok(None);
    };
    let base = sb.resolve(Path::new(base)).map_err(|e| e.to_string())?;
    if !tree_covers(index, &base) {
        return Ok(None);
    }
    let (index, pattern, ci) = (
        Arc::clone(index),
        spec.pattern.to_string(),
        spec.case_insensitive,
    );
    let candidates = tokio::task::spawn_blocking(move || index.grep_candidates(&pattern, ci))
        .await
        .map_err(|e| e.to_string())?;
    let Candidates::Files(rels) = candidates else {
        return Ok(None);
    };
    let mut files: Vec<PathBuf> = rels
        .iter()
        .map(|rel| sb.root().join(rel))
        .filter(|full| full.starts_with(&base))
        .collect();
    // The index sorts keys as strings (`a.rs` < `a/b.rs`); the walker sorts as paths.
    files.sort();
    Ok(Some(files))
}
