//! Static shell command parser built on tree-sitter-bash.
//!
//! Splits a shell string into [`SimpleCommand`]s (pipelines, lists, subshells,
//! command/process substitutions and compound statements are all flattened) so
//! the permission policy can judge each one. Anything that cannot be analyzed
//! statically is reported as [`Unparseable`].

use tree_sitter::{Node, Parser};

use crate::runners::{base, reject_unanalyzable, runs_module};

/// A single command invocation with its words unquoted. Variable expansions
/// stay literal (`$X`). Wrappers such as `sudo` or `env` are not unwrapped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimpleCommand {
    /// Leading `NAME=VALUE` words (environment for this command only), verbatim source text.
    pub assignments: Vec<String>,
    pub argv: Vec<String>,
    pub redirects: Vec<Redirect>,
}

/// A file redirection. `write` is true for `>`, `>>`, `&>`, `>|` and `<>`.
/// fd duplications (`2>&1`) and heredocs/herestrings are not file targets and
/// are not listed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redirect {
    pub target: String,
    pub write: bool,
}

/// The input cannot be statically analyzed; the caller should treat it as
/// requiring explicit approval.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unparseable shell command: {0}")]
pub struct Unparseable(pub String);

/// Syntax-tree depth beyond which input is refused rather than walked; keeps the
/// recursive walk far from the stack limit on crafted input.
const MAX_DEPTH: usize = 256;

/// Parse `src` into the flat list of simple commands it would run.
pub fn parse_commands(src: &str) -> Result<Vec<SimpleCommand>, Unparseable> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_bash::LANGUAGE.into())
        .map_err(|e| Unparseable(format!("bash grammar unavailable: {e}")))?;
    let tree = parser
        .parse(src, None)
        .ok_or_else(|| Unparseable("parser produced no tree".into()))?;
    let root = tree.root_node();
    if root.has_error() {
        return Err(Unparseable(describe_error(root, src)));
    }
    let mut out = Vec::new();
    walk(root, src.as_bytes(), &mut out, 0)?;
    if out.is_empty() && !src.trim().is_empty() {
        // Fail closed: only plain assignments and comments may yield nothing.
        let mut cur = root.walk();
        let benign = root
            .named_children(&mut cur)
            .all(|n| matches!(n.kind(), "variable_assignment" | "comment"));
        if !benign {
            return Err(Unparseable("no analyzable command found".into()));
        }
    }
    Ok(out)
}

/// Locate the first error or missing node without recursion (the tree may be
/// arbitrarily deep).
fn describe_error(root: Node, src: &str) -> String {
    let mut stack = vec![root];
    let mut found = None;
    while let Some(n) = stack.pop() {
        if n.is_error() || n.is_missing() {
            found = Some(n);
            break;
        }
        let mut c = n.walk();
        let children: Vec<Node> = n.children(&mut c).collect();
        stack.extend(children.into_iter().rev());
    }
    match found {
        Some(n) => {
            let p = n.start_position();
            let what = if n.is_missing() {
                format!("missing `{}`", n.kind())
            } else {
                format!("syntax error near `{}`", snippet(&src[n.byte_range()]))
            };
            format!("{what} at line {} column {}", p.row + 1, p.column + 1)
        }
        None => "syntax error".into(),
    }
}

fn snippet(s: &str) -> String {
    s.chars().take(20).collect()
}

/// Node kinds that are walked into without further checks.
const STRUCTURAL: &[&str] = &[
    "program",
    "list",
    "subshell",
    "if_statement",
    "elif_clause",
    "else_clause",
    "while_statement",
    "case_statement",
    "case_item",
    "do_group",
    "negated_command",
    "command_substitution",
    "process_substitution",
    "concatenation",
    "string",
    "translated_string",
    "simple_expansion",
    "variable_assignments",
    "heredoc_body",
    "brace_expression",
    "command_name",
    "parenthesized_expression",
    "ternary_expression",
    "postfix_expression",
];

/// Node kinds that cannot contain commands.
const LEAVES: &[&str] = &[
    "word",
    "number",
    "raw_string",
    "ansi_c_string",
    "string_content",
    "comment",
    "variable_name",
    "special_variable_name",
    "file_descriptor",
    "heredoc_start",
    "heredoc_content",
    "heredoc_end",
    "regex",
    "extglob_pattern",
    "test_operator",
];

