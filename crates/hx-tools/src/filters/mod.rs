//! Output filters: summarize large test and build output so the model sees the
//! failures instead of thousands of lines.
//!
//! The parsers key on stable text markers (`--> file:line`, `FAILED path::name`,
//! `--- FAIL:`, `file(line,col): error TS`, stylish blocks, `●` blocks) rather than
//! on exact layout, and give up (`None`) when they find nothing, so the caller
//! can fall back to the plain head + tail cap.

use std::borrow::Cow;
use std::path::Path;

use hx_sandbox::SimpleCommand;

/// A lazily compiled regex literal.
macro_rules! re {
    ($pattern:expr) => {{
        static RE: std::sync::LazyLock<regex::Regex> =
            std::sync::LazyLock::new(|| regex::Regex::new($pattern).expect("valid regex"));
        &*RE
    }};
}

mod cargo;
mod eslint;
mod gotest;
mod jest;
mod pytest;
mod tsc;

/// Detail lines kept per item.
pub const MAX_DETAIL_LINES: usize = 20;

/// Which parser applies to a command's output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterKind {
    /// `cargo test|build|check|clippy`.
    Cargo,
    Pytest,
    /// `jest` and `vitest`.
    Jest,
    GoTest,
    Tsc,
    Eslint,
}

/// One failure or diagnostic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub name: String,
    /// `file:line[:col]` when the tool reports one.
    pub location: Option<String>,
    pub message: String,
    /// Further lines (at most [`MAX_DETAIL_LINES`] when produced by a parser).
    pub detail: Vec<String>,
}

impl Item {
    /// Blank lines are dropped and trailing whitespace trimmed before the cap, so the
    /// kept lines are all informative.
    pub(crate) fn new<S: AsRef<str>>(
        name: impl Into<String>,
        location: Option<String>,
        message: impl Into<String>,
        detail: impl IntoIterator<Item = S>,
    ) -> Self {
        Self {
            name: name.into(),
            location,
            message: message.into(),
            detail: detail
                .into_iter()
                .map(|l| l.as_ref().trim_end().to_string())
                .filter(|l| !l.is_empty())
                .take(MAX_DETAIL_LINES)
                .collect(),
        }
    }
}

/// Counts plus the failures and diagnostics found in a tool's output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    /// Tool label shown on the first line (`cargo`, `pytest`, ...).
    pub tool: &'static str,
    /// `None` when the tool does not report passes (builds, linters).
    pub passed: Option<u32>,
    pub failed: u32,
    pub errors: u32,
    pub warnings: u32,
    pub items: Vec<Item>,
}

impl Summary {
    /// The text the model sees: counts, one block per item, then where the full
    /// output was saved (`spill`).
    pub fn render(&self, spill: &Path) -> String {
        let mut out = format!("{} summary: ", self.tool);
        if let Some(p) = self.passed {
            out.push_str(&format!("{p} passed, "));
        }
        out.push_str(&format!(
            "{} failed, {} errors, {} warnings\n",
            self.failed, self.errors, self.warnings
        ));
        for item in &self.items {
            out.push_str("- ");
            out.push_str(&item.name);
            if let Some(loc) = &item.location {
                out.push_str(&format!(" ({loc})"));
            }
            if !item.message.is_empty() {
                out.push_str(": ");
                out.push_str(&item.message);
            }
            out.push('\n');
            for line in item.detail.iter().take(MAX_DETAIL_LINES) {
                out.push_str("    ");
                out.push_str(line);
                out.push('\n');
            }
        }
        out.push_str(&format!("[full output saved to {}]", spill.display()));
        out
    }
}

/// The filter for the command that produced the output, if any. Only the last
/// command of the line counts: anything after the tool (a pipe into `tail`, a
/// trailing `echo`) changes what the output is.
pub fn detect(cmds: &[SimpleCommand]) -> Option<FilterKind> {
    tool_kind(&cmds.last()?.argv)
}

