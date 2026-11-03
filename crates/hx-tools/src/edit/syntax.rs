//! Post-edit syntax check: parse the file before and after an edit with tree-sitter and
//! report parse errors the edit introduced. The edit stays applied either way; this only
//! feeds a warning back to the model (spec 5.4 step 4).

use std::collections::HashSet;
use std::path::Path;

use tree_sitter::{Language, Node, Parser};

/// Languages with a syntax check, chosen by file extension. Kept free of edit-tool
/// types so the extension map can move to the index crate unchanged.
pub(crate) mod lang {
    use std::path::Path;

    use tree_sitter::Language;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum Lang {
        Rust,
        TypeScript,
        Tsx,
        JavaScript,
        Python,
        Go,
        Json,
        Toml,
        Yaml,
    }

    impl Lang {
        /// The language for `path`'s extension (case-insensitive); None if unsupported.
        pub(crate) fn from_path(path: &Path) -> Option<Lang> {
            let ext = path.extension()?.to_str()?.to_ascii_lowercase();
            Some(match ext.as_str() {
                "rs" => Lang::Rust,
                "ts" => Lang::TypeScript,
                "tsx" => Lang::Tsx,
                "js" => Lang::JavaScript,
                "py" => Lang::Python,
                "go" => Lang::Go,
                "json" => Lang::Json,
                "toml" => Lang::Toml,
                "yaml" | "yml" => Lang::Yaml,
                _ => return None,
            })
        }

        pub(crate) fn grammar(self) -> Language {
            match self {
                Lang::Rust => tree_sitter_rust::LANGUAGE.into(),
                Lang::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
                Lang::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
                Lang::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
                Lang::Python => tree_sitter_python::LANGUAGE.into(),
                Lang::Go => tree_sitter_go::LANGUAGE.into(),
                Lang::Json => tree_sitter_json::LANGUAGE.into(),
                Lang::Toml => tree_sitter_toml_ng::LANGUAGE.into(),
                Lang::Yaml => tree_sitter_yaml::LANGUAGE.into(),
            }
        }
    }
}

use lang::Lang;

/// Longest snippet quoted in an error message.
const SNIPPET_CHARS: usize = 30;

/// Parse errors in `after` that `before` did not already have, as 1-based
/// `(line, message)` pairs sorted by line. An error counts as pre-existing when
/// `before` had an error on the same line. `None` means the language is unsupported
/// (or its grammar could not be loaded): no check was made.
pub fn new_errors(path: &Path, before: &str, after: &str) -> Option<Vec<(usize, String)>> {
    let lang = Lang::from_path(path)?;
    let grammar = lang.grammar();
    let after_errors = errors(&grammar, after)?;
    if after_errors.is_empty() {
        return Some(Vec::new());
    }
    let known: HashSet<usize> = errors(&grammar, before)?
        .into_iter()
        .map(|(line, _)| line)
        .collect();
    Some(
        after_errors
            .into_iter()
            .filter(|(line, _)| !known.contains(line))
            .collect(),
    )
}

/// Every `ERROR` and `MISSING` node of `src`, in document order, without recursion
/// (a tree can be arbitrarily deep).
fn errors(grammar: &Language, src: &str) -> Option<Vec<(usize, String)>> {
    let mut parser = Parser::new();
    parser.set_language(grammar).ok()?;
    let tree = parser.parse(src, None)?;
    let root = tree.root_node();
    let mut out = Vec::new();
    if !root.has_error() {
        return Some(out);
    }
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.is_missing() {
            out.push((line_of(node), format!("missing `{}`", node.kind())));
        } else if node.is_error() {
            out.push((line_of(node), error_message(node, src)));
        } else if node.has_error() {
            // Children go on in reverse so they pop in document order.
            let mut cur = node.walk();
            let kids: Vec<Node> = node.children(&mut cur).collect();
            stack.extend(kids.into_iter().rev());
        }
    }
    Some(out)
}

fn line_of(node: Node) -> usize {
    node.start_position().row + 1
}

fn error_message(node: Node, src: &str) -> String {
    let text = src.get(node.byte_range()).unwrap_or("");
    let first = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let snippet: String = first.trim().chars().take(SNIPPET_CHARS).collect();
    if snippet.is_empty() {
        "syntax error".to_string()
    } else {
        format!("syntax error near `{snippet}`")
    }
}