fn walk(
    node: Node,
    src: &[u8],
    out: &mut Vec<SimpleCommand>,
    depth: usize,
) -> Result<(), Unparseable> {
    if depth > MAX_DEPTH {
        return Err(Unparseable("nesting too deep to analyze".into()));
    }
    let depth = depth + 1;
    match node.kind() {
        "command" => walk_command(node, src, out, depth),
        "declaration_command" | "unset_command" => walk_declaration(node, src, out, depth),
        "test_command" => {
            out.push(SimpleCommand {
                assignments: Vec::new(),
                argv: vec!["[[".into()],
                redirects: Vec::new(),
            });
            walk_children(node, src, out, depth)
        }
        // Every element after the first reads the previous one's output.
        "pipeline" => walk_pipeline(node, src, out, depth),
        "redirected_statement" | "function_definition" => walk_redirected(node, src, out, depth),
        // Redirects are only understood in the contexts above; anywhere else
        // they could hide a write, so refuse.
        "file_redirect" | "heredoc_redirect" | "herestring_redirect" => {
            Err(Unparseable("redirection in an unsupported position".into()))
        }
        // A bare assignment changes how every later command in the input runs.
        "variable_assignment" => {
            let t = text(node, src);
            let name = t.split(['=', '[', '+']).next().unwrap_or(t);
            if is_dangerous_env(name) {
                return Err(Unparseable(format!(
                    "assignment to `{name}` changes how later commands run"
                )));
            }
            walk_children(node, src, out, depth)
        }
        // bash evaluates the *values* of variables named in arithmetic (including
        // any `$(...)` stored in them), so only literal arithmetic is analyzable.
        "arithmetic_expansion" => {
            let t = text(node, src);
            let inner = t
                .strip_prefix("$((")
                .and_then(|t| t.strip_suffix("))"))
                .or_else(|| t.strip_prefix("$[").and_then(|t| t.strip_suffix(']')))
                .unwrap_or(t);
            require_literal_arith(inner)?;
            walk_children(node, src, out, depth)
        }
        "compound_statement" => {
            let t = text(node, src);
            if let Some(inner) = t.strip_prefix("((") {
                require_literal_arith(inner.strip_suffix("))").unwrap_or(inner))?;
            }
            walk_children(node, src, out, depth)
        }
        // `for NAME in ...` / `select NAME in ...` assign NAME.
        "for_statement" => {
            if let Some(var) = node.child_by_field_name("variable") {
                reject_dangerous_name(text(var, src))?;
            }
            walk_children(node, src, out, depth)
        }
        "c_style_for_statement" => {
            let mut cur = node.walk();
            for child in node.named_children(&mut cur) {
                if child.kind() != "do_group" && child.kind() != "compound_statement" {
                    require_literal_arith(text(child, src))?;
                }
            }
            walk_children(node, src, out, depth)
        }
        "subscript" => {
            if let Some(index) = node.child_by_field_name("index") {
                require_literal_index(text(index, src))?;
            }
            walk_children(node, src, out, depth)
        }
        "array" => {
            // `a=([i]=v)`: explicit indices are arithmetic too.
            let mut cur = node.walk();
            for child in node.named_children(&mut cur) {
                if let Some(rest) = text(child, src).strip_prefix('[') {
                    let index = rest.split_once(']').map_or(rest, |(i, _)| i);
                    require_literal_index(index)?;
                }
            }
            walk_children(node, src, out, depth)
        }
        "expansion" => {
            check_expansion(text(node, src))?;
            walk_children(node, src, out, depth)
        }
        "binary_expression" => {
            // `[[ a -eq b ]]` evaluates both operands arithmetically.
            const ARITH_TESTS: &[&str] = &["-eq", "-ne", "-lt", "-le", "-gt", "-ge"];
            let op = node.child_by_field_name("operator").map(|o| text(o, src));
            if op.is_some_and(|o| ARITH_TESTS.contains(&o)) {
                for side in ["left", "right"] {
                    if let Some(n) = node.child_by_field_name(side) {
                        require_literal_arith(text(n, src))?;
                    }
                }
            }
            walk_children(node, src, out, depth)
        }
        "unary_expression" => {
            // `[[ -v name ]]` evaluates a subscript in `name`.
            let mut cur = node.walk();
            let kids: Vec<Node> = node.children(&mut cur).collect();
            if kids.first().is_some_and(|o| text(*o, src) == "-v") {
                for k in &kids[1..] {
                    require_static_name(&unquote(*k, src))?;
                }
            }
            walk_children(node, src, out, depth)
        }
        k if STRUCTURAL.contains(&k) => walk_children(node, src, out, depth),
        k if LEAVES.contains(&k) || !node.is_named() => Ok(()),
        other => Err(Unparseable(format!(
            "unsupported shell construct `{other}`"
        ))),
    }
}