fn base(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

/// `rest` without its leading option words (`-y`, `--yes`, `--`).
fn skip_flags(rest: &[String]) -> &[String] {
    let n = rest.iter().take_while(|w| w.starts_with('-')).count();
    &rest[n..]
}

fn tool_kind(argv: &[String]) -> Option<FilterKind> {
    let (first, rest) = argv.split_first()?;
    match base(first) {
        // Prefixes that run the rest of the line.
        "env" => {
            let n = rest
                .iter()
                .take_while(|w| w.starts_with('-') || w.contains('='))
                .count();
            tool_kind(&rest[n..])
        }
        "time" | "nohup" | "exec" | "command" | "nice" | "npx" | "bunx" | "pnpx" | "uvx" => {
            tool_kind(skip_flags(rest))
        }
        // Package runners: `<runner> [exec|dlx|x] <tool>`. Scripts (`run`, `test`)
        // have unknown contents.
        "npm" | "pnpm" | "yarn" | "bun" => {
            let rest = skip_flags(rest);
            match rest.split_first() {
                Some((sub, tail)) if matches!(sub.as_str(), "exec" | "dlx" | "x") => {
                    tool_kind(skip_flags(tail))
                }
                _ => tool_kind(rest),
            }
        }
        "uv" | "poetry" | "pdm" | "pipenv" | "rye" | "hatch" => {
            let rest = skip_flags(rest);
            match rest.split_first() {
                Some((sub, tail)) if sub == "run" => tool_kind(skip_flags(tail)),
                _ => None,
            }
        }
        "cargo" => {
            let rest: Vec<&String> = rest
                .iter()
                .filter(|w| !w.starts_with('+'))
                .skip_while(|w| w.starts_with('-'))
                .collect();
            matches!(
                rest.first().map(|w| w.as_str()),
                Some("test" | "build" | "check" | "clippy")
            )
            .then_some(FilterKind::Cargo)
        }
        "go" => (rest.first().map(String::as_str) == Some("test")).then_some(FilterKind::GoTest),
        "pytest" | "py.test" => Some(FilterKind::Pytest),
        "jest" | "vitest" => Some(FilterKind::Jest),
        "tsc" => Some(FilterKind::Tsc),
        "eslint" => Some(FilterKind::Eslint),
        py if py.starts_with("python") || py == "py" => python_module(rest),
        _ => None,
    }
}

/// `python [flags] -m pytest`.
fn python_module(args: &[String]) -> Option<FilterKind> {
    let mut it = args.iter();
    while let Some(w) = it.next() {
        if w == "-m" {
            return matches!(it.next().map(String::as_str), Some("pytest" | "py.test"))
                .then_some(FilterKind::Pytest);
        }
        if !w.starts_with('-') {
            return None;
        }
    }
    None
}

/// Parse `output`; `None` when it has no recognizable counts or items.
pub fn apply(kind: FilterKind, output: &str) -> Option<Summary> {
    let output = strip_ansi(output);
    match kind {
        FilterKind::Cargo => cargo::parse(&output),
        FilterKind::Pytest => pytest::parse(&output),
        FilterKind::Jest => jest::parse(&output),
        FilterKind::GoTest => gotest::parse(&output),
        FilterKind::Tsc => tsc::parse(&output),
        FilterKind::Eslint => eslint::parse(&output),
    }
}

fn strip_ansi(s: &str) -> Cow<'_, str> {
    if s.contains('\x1b') {
        re!(r"\x1b\[[0-9;?]*[ -/]*[@-~]").replace_all(s, "")
    } else {
        Cow::Borrowed(s)
    }
}

fn num(s: &str) -> u32 {
    s.parse().unwrap_or(0)
}

/// `file:line` / `file:line:col` from regex captures `file`, `line` and optional `col`.
fn location(file: &str, line: &str, col: Option<&str>) -> String {
    match col {
        Some(col) => format!("{file}:{line}:{col}"),
        None => format!("{file}:{line}"),
    }
}

/// The first line of `lines` that is not blank, trimmed, and the lines after it.
fn split_message<'a>(lines: &[&'a str]) -> (String, Vec<&'a str>) {
    match lines.iter().position(|l| !l.trim().is_empty()) {
        Some(i) => (lines[i].trim().to_string(), lines[i + 1..].to_vec()),
        None => (String::new(), Vec::new()),
    }
}

/// Every `<n> <word>` pair in `s`, e.g. `2 failed, 11 passed` -> `[("failed", 2), ...]`.
fn count_pairs(s: &str) -> Vec<(String, u32)> {
    re!(r"(\d+) ([A-Za-z]+)")
        .captures_iter(s)
        .map(|c| (c[2].to_lowercase(), num(&c[1])))
        .collect()
}
