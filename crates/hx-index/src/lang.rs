//! Languages known to the index and the syntax check, chosen by file extension.

use std::path::Path;

use tree_sitter::Language;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lang {
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