fn walk_command(
    node: Node,
    src: &[u8],
    out: &mut Vec<SimpleCommand>,
    depth: usize,
) -> Result<(), Unparseable> {
    let mut assignments = Vec::new();
    let mut argv = Vec::new();
    let mut redirects = Vec::new();
    let mut extra = Vec::new();
    let mut rest = Vec::new();
    let mut fed = false;
    let mut cur = node.walk();
    for child in node.children(&mut cur) {
        fed |= feeds_stdin(child, src);
        match child.kind() {
            "command_name" => argv.push(unquote(child, src)),
            "variable_assignment" => {
                assignments.push(text(child, src).to_string());
                rest.push(child);
            }
            "file_redirect" | "heredoc_redirect" | "herestring_redirect" => {
                collect_redirects(child, src, &mut redirects, &mut extra, depth)?
            }
            "comment" => {}
            _ if child.is_named() => {
                argv.push(unquote(child, src));
                rest.push(child);
            }
            _ => {}
        }
    }
    if argv.is_empty() {
        if redirects.iter().any(|r| r.write) {
            return Err(Unparseable("redirection without a command".into()));
        }
    } else {
        reject_unanalyzable(&argv)?;
        reject_dynamic_names(&argv)?;
        if fed && runs_module(&argv) {
            return Err(fed_module(&argv));
        }
        out.push(SimpleCommand {
            assignments,
            argv,
            redirects,
        });
    }
    for child in rest {
        // A command's own assignments only reach its environment; the policy
        // judges them, so only look inside for substitutions.
        if child.kind() == "variable_assignment" {
            walk_children(child, src, out, depth)?;
        } else {
            walk(child, src, out, depth)?;
        }
    }
    out.append(&mut extra);
    Ok(())
}

fn walk_declaration(
    node: Node,
    src: &[u8],
    out: &mut Vec<SimpleCommand>,
    depth: usize,
) -> Result<(), Unparseable> {
    let mut argv = Vec::new();
    let mut cur = node.walk();
    for child in node.children(&mut cur) {
        if !child.is_named() || child.kind() == "variable_assignment" {
            argv.push(text(child, src).to_string());
        } else {
            argv.push(unquote(child, src));
        }
    }
    reject_dynamic_names(&argv)?;
    out.push(SimpleCommand {
        assignments: Vec::new(),
        argv,
        redirects: Vec::new(),
    });
    // Assignments here are argv words the policy judges; only look inside them.
    let mut cur = node.walk();
    for child in node.children(&mut cur) {
        if child.kind() == "variable_assignment" {
            walk_children(child, src, out, depth)?;
        } else {
            walk(child, src, out, depth)?;
        }
    }
    Ok(())
}

fn walk_pipeline(
    node: Node,
    src: &[u8],
    out: &mut Vec<SimpleCommand>,
    depth: usize,
) -> Result<(), Unparseable> {
    let mut cur = node.walk();
    let mut first = true;
    for child in node.children(&mut cur) {
        let start = out.len();
        walk(child, src, out, depth)?;
        if child.is_named() {
            if !first {
                reject_fed_modules(&out[start..])?;
            }
            first = false;
        }
    }
    Ok(())
}

