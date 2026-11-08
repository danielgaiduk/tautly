//! Tree-sitter extraction of symbols and name-based references from one source file.
//!
//! Definitions come from the `queries/<lang>.scm` files (captures `@definition.<kind>`
//! and `@name`); context the query language cannot express (is this function a method,
//! which class owns it, is this `const` really a function) is decided here.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::OnceLock;

use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, Parser, Query, QueryCursor};

use crate::lang::Lang;
use crate::store::{SymbolKind, SymbolRow};

/// Longest stored signature, in characters.
const MAX_SIGNATURE_CHARS: usize = 200;
/// Identifiers longer than this are not worth indexing as references.
const MAX_REF_NAME_BYTES: usize = 128;

/// Symbols (with an empty `path`) and `(1-based line, name)` references of one file.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Extracted {
    /// The file's definitions.
    pub symbols: Vec<SymbolRow>,
    /// The file's references: 1-based line and identifier.
    pub refs: Vec<(u32, String)>,
}

/// Extract symbols and references. Languages without symbol support (and sources the
/// grammar cannot parse at all) yield an empty result; syntax errors are tolerated.
pub fn extract(lang: Lang, src: &str) -> Extracted {
    let Some(query) = query_for(lang) else {
        return Extracted::default();
    };
    let mut parser = Parser::new();
    if parser.set_language(&lang.grammar()).is_err() {
        return Extracted::default();
    }
    let Some(tree) = parser.parse(src, None) else {
        return Extracted::default();
    };
    let root = tree.root_node();

    let names = query.capture_names();
    let mut found: Vec<(usize, SymbolRow)> = Vec::new();
    let mut def_names: HashSet<(usize, usize)> = HashSet::new();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, root, src.as_bytes());
    while let Some(m) = matches.next() {
        let mut name = None;
        let mut def = None;
        let mut declarator = None;
        for c in m.captures() {
            match names[c.index as usize] {
                "name" => name = Some(c.node),
                "declarator" => declarator = Some(c.node),
                n => {
                    if let Some(kind) = n.strip_prefix("definition.") {
                        def = Some((c.node, kind));
                    }
                }
            }
        }
        let (Some(name), Some((def, kind))) = (name, def) else {
            continue;
        };
        if let Some(row) = build_symbol(lang, src, name, def, kind, declarator) {
            def_names.insert((name.start_byte(), name.end_byte()));
            found.push((def.start_byte(), row));
        }
    }
    found.sort_by(|a, b| (a.0, &a.1.name).cmp(&(b.0, &b.1.name)));
    found.dedup_by(|a, b| a.0 == b.0 && a.1.name == b.1.name);

    Extracted {
        symbols: found.into_iter().map(|(_, s)| s).collect(),
        refs: references(root, src, &def_names),
    }
}

fn query_for(lang: Lang) -> Option<&'static Query> {
    static CACHE: [OnceLock<Option<Query>>; 9] = [const { OnceLock::new() }; 9];
    let (slot, source) = match lang {
        Lang::Rust => (0, include_str!("../queries/rust.scm")),
        Lang::TypeScript => (1, include_str!("../queries/typescript.scm")),
        Lang::Tsx => (2, include_str!("../queries/typescript.scm")),
        Lang::JavaScript => (3, include_str!("../queries/javascript.scm")),
        Lang::Python => (4, include_str!("../queries/python.scm")),
        Lang::Go => (5, include_str!("../queries/go.scm")),
        Lang::Json | Lang::Toml | Lang::Yaml => return None,
    };
    CACHE[slot]
        .get_or_init(|| Query::new(&lang.grammar(), source).ok())
        .as_ref()
}

fn text<'a>(node: Node, src: &'a str) -> &'a str {
    src.get(node.byte_range()).unwrap_or("")
}

fn build_symbol(
    lang: Lang,
    src: &str,
    name: Node,
    def: Node,
    kind: &str,
    declarator: Option<Node>,
) -> Option<SymbolRow> {
    let mut sig_end = None;
    let mut parent = None;
    let kind = match kind {
        "function" => match method_parent(lang, def, src) {
            Some(p) => {
                parent = p;
                SymbolKind::Method
            }
            None => SymbolKind::Function,
        },
        "method" => {
            // An object-literal method is not a definition of its own.
            parent = method_parent(lang, def, src)?;
            SymbolKind::Method
        }
        "type" => SymbolKind::Type,
        "module" => SymbolKind::Module,
        "constant" => {
            if inside_function(def) {
                return None;
            }
            match declarator {
                Some(decl) => {
                    let value = decl.child_by_field_name("value");
                    match value.filter(|v| is_function_value(v.kind())) {
                        Some(f) => {
                            sig_end = Some(body_start(f));
                            SymbolKind::Function
                        }
                        None if text(def, src).starts_with("const") => SymbolKind::Constant,
                        None => return None,
                    }
                }
                None => {
                    if lang == Lang::Python && !is_upper_case(text(name, src)) {
                        return None;
                    }
                    SymbolKind::Constant
                }
            }
        }
        _ => return None,
    };
    let def_text = text(def, src);
    Some(SymbolRow {
        path: PathBuf::new(),
        name: text(name, src).to_string(),
        kind,
        parent,
        start_line: def.start_position().row as u32 + 1,
        end_line: def.end_position().row as u32 + 1,
        signature: signature(lang, src, def, sig_end),
        hash: blake3::hash(def_text.as_bytes()).to_hex().to_string(),
    })
}

