//! Languages known to the index and the syntax check, chosen by file extension.

use std::path::Path;

use tree_sitter::Language;

/// A language the index can parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lang {
    /// `.rs`
    Rust,
    /// `.ts`
    TypeScript,
    /// `.tsx`
    Tsx,
    /// `.js`
    JavaScript,
    /// `.py`
    Python,
    /// `.go`
    Go,
    /// `.json` (syntax check only)
    Json,
    /// `.toml` (syntax check only)
    Toml,
    /// `.yaml`, `.yml` (syntax check only)
    Yaml,
}

impl Lang {
    /// Every language, in declaration order (the order of [`Lang::code`]).
    const ALL: [Lang; 9] = [
        Lang::Rust,
        Lang::TypeScript,
        Lang::Tsx,
        Lang::JavaScript,
        Lang::Python,
        Lang::Go,
        Lang::Json,
        Lang::Toml,
        Lang::Yaml,
    ];

    /// A one-byte code for an optional language (0: none), as the tree stores it.
    pub(crate) fn code(lang: Option<Lang>) -> u8 {
        lang.map_or(0, |l| l as u8 + 1)
    }

    /// The inverse of [`Lang::code`].
    pub(crate) fn from_code(code: u8) -> Option<Lang> {
        code.checked_sub(1)
            .and_then(|i| Self::ALL.get(usize::from(i)).copied())
    }

    /// The language for `path`'s extension (case-insensitive); None if unsupported.
    pub fn from_path(path: &Path) -> Option<Lang> {
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

    /// The tree-sitter grammar for this language.
    pub fn grammar(self) -> Language {
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

    /// Stable name stored in the `files.lang` column.
    pub fn as_str(self) -> &'static str {
        match self {
            Lang::Rust => "rust",
            Lang::TypeScript => "typescript",
            Lang::Tsx => "tsx",
            Lang::JavaScript => "javascript",
            Lang::Python => "python",
            Lang::Go => "go",
            Lang::Json => "json",
            Lang::Toml => "toml",
            Lang::Yaml => "yaml",
        }
    }

    /// Whether symbols and references are extracted (config formats are only tracked
    /// by hash).
    pub fn has_symbols(self) -> bool {
        !matches!(self, Lang::Json | Lang::Toml | Lang::Yaml)
    }
}