fn walk_redirected(
    node: Node,
    src: &[u8],
    out: &mut Vec<SimpleCommand>,
    depth: usize,
) -> Result<(), Unparseable> {
    let start = out.len();
    let mut redirects = Vec::new();
    let mut extra = Vec::new();
    let mut fed = false;
    let mut cur = node.walk();
    for child in node.children(&mut cur) {
        fed |= feeds_stdin(child, src);
        match child.kind() {
            "file_redirect" | "heredoc_redirect" | "herestring_redirect" => {
                collect_redirects(child, src, &mut redirects, &mut extra, depth)?
            }
            _ => walk(child, src, out, depth)?,
        }
    }
    if fed {
        reject_fed_modules(&out[start..])?;
    }
    if out.len() == start {
        if redirects.iter().any(|r| r.write) {
            return Err(Unparseable("redirection without a command".into()));
        }
    } else {
        for cmd in &mut out[start..] {
            cmd.redirects.extend(redirects.iter().cloned());
        }
    }
    out.append(&mut extra);
    Ok(())
}

fn walk_children(
    node: Node,
    src: &[u8],
    out: &mut Vec<SimpleCommand>,
    depth: usize,
) -> Result<(), Unparseable> {
    let mut cur = node.walk();
    for child in node.children(&mut cur) {
        walk(child, src, out, depth)?;
    }
    Ok(())
}

/// Gather the file redirects under a redirect node into `redirects`, and any
/// commands hidden in its operands (substitutions, heredoc pipelines) into
/// `extra`.
fn collect_redirects(
    node: Node,
    src: &[u8],
    redirects: &mut Vec<Redirect>,
    extra: &mut Vec<SimpleCommand>,
    depth: usize,
) -> Result<(), Unparseable> {
    if depth > MAX_DEPTH {
        return Err(Unparseable("nesting too deep to analyze".into()));
    }
    let depth = depth + 1;
    let mut cur = node.walk();
    match node.kind() {
        "file_redirect" => {
            redirects.extend(redirect_of(node, src)?);
            walk_children(node, src, extra, depth)
        }
        "heredoc_redirect" => {
            // With an unquoted delimiter bash expands the body, and the grammar
            // misreads parts of it (`$((x))` as a subshell, `...` as text), so any
            // expansion there fails closed. Quoted delimiters keep the body literal.
            let quoted = node
                .named_children(&mut node.walk())
                .find(|c| c.kind() == "heredoc_start")
                .is_some_and(|c| text(c, src).contains(['\'', '"', '\\']));
            let body_expands = node
                .named_children(&mut node.walk())
                .any(|c| c.kind() == "heredoc_body" && text(c, src).contains(['`', '$']));
            if !quoted && body_expands {
                return Err(Unparseable("expansion in an unquoted heredoc body".into()));
            }
            for child in node.children(&mut cur) {
                match child.kind() {
                    "file_redirect" | "heredoc_redirect" | "herestring_redirect" => {
                        collect_redirects(child, src, redirects, extra, depth)?
                    }
                    _ => {
                        // `cmd <<EOF | next`: `next` reads `cmd`'s output.
                        let start = extra.len();
                        walk(child, src, extra, depth)?;
                        reject_fed_modules(&extra[start..])?;
                    }
                }
            }
            Ok(())
        }
        _ => walk_children(node, src, extra, depth),
    }
}

/// Whether a redirect feeds the command's stdin: a heredoc, a herestring, or
/// `<` / `<>` on descriptor 0.
fn feeds_stdin(node: Node, src: &[u8]) -> bool {
    match node.kind() {
        "heredoc_redirect" | "herestring_redirect" => true,
        "file_redirect" => {
            let fd = node.child_by_field_name("descriptor").map(|d| text(d, src));
            let mut cur = node.walk();
            let op = node
                .children(&mut cur)
                .find(|c| !c.is_named())
                .map(|c| text(c, src));
            matches!(op, Some("<" | "<>")) && fd.is_none_or(|d| d == "0")
        }
        _ => false,
    }
}

fn fed_module(argv: &[String]) -> Unparseable {
    Unparseable(format!(
        "`{}` runs a module whose stdin the command supplies",
        argv.join(" ")
    ))
}

/// Commands whose stdin is supplied by the command line must not run a module
/// that may execute it (`echo 'code' | python3 -m code`).
fn reject_fed_modules(cmds: &[SimpleCommand]) -> Result<(), Unparseable> {
    match cmds.iter().find(|c| runs_module(&c.argv)) {
        Some(c) => Err(fed_module(&c.argv)),
        None => Ok(()),
    }
}