fn body_start(node: Node) -> usize {
    node.child_by_field_name("body")
        .map_or(node.end_byte(), |b| b.start_byte())
}

fn is_function_value(kind: &str) -> bool {
    matches!(
        kind,
        "arrow_function" | "function_expression" | "function" | "generator_function"
    )
}

fn is_upper_case(name: &str) -> bool {
    name.chars().any(|c| c.is_ascii_uppercase())
        && !name.starts_with(|c: char| c.is_ascii_digit())
        && name
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

fn inside_function(node: Node) -> bool {
    let mut cur = node.parent();
    while let Some(n) = cur {
        if matches!(
            n.kind(),
            "function_item"
                | "function_declaration"
                | "function_definition"
                | "generator_function_declaration"
                | "method_definition"
                | "method_declaration"
                | "arrow_function"
                | "function_expression"
                | "closure_expression"
        ) {
            return true;
        }
        cur = n.parent();
    }
    false
}

/// Source from the definition start up to its body, whitespace-collapsed and capped.
fn signature(lang: Lang, src: &str, def: Node, end: Option<usize>) -> String {
    let start = def.start_byte();
    let end = end.unwrap_or_else(|| body_start(def)).max(start);
    let prefix = match (lang, def.kind()) {
        (Lang::Go, "type_spec") => "type ",
        (Lang::Go, "const_spec") => "const ",
        _ => "",
    };
    let body = src.get(start..end).unwrap_or("");
    let collapsed = format!(
        "{prefix}{}",
        body.split_whitespace().collect::<Vec<_>>().join(" ")
    );
    collapsed.chars().take(MAX_SIGNATURE_CHARS).collect()
}

/// `Some(parent)` when `node` is a method: directly inside an impl/trait body, class
/// body or (Go) declared with a receiver. The inner option is None for anonymous
/// classes.
fn method_parent(lang: Lang, node: Node, src: &str) -> Option<Option<String>> {
    let name_of = |n: Node| {
        n.child_by_field_name("name")
            .map(|c| text(c, src).to_string())
    };
    match lang {
        Lang::Rust => {
            let body = node.parent().filter(|p| p.kind() == "declaration_list")?;
            let owner = body.parent()?;
            match owner.kind() {
                "impl_item" => Some(
                    owner
                        .child_by_field_name("type")
                        .map(|t| base_type_name(text(t, src))),
                ),
                "trait_item" => Some(name_of(owner)),
                _ => None,
            }
        }
        Lang::TypeScript | Lang::Tsx | Lang::JavaScript => {
            let body = node.parent().filter(|p| p.kind() == "class_body")?;
            Some(name_of(body.parent()?))
        }
        Lang::Python => {
            let mut p = node.parent()?;
            if p.kind() == "decorated_definition" {
                p = p.parent()?;
            }
            let class = p
                .parent()
                .filter(|_| p.kind() == "block")
                .filter(|c| c.kind() == "class_definition")?;
            Some(name_of(class))
        }
        Lang::Go => {
            if node.kind() != "method_declaration" {
                return None;
            }
            let receiver = node.child_by_field_name("receiver")?;
            let mut cur = receiver.walk();
            let ty = receiver
                .named_children(&mut cur)
                .find(|c| c.kind() == "parameter_declaration")
                .and_then(|p| p.child_by_field_name("type"));
            Some(ty.map(|t| base_type_name(text(t, src))))
        }
        Lang::Json | Lang::Toml | Lang::Yaml => None,
    }
}

/// `&mut Foo<T>`, `*Foo[T]`, `a::Foo` -> `Foo`.
fn base_type_name(ty: &str) -> String {
    let ty = ty.trim().trim_start_matches(['&', '*']).trim_start();
    let ty = ty.strip_prefix("mut ").unwrap_or(ty);
    let ty = ty.split(['<', '[']).next().unwrap_or(ty);
    ty.rsplit("::").next().unwrap_or(ty).trim().to_string()
}

fn is_ref_kind(kind: &str) -> bool {
    matches!(
        kind,
        "identifier"
            | "type_identifier"
            | "field_identifier"
            | "property_identifier"
            | "shorthand_property_identifier"
            | "shorthand_property_identifier_pattern"
    )
}

/// Every identifier leaf that is not the name of a definition, one per (line, name),
/// sorted by line then name. Iterative: a syntax tree can be arbitrarily deep.
fn references(root: Node, src: &str, def_names: &HashSet<(usize, usize)>) -> Vec<(u32, String)> {
    let mut out: Vec<(u32, String)> = Vec::new();
    let mut cursor = root.walk();
    'walk: loop {
        let node = cursor.node();
        if node.child_count() == 0
            && is_ref_kind(node.kind())
            && !def_names.contains(&(node.start_byte(), node.end_byte()))
        {
            let name = text(node, src);
            if !name.is_empty() && name.len() <= MAX_REF_NAME_BYTES {
                out.push((node.start_position().row as u32 + 1, name.to_string()));
            }
        }
        if cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                continue 'walk;
            }
            if !cursor.goto_parent() {
                break 'walk;
            }
        }
    }
    out.sort();
    out.dedup();
    out
}