fn hidden(what: &str) -> Unparseable {
    Unparseable(format!("{what} can run commands hidden in variable values"))
}

/// Arithmetic text that names no variable: numbers, operators and the
/// always-numeric special parameters (`$?`, `$#`, `$$`, `$!`, `$0`..`$9`).
fn is_literal_arith(s: &str) -> bool {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_digit() {
            let start = i;
            while i < b.len()
                && (b[i].is_ascii_alphanumeric() || matches!(b[i], b'#' | b'@' | b'_'))
            {
                i += 1;
            }
            let num = &s[start..i];
            let ok = num.bytes().all(|c| c.is_ascii_digit())
                || num
                    .strip_prefix("0x")
                    .or_else(|| num.strip_prefix("0X"))
                    .is_some_and(|h| !h.is_empty() && h.bytes().all(|c| c.is_ascii_hexdigit()))
                || num.split_once('#').is_some_and(|(base, digits)| {
                    base.bytes().all(|c| c.is_ascii_digit())
                        && !digits.is_empty()
                        && digits
                            .bytes()
                            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'@' | b'_'))
                });
            if !ok {
                return false;
            }
            continue;
        }
        if c == b'$' {
            match b.get(i + 1) {
                Some(n) if matches!(n, b'?' | b'#' | b'$' | b'!') || n.is_ascii_digit() => {
                    i += 2;
                    continue;
                }
                _ => return false,
            }
        }
        if !(c.is_ascii_whitespace() || b"+-*/%<>=!&|^~?:,()".contains(&c)) {
            return false;
        }
        i += 1;
    }
    true
}

fn require_literal_arith(s: &str) -> Result<(), Unparseable> {
    if is_literal_arith(s) {
        Ok(())
    } else {
        Err(hidden(&format!("arithmetic `{}`", snippet(s))))
    }
}

/// An array index: literal arithmetic, or `@` / `*`.
fn require_literal_index(s: &str) -> Result<(), Unparseable> {
    if matches!(s.trim(), "@" | "*") || is_literal_arith(s) {
        Ok(())
    } else {
        Err(hidden(&format!("array subscript `{}`", snippet(s))))
    }
}

/// A variable name argument (`read NAME`, `unset NAME`, `printf -v NAME`, ...):
/// an identifier with an optional literal subscript. Anything else may hide code.
fn require_static_name(s: &str) -> Result<(), Unparseable> {
    let (name, sub) = match s.split_once('[') {
        Some((n, rest)) => (n, Some(rest)),
        None => (s, None),
    };
    let ident = !name.is_empty()
        && !name.starts_with(|c: char| c.is_ascii_digit())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    let sub_ok = match sub {
        None => true,
        Some(rest) => rest
            .strip_suffix(']')
            .is_some_and(|i| require_literal_index(i).is_ok()),
    };
    if ident && sub_ok {
        Ok(())
    } else {
        Err(hidden(&format!("variable name `{}`", snippet(s))))
    }
}

/// `${...}`: indirect expansion (`${!x}`) and substring offsets (`${x:off:len}`)
/// evaluate variable values as names/arithmetic.
fn check_expansion(t: &str) -> Result<(), Unparseable> {
    let inner = t
        .strip_prefix("${")
        .and_then(|t| t.strip_suffix('}'))
        .unwrap_or(t);
    if inner.starts_with('!') {
        return Err(hidden(&format!("indirect expansion `{}`", snippet(t))));
    }
    let b = inner.as_bytes();
    let mut i = 0;
    if b.first() == Some(&b'#') && b.len() > 1 {
        i = 1; // `${#name}` length
    }
    if b.get(i)
        .is_some_and(|c| c.is_ascii_alphabetic() || *c == b'_')
    {
        while b
            .get(i)
            .is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_')
        {
            i += 1;
        }
    } else if b.get(i).is_some_and(|c| c.is_ascii_digit()) {
        while b.get(i).is_some_and(|c| c.is_ascii_digit()) {
            i += 1;
        }
    } else if i < b.len() {
        i += 1; // special parameter
    }
    if b.get(i) == Some(&b'[') {
        // The subscript node is checked on its own.
        match inner[i..].find(']') {
            Some(j) => i += j + 1,
            None => return Err(hidden(&format!("expansion `{}`", snippet(t)))),
        }
    }
    if b.get(i) == Some(&b'@') {
        // `${x@P}` expands prompt escapes (running `$(...)` in the value), and the
        // other transformations are not worth telling apart.
        return Err(hidden(&format!("transformation `{}`", snippet(t))));
    }
    if b.get(i) == Some(&b':') && !matches!(b.get(i + 1), Some(b'-' | b'=' | b'?' | b'+')) {
        require_literal_arith(&inner[i + 1..])?;
    }
    Ok(())
}

/// Builtins that take variable *names* evaluate any subscript in them, so a
/// dynamic or code-bearing name hides commands. Also `let`, integer/nameref
/// declarations and `mapfile -C` callbacks.
fn reject_dynamic_names(argv: &[String]) -> Result<(), Unparseable> {
    let cmd = base(&argv[0]);
    let args = &argv[1..];
    let names: Vec<&str> = match cmd {
        "let" => return Err(hidden("`let` arithmetic")),
        "read" => {
            let (opt_names, operands) = options(args, "a", "dinNptu");
            opt_names.into_iter().chain(operands).collect()
        }
        "mapfile" | "readarray" => {
            if args.iter().any(|a| a.starts_with('-') && a.contains('C')) {
                return Err(Unparseable(format!("`{cmd} -C` runs a callback")));
            }
            options(args, "", "dnOsuCc").1
        }
        "getopts" => args.get(1).map(String::as_str).into_iter().collect(),
        "wait" => options(args, "p", "").0,
        "printf" => options(args, "v", "").0,
        "declare" | "typeset" | "local" | "export" | "readonly" | "unset" => {
            if matches!(cmd, "declare" | "typeset" | "local")
                && args
                    .iter()
                    .any(|a| (a.starts_with('-') || a.starts_with('+')) && a.contains(['i', 'n']))
            {
                return Err(hidden(&format!("`{cmd}` integer or nameref attribute")));
            }
            args.iter()
                .filter(|a| !a.starts_with('-') && !a.starts_with('+'))
                .map(|a| a.split_once('=').map_or(a.as_str(), |(n, _)| n))
                .map(|n| n.strip_suffix('+').unwrap_or(n))
                .collect()
        }
        "[" | "test" => args
            .windows(2)
            .filter(|w| w[0] == "-v")
            .map(|w| w[1].as_str())
            .collect(),
        _ => Vec::new(),
    };
    for name in names {
        require_static_name(name)?;
        // Name operands assign the variable (`read PATH`, `export LD_PRELOAD`);
        // `unset` and `test -v` only remove or inspect it.
        if !matches!(cmd, "unset" | "[" | "test") {
            reject_dangerous_name(name)?;
        }
    }
    Ok(())
}

/// A variable assignment by name (for/select/read/export/...) to a variable that
/// can make later commands run other code.
fn reject_dangerous_name(name: &str) -> Result<(), Unparseable> {
    let name = name.split('[').next().unwrap_or(name);
    if is_dangerous_env(name) {
        Err(Unparseable(format!(
            "assignment to `{name}` changes how later commands run"
        )))
    } else {
        Ok(())
    }
}

/// Split getopt-style `args` into the values of options in `name_opts` (which
/// are variable names) and the operands. Options in `value_opts` take a value
/// that is neither.
fn options<'a>(
    args: &'a [String],
    name_opts: &str,
    value_opts: &str,
) -> (Vec<&'a str>, Vec<&'a str>) {
    let (mut names, mut operands) = (Vec::new(), Vec::new());
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--" {
            operands.extend(it.map(String::as_str));
            break;
        }
        let Some(cluster) = a.strip_prefix('-').filter(|c| !c.is_empty()) else {
            operands.push(a.as_str());
            continue;
        };
        for (i, ch) in cluster.char_indices() {
            if name_opts.contains(ch) || value_opts.contains(ch) {
                let attached = &cluster[i + ch.len_utf8()..];
                let value = if attached.is_empty() {
                    it.next().map(String::as_str)
                } else {
                    Some(attached)
                };
                if name_opts.contains(ch) {
                    names.extend(value);
                }
                break;
            }
        }
    }
    (names, operands)
}

/// Environment variables that make an otherwise harmless program load or run
/// other code (dynamic loader, shell startup, interpreters, git, tool runners).
const DANGEROUS_ENV: &[&str] = &[
    "BASH_ENV",
    "ENV",
    "PATH",
    "IFS",
    "PS4",
    "PROMPT_COMMAND",
    "NODE_OPTIONS",
    "RUBYOPT",
    "SHELLOPTS",
    "BASHOPTS",
    "HOME",
    "ZDOTDIR",
    "XDG_CONFIG_HOME",
    "EDITOR",
    "VISUAL",
    "PAGER",
    "MANPAGER",
    "LESSOPEN",
    "JAVA_TOOL_OPTIONS",
    "_JAVA_OPTIONS",
    "RUSTC_WRAPPER",
    "RUSTC_WORKSPACE_WRAPPER",
    "SSH_ASKPASS",
    "SUDO_ASKPASS",
];
/// Prefixes of dangerous environment variable families.
const DANGEROUS_ENV_PREFIXES: &[&str] = &[
    "LD_",
    "DYLD_",
    "GIT_",
    "PYTHON",
    "PERL5",
    "BASH_FUNC_",
    "CARGO_",
    "NPM_CONFIG_",
];

/// Whether assigning `name` in a command's environment can make it run other code.
/// Compared case-insensitively (some tools, e.g. npm, also read lowercase names).
pub(crate) fn is_dangerous_env(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    DANGEROUS_ENV.contains(&upper.as_str())
        || DANGEROUS_ENV_PREFIXES.iter().any(|p| upper.starts_with(p))
}

fn redirect_of(node: Node, src: &[u8]) -> Result<Option<Redirect>, Unparseable> {
    let mut op = "";
    let mut cur = node.walk();
    for child in node.children(&mut cur) {
        if !child.is_named() {
            op = text(child, src);
            break;
        }
    }
    let Some(dest) = node.child_by_field_name("destination") else {
        // `>&-` / `2>&-` close a descriptor; anything else is unknown.
        return if matches!(op, ">&-" | "<&-") {
            Ok(None)
        } else {
            Err(Unparseable(format!("redirect `{op}` without a target")))
        };
    };
    let target = unquote(dest, src);
    let dup = matches!(op, ">&" | "<&")
        && (target == "-" || (!target.is_empty() && target.bytes().all(|b| b.is_ascii_digit())));
    if dup {
        return Ok(None);
    }
    let write = !matches!(op, "<");
    Ok(Some(Redirect { target, write }))
}

fn text<'a>(node: Node, src: &'a [u8]) -> &'a str {
    std::str::from_utf8(&src[node.byte_range()]).unwrap_or("")
}

/// Shell-unquote a word node. Expansions stay as their literal source text.
fn unquote(node: Node, src: &[u8]) -> String {
    match node.kind() {
        "command_name" => match node.named_child(0) {
            Some(c) => unquote(c, src),
            None => text(node, src).to_string(),
        },
        "raw_string" => strip(text(node, src), 1, 1).to_string(),
        "ansi_c_string" => strip(text(node, src), 2, 1).to_string(),
        "string" | "translated_string" => {
            let t = text(node, src);
            let t = t.strip_prefix('$').unwrap_or(t);
            unescape(strip(t, 1, 1), "$`\"\\")
        }
        "concatenation" => {
            let mut cur = node.walk();
            node.children(&mut cur).map(|c| unquote(c, src)).collect()
        }
        "word" => unescape(text(node, src), ""),
        _ => text(node, src).to_string(),
    }
}

fn strip(s: &str, head: usize, tail: usize) -> &str {
    s.get(head..s.len().saturating_sub(tail)).unwrap_or(s)
}

/// Remove backslash escapes. With an empty `only` set every `\x` becomes `x`
/// (unquoted word); otherwise only listed characters (double quotes).
fn unescape(s: &str, only: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\\' {
            match it.peek() {
                Some('\n') => {
                    it.next();
                }
                Some(&n) if only.is_empty() || only.contains(n) => {
                    out.push(n);
                    it.next();
                }
                _ => out.push(c),
            }
        } else {
            out.push(c);
        }
    }
    out
}
