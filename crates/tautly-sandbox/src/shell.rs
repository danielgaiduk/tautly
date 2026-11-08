//! Static shell command parser built on tree-sitter-bash.
//!
//! Splits a shell string into [`SimpleCommand`]s (pipelines, lists, subshells,
//! command/process substitutions and compound statements are all flattened) so
//! the permission policy can judge each one. Anything that cannot be analyzed
//! statically is reported as [`Unparseable`].

use std::path::PathBuf;

use tree_sitter::{Node, Parser};

use crate::runners::{base, reject_unanalyzable, runs_module};
use crate::shell_state::{Cwd, unwrap_prefixes};

/// A single command invocation with its words unquoted. Variable expansions
/// stay literal (`$X`). Wrappers such as `sudo` or `env` are not unwrapped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimpleCommand {
    /// Leading `NAME=VALUE` words (environment for this command only), verbatim source text.
    pub assignments: Vec<String>,
    /// Every working directory the command may run in, as lexical paths relative to
    /// the workspace root (empty = the root itself) or absolute; earlier `cd`/`pushd`
    /// in the same input change it. Relative paths in the command resolve against each.
    pub cwds: Vec<PathBuf>,
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
    reject_split_continuations(root, src)?;
    let mut out = Vec::new();
    walk(root, src.as_bytes(), &mut out, &mut Cwd::default(), 0)?;
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

/// Bash deletes a backslash-newline pair before tokenizing, so `.g\<nl>it` is the
/// word `.git`; the grammar keeps the pieces apart and every later check would
/// judge the wrong words. Such a pair is refused wherever bash would join it: the
/// only literal places are single-quoted strings and bodies of heredocs whose
/// delimiter is quoted. A continuation cannot change any word, and stays fine, when
/// it is followed by whitespace (or the end of the input), when it follows
/// unescaped space or tab, or when it follows a shell operator token (`|`, `&&`,
/// `||`, `;`, `(`) and the next byte could not fuse with that operator.
///
/// Carriage return, form feed and vertical tab are refused on the same terms: the
/// grammar treats them as whitespace (`\<CR><LF>` as a continuation) while bash makes
/// them part of a word (or ends the command at `<CR><LF>`).
fn reject_split_continuations(root: Node, src: &str) -> Result<(), Unparseable> {
    let bytes = src.as_bytes();
    let hits: Vec<usize> = bytes
        .windows(3)
        .enumerate()
        .filter(|(_, w)| w[0] == b'\\' && w[1] == b'\n' && !w[2].is_ascii_whitespace())
        .map(|(i, _)| i)
        .filter(|&i| !separates_words(root, bytes, i))
        .collect();
    let controls: Vec<usize> = bytes
        .iter()
        .enumerate()
        .filter(|&(_, &b)| matches!(b, b'\r' | 0x0b | 0x0c))
        .map(|(i, _)| i)
        .collect();
    if hits.is_empty() && controls.is_empty() {
        return Ok(());
    }
    // Byte ranges where a backslash-newline stays literal.
    let mut literal: Vec<(usize, usize)> = Vec::new();
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        match n.kind() {
            "raw_string" => literal.push((n.start_byte(), n.end_byte())),
            "heredoc_redirect" => {
                let quoted = n
                    .named_children(&mut n.walk())
                    .find(|c| c.kind() == "heredoc_start")
                    .is_some_and(|c| text(c, bytes).contains(['\'', '"', '\\']));
                if quoted {
                    literal.extend(
                        n.named_children(&mut n.walk())
                            .filter(|c| c.kind() == "heredoc_body")
                            .map(|c| (c.start_byte(), c.end_byte())),
                    );
                }
            }
            _ => {}
        }
        stack.extend(n.children(&mut n.walk()));
    }
    let outside = |i: &usize| !literal.iter().any(|&(s, e)| s <= *i && *i < e);
    if controls.iter().any(outside) {
        return Err(Unparseable(
            "carriage return, form feed or vertical tab changes how bash splits words".into(),
        ));
    }
    if hits.iter().any(outside) {
        return Err(Unparseable("line continuation splits a word".into()));
    }
    Ok(())
}

/// Whether the backslash-newline at `i` (followed by a non-whitespace byte) sits
/// where bash and the grammar agree on the tokens: at the start of the input, after
/// an unescaped space or tab, or right after an operator token that the next byte
/// cannot extend (`|` + `|` would be `||` to bash, `(` + `(` would be `((`).
fn separates_words(root: Node, src: &[u8], i: usize) -> bool {
    let Some(&prev) = i.checked_sub(1).and_then(|p| src.get(p)) else {
        return true;
    };
    if matches!(prev, b' ' | b'\t') {
        let escapes = src[..i - 1]
            .iter()
            .rev()
            .take_while(|&&b| b == b'\\')
            .count();
        return escapes % 2 == 0;
    }
    if matches!(prev, b'|' | b'&' | b';' | b'(') && !b"|&;()<>".contains(&src[i + 2]) {
        // An operator token of its own, not an escaped or quoted lookalike.
        return root.descendant_for_byte_range(i - 1, i).is_some_and(|n| {
            !n.is_named() && n.end_byte() == i && matches!(n.kind(), "|" | "&&" | "||" | ";" | "(")
        });
    }
    false
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
    "if_statement",
    "elif_clause",
    "else_clause",
    "case_statement",
    "case_item",
    "do_group",
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
    cx: &mut Cwd,
    depth: usize,
) -> Result<(), Unparseable> {
    if depth > MAX_DEPTH {
        return Err(Unparseable("nesting too deep to analyze".into()));
    }
    let depth = depth + 1;
    match node.kind() {
        "command" => walk_command(node, src, out, cx, depth),
        "declaration_command" | "unset_command" => walk_declaration(node, src, out, cx, depth),
        "test_command" => {
            // `[ … ]` is the ordinary `[` command, so `>` and `<` inside it are redirects
            // (`[ a > Z ]` truncates `Z`); only `[[ … ]]` compares. The grammar reads both
            // as comparisons, so a single-bracket test holding one fails closed.
            let single = node.child(0).is_some_and(|c| text(c, src) == "[");
            if single && holds_redirect(node, src) {
                return Err(Unparseable(
                    "redirect inside `[ … ]`: the `[` command, unlike `[[ … ]]`, redirects on \
                     `>` and `<`"
                        .into(),
                ));
            }
            cx.push(
                out,
                SimpleCommand {
                    cwds: vec![PathBuf::new()],
                    assignments: Vec::new(),
                    argv: vec!["[[".into()],
                    redirects: Vec::new(),
                },
            )?;
            walk_children(node, src, out, cx, depth)
        }
        // Every element after the first reads the previous one's output.
        "pipeline" => walk_pipeline(node, src, out, cx, depth),
        "redirected_statement" | "function_definition" => {
            walk_redirected(node, src, out, cx, depth)
        }
        // Redirects are only understood in the contexts above; anywhere else
        // they could hide a write, so refuse.
        "file_redirect" | "heredoc_redirect" | "herestring_redirect" => {
            Err(Unparseable("redirection in an unsupported position".into()))
        }
        // A bare assignment changes how every later command in the input runs.
        "variable_assignment" => {
            let t = text(node, src);
            reject_lone_dollar(t)?;
            let name = t.split(['=', '[', '+']).next().unwrap_or(t);
            if is_dangerous_env(name) {
                return Err(Unparseable(format!(
                    "assignment to `{name}` changes how later commands run"
                )));
            }
            walk_children(node, src, out, cx, depth)
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
            walk_children(node, src, out, cx, depth)
        }
        "compound_statement" => {
            let t = text(node, src);
            if let Some(inner) = t.strip_prefix("((") {
                require_literal_arith(inner.strip_suffix("))").unwrap_or(inner))?;
            }
            walk_children(node, src, out, cx, depth)
        }
        // `for NAME in ...` / `select NAME in ...` assign NAME.
        "for_statement" => {
            if let Some(var) = node.child_by_field_name("variable") {
                reject_dangerous_name(text(var, src))?;
            }
            cx.in_loop(|cx| walk_children(node, src, out, cx, depth))
        }
        "while_statement" => cx.in_loop(|cx| walk_children(node, src, out, cx, depth)),
        // Subshells and substitutions keep their `cd` to themselves.
        "subshell" | "command_substitution" | "process_substitution" => {
            let saved = cx.save();
            let r = walk_children(node, src, out, cx, depth);
            cx.restore(saved);
            r
        }
        "list" => walk_list(node, src, out, cx, depth),
        // `! cmd` succeeds exactly when `cmd` fails.
        "negated_command" => {
            walk_children(node, src, out, cx, depth)?;
            cx.assume_failure();
            Ok(())
        }
        "c_style_for_statement" => {
            let mut cur = node.walk();
            for child in node.named_children(&mut cur) {
                if child.kind() != "do_group" && child.kind() != "compound_statement" {
                    require_literal_arith(text(child, src))?;
                }
            }
            cx.in_loop(|cx| walk_children(node, src, out, cx, depth))
        }
        "subscript" => {
            if let Some(index) = node.child_by_field_name("index") {
                require_literal_index(text(index, src))?;
            }
            walk_children(node, src, out, cx, depth)
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
            walk_children(node, src, out, cx, depth)
        }
        "expansion" => {
            check_expansion(text(node, src))?;
            walk_children(node, src, out, cx, depth)
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
            walk_children(node, src, out, cx, depth)
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
            walk_children(node, src, out, cx, depth)
        }
        k if STRUCTURAL.contains(&k) => walk_children(node, src, out, cx, depth),
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
    cx: &mut Cwd,
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
        // bash joins `c\<newline>d` into one word; the grammar splits it.
        let after = &src[child.end_byte()..];
        if child.is_named()
            && after.starts_with(b"\\\n")
            && after.get(2).is_some_and(|c| !c.is_ascii_whitespace())
        {
            return Err(Unparseable(format!(
                "line continuation inside the word `{}`",
                snippet(text(child, src))
            )));
        }
        match child.kind() {
            "command_name" => argv.push(unquote(child, src)),
            "variable_assignment" => {
                reject_lone_dollar(text(child, src))?;
                assignments.push(text(child, src).to_string());
                rest.push(child);
            }
            "file_redirect" | "heredoc_redirect" | "herestring_redirect" => {
                collect_redirects(child, src, &mut redirects, &mut extra, cx, depth)?
            }
            "comment" => {}
            // `cmd 0<&3`: a digit-only word glued to a redirect operator is its
            // descriptor in bash; the grammar may leave it as a word before the redirect.
            _ if child.is_named() && is_glued_descriptor(child, src) => {}
            _ if child.is_named() => {
                argv.push(unquote(child, src));
                rest.push(child);
            }
            _ => {}
        }
    }
    if argv.is_empty() {
        // Even a read opens its target (`< /dev/tcp/host/port` connects).
        if !redirects.is_empty() {
            return Err(Unparseable("redirection without a command".into()));
        }
    } else {
        reject_unanalyzable(&argv)?;
        reject_dynamic_names(unwrap_prefixes(&argv))?;
        reject_dynamic_numbers(unwrap_prefixes(&argv))?;
        if fed && runs_module(&argv) {
            return Err(fed_module(&argv));
        }
        let effective = unwrap_prefixes(&argv).to_vec();
        cx.push(
            out,
            SimpleCommand {
                cwds: vec![PathBuf::new()],
                assignments,
                argv,
                redirects,
            },
        )?;
        cx.after_command(&effective);
    }
    for child in rest {
        // A command's own assignments only reach its environment; the policy
        // judges them, so only look inside for substitutions.
        if child.kind() == "variable_assignment" {
            walk_children(child, src, out, cx, depth)?;
        } else {
            walk(child, src, out, cx, depth)?;
        }
    }
    out.append(&mut extra);
    Ok(())
}

fn walk_declaration(
    node: Node,
    src: &[u8],
    out: &mut Vec<SimpleCommand>,
    cx: &mut Cwd,
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
    cx.push(
        out,
        SimpleCommand {
            cwds: vec![PathBuf::new()],
            assignments: Vec::new(),
            argv,
            redirects: Vec::new(),
        },
    )?;
    // Assignments here are argv words the policy judges; only look inside them.
    let mut cur = node.walk();
    for child in node.children(&mut cur) {
        if child.kind() == "variable_assignment" {
            walk_children(child, src, out, cx, depth)?;
        } else {
            walk(child, src, out, cx, depth)?;
        }
    }
    Ok(())
}

fn walk_pipeline(
    node: Node,
    src: &[u8],
    out: &mut Vec<SimpleCommand>,
    cx: &mut Cwd,
    depth: usize,
) -> Result<(), Unparseable> {
    let mut cur = node.walk();
    let mut first = true;
    for child in node.children(&mut cur) {
        let start = out.len();
        // Each element runs in its own subshell; a `cd` there does not last.
        let saved = cx.save();
        walk(child, src, out, cx, depth)?;
        cx.restore(saved);
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
    cx: &mut Cwd,
    depth: usize,
) -> Result<(), Unparseable> {
    let start = out.len();
    let mut redirects = Vec::new();
    let mut extra = Vec::new();
    let mut fed = false;
    // A function body runs later, any number of times: a `cd` in it cannot be followed.
    let function = node.kind() == "function_definition";
    cx.loops += usize::from(function);
    let mut cur = node.walk();
    for child in node.children(&mut cur) {
        fed |= feeds_stdin(child, src);
        match child.kind() {
            "file_redirect" | "heredoc_redirect" | "herestring_redirect" => {
                collect_redirects(child, src, &mut redirects, &mut extra, cx, depth)?
            }
            // Structural invariant: a redirected compound command has no words of its
            // own (a function definition has its name), so a word here (`{ cmd; } >&- a`,
            // a bash syntax error) is unknown.
            k if !function && WORDS.contains(&k) => return Err(hidden_word("the redirect")),
            _ => walk(child, src, out, cx, depth)?,
        }
    }
    cx.loops -= usize::from(function);
    // Commands after a redefinition of `cd`, `set`, ... mean something else.
    if function && let Some(name) = node.child_by_field_name("name") {
        cx.define_function(text(name, src));
    }
    if fed {
        reject_fed_modules(&out[start..])?;
    }
    if out.len() == start {
        if !redirects.is_empty() {
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

/// `a && b` runs `b` only after `a` succeeded (so after any `cd` in it); `a || b`
/// runs `b` after `a` failed, so either directory is possible.
fn walk_list(
    node: Node,
    src: &[u8],
    out: &mut Vec<SimpleCommand>,
    cx: &mut Cwd,
    depth: usize,
) -> Result<(), Unparseable> {
    let mut cur = node.walk();
    let kids: Vec<Node> = node.children(&mut cur).collect();
    let named: Vec<Node> = kids.iter().copied().filter(|k| k.is_named()).collect();
    let op = kids.iter().find(|k| !k.is_named()).map(|k| text(*k, src));
    let (Some(left), Some(right), true) = (named.first(), named.last(), named.len() == 2) else {
        return walk_children(node, src, out, cx, depth);
    };
    walk(*left, src, out, cx, depth)?;
    if op == Some("||") {
        let after_left = cx.cwds.clone();
        cx.assume_failure();
        walk(*right, src, out, cx, depth)?;
        cx.merge(after_left);
    } else {
        // Directories where the left side may have failed are kept in the state
        // and become possible again after the statement.
        walk(*right, src, out, cx, depth)?;
    }
    Ok(())
}

fn walk_children(
    node: Node,
    src: &[u8],
    out: &mut Vec<SimpleCommand>,
    cx: &mut Cwd,
    depth: usize,
) -> Result<(), Unparseable> {
    // Between statements every directory a `cd` may have left behind (it can
    // fail) becomes possible again.
    let statements = STATEMENT_LISTS.contains(&node.kind());
    let mut cur = node.walk();
    for child in node.children(&mut cur) {
        let before = cx.cwds.clone();
        walk(child, src, out, cx, depth)?;
        if statements {
            cx.end_statement(before);
        }
    }
    Ok(())
}

/// Node kinds whose children are separate statements.
const STATEMENT_LISTS: &[&str] = &[
    "program",
    "compound_statement",
    "do_group",
    "subshell",
    "if_statement",
    "elif_clause",
    "else_clause",
    "case_statement",
    "case_item",
    "while_statement",
    "for_statement",
    "c_style_for_statement",
];

/// Gather the file redirects under a redirect node into `redirects`, and any
/// commands hidden in its operands (substitutions, heredoc pipelines) into
/// `extra`.
fn collect_redirects(
    node: Node,
    src: &[u8],
    redirects: &mut Vec<Redirect>,
    extra: &mut Vec<SimpleCommand>,
    cx: &mut Cwd,
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
            walk_children(node, src, extra, cx, depth)
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
                    _ if !child.is_named() => {}
                    "file_descriptor" | "heredoc_start" | "heredoc_end" | "comment" => {}
                    "file_redirect" | "heredoc_redirect" | "herestring_redirect" => {
                        collect_redirects(child, src, redirects, extra, cx, depth)?
                    }
                    // `cmd <<EOF | next` / `cmd <<EOF && next`: `next` reads `cmd`'s
                    // output or runs after it; the body may hold substitutions.
                    k if k == "heredoc_body" || STATEMENTS.contains(&k) => {
                        let start = extra.len();
                        walk(child, src, extra, cx, depth)?;
                        reject_fed_modules(&extra[start..])?;
                    }
                    // Structural invariant: anything else on the line (`cmd <<EOF a b`)
                    // is a word bash passes to the command, which the rules could not see.
                    _ => return Err(hidden_word("the heredoc delimiter")),
                }
            }
            Ok(())
        }
        "herestring_redirect" => {
            // `<<< word`: exactly one operand; anything more is unknown.
            let operands = node
                .named_children(&mut node.walk())
                .filter(|c| !matches!(c.kind(), "file_descriptor" | "comment"))
                .count();
            if operands != 1 {
                return Err(hidden_word("the herestring"));
            }
            walk_children(node, src, extra, cx, depth)
        }
        _ => walk_children(node, src, extra, cx, depth),
    }
}

/// Node kinds that are statements of their own (the grammar's `_statement`), as a
/// heredoc line may continue with one (`cmd <<EOF | next`).
const STATEMENTS: &[&str] = &[
    "redirected_statement",
    "variable_assignment",
    "variable_assignments",
    "command",
    "declaration_command",
    "unset_command",
    "test_command",
    "negated_command",
    "for_statement",
    "c_style_for_statement",
    "while_statement",
    "if_statement",
    "case_statement",
    "pipeline",
    "list",
    "subshell",
    "compound_statement",
    "function_definition",
];

/// Node kinds that are a word (the grammar's `_literal`): one bash would pass to the
/// command, so one that must reach argv or be a redirect operand.
const WORDS: &[&str] = &[
    "word",
    "string",
    "raw_string",
    "translated_string",
    "ansi_c_string",
    "number",
    "expansion",
    "simple_expansion",
    "command_substitution",
    "process_substitution",
    "arithmetic_expansion",
    "brace_expression",
    "concatenation",
];

/// Whether `node`'s subtree holds a `<` or `>` token (any operator starting with one:
/// `>=` inside `[ … ]` is a redirect to `=`) or a redirect node.
fn holds_redirect(node: Node, src: &[u8]) -> bool {
    let redirect = if node.is_named() {
        matches!(
            node.kind(),
            "file_redirect" | "heredoc_redirect" | "herestring_redirect" | "redirected_statement"
        )
    } else {
        text(node, src).starts_with(['<', '>'])
    };
    let mut cur = node.walk();
    redirect || node.children(&mut cur).any(|c| holds_redirect(c, src))
}

/// Whether a command's word is a redirect descriptor: all digits and glued to a `<` or `>`
/// that follows it (`0<&3`), but not to a process substitution (`0<(cmd)` keeps the `0`).
fn is_glued_descriptor(node: Node, src: &[u8]) -> bool {
    let t = text(node, src);
    let after = &src[node.end_byte()..];
    !t.is_empty()
        && t.bytes().all(|b| b.is_ascii_digit())
        && (after.starts_with(b"<") || after.starts_with(b">"))
        && !(after.starts_with(b"<(") || after.starts_with(b">("))
}

/// A word the grammar filed under a redirect (`cmd > f a`, `cmd >&- a`, `cmd <<EOF a`):
/// bash passes it to the command, where the rules could not see it. Refuse (fail closed).
fn hidden_word(after: &str) -> Unparseable {
    Unparseable(format!(
        "words after {after} are arguments the rules cannot see; put them before the redirect"
    ))
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
    let name = &inner[..i];
    let name = name.strip_prefix('#').unwrap_or(name);
    let assigns = match b.get(i) {
        Some(b'=' | b'?') => true,
        Some(b':') => matches!(b.get(i + 1), Some(b'=' | b'?')),
        _ => false,
    };
    if assigns {
        // `${CDPATH:=.git}` assigns (and `?` tests) like `CDPATH=.git` would.
        reject_dangerous_name(name)?;
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

/// An assignment holding an unquoted `$` before whitespace: bash ends the word there
/// (`ex=$ rm -rf x` runs `rm`), while the grammar reads the next word into the value,
/// hiding the command name. Fails closed on any such `$` outside quotes.
fn reject_lone_dollar(t: &str) -> Result<(), Unparseable> {
    let b = t.as_bytes();
    let (mut single, mut double, mut i) = (false, false, 0);
    while i < b.len() {
        match b[i] {
            b'\\' if !single => i += 1,
            b'\'' if !double => single = !single,
            b'"' if !single => double = !double,
            b'$' if !single && !double && b.get(i + 1).is_some_and(u8::is_ascii_whitespace) => {
                return Err(Unparseable(format!(
                    "`$` before whitespace in the assignment `{}`",
                    snippet(t)
                )));
            }
            _ => {}
        }
        i += 1;
    }
    Ok(())
}

/// Builtins that take variable *names* evaluate any subscript in them, so a
/// dynamic or code-bearing name hides commands. Also `let`, integer/nameref
/// declarations and `mapfile -C` callbacks.
fn reject_dynamic_names(argv: &[String]) -> Result<(), Unparseable> {
    // Empty behind a bare prefix (`command`, `time -p`): nothing runs.
    let Some(first) = argv.first() else {
        return Ok(());
    };
    let cmd = base(first);
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

/// Numeric operands are evaluated arithmetically by bash (>= 4 evaluates
/// `a[$(cmd)]` subscripts there too), so a dynamic one may hide commands:
/// `test`/`[` with `-eq`..`-ge`, `shift`/`ulimit`/`return`/`exit`/`break`/
/// `continue`, and `printf` with a numeric conversion (or `*` width/precision,
/// or a dynamic format). Literal numbers stay fine.
fn reject_dynamic_numbers(argv: &[String]) -> Result<(), Unparseable> {
    const ARITH_TESTS: &[&str] = &["-eq", "-ne", "-lt", "-le", "-gt", "-ge"];
    let dynamic = |w: &String| w.contains(['$', '`']);
    let Some(first) = argv.first() else {
        return Ok(());
    };
    let cmd = base(first);
    let args = &argv[1..];
    let bad = match cmd {
        "test" | "[" => {
            args.iter().any(|a| ARITH_TESTS.contains(&a.as_str())) && args.iter().any(dynamic)
        }
        "shift" | "ulimit" | "return" | "exit" | "break" | "continue" => args.iter().any(dynamic),
        "printf" => {
            // Skip `-v NAME` / `-vNAME` and `--`; the next word is the format.
            let mut rest = args;
            loop {
                match rest.first().map(String::as_str) {
                    Some("-v") => rest = rest.get(2..).unwrap_or(&[]),
                    Some("--") => {
                        rest = &rest[1..];
                        break;
                    }
                    Some(a) if a.starts_with("-v") => rest = &rest[1..],
                    _ => break,
                }
            }
            match rest.split_first() {
                Some((format, values)) => {
                    (dynamic(format) || numeric_format(format)) && values.iter().any(dynamic)
                }
                None => false,
            }
        }
        _ => false,
    };
    if bad {
        Err(hidden(&format!(
            "numeric operand of `{}`",
            snippet(&argv.join(" "))
        )))
    } else {
        Ok(())
    }
}

/// Whether a printf format has a numeric conversion (`%d %i %o %u %x %X`, with
/// any length modifier), a `*` width/precision or a `%(...)T` time, whose
/// argument bash evaluates arithmetically.
fn numeric_format(f: &str) -> bool {
    let b = f.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'%' {
            i += 1;
            continue;
        }
        i += 1;
        if b.get(i) == Some(&b'%') {
            i += 1;
            continue;
        }
        if b.get(i) == Some(&b'(') {
            // `%(fmt)T` takes its argument as an integer time.
            return true;
        }
        let mut star = false;
        while let Some(&c) = b.get(i) {
            if b"-+ #0'.123456789*".contains(&c) {
                star |= c == b'*';
                i += 1;
            } else {
                break;
            }
        }
        // Length modifiers (`%ld`, `%hhu`, `%zx`, ...).
        while b.get(i).is_some_and(|c| b"hjlLqtzZI".contains(c)) {
            i += 1;
        }
        if star || matches!(b.get(i), Some(b'd' | b'i' | b'o' | b'u' | b'x' | b'X')) {
            return true;
        }
    }
    false
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
    "CDPATH",
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

/// The file redirect a `file_redirect` node describes, or `None` for an fd duplication
/// or close. Structural invariant: besides its operator and descriptor the node holds
/// exactly one word, the target, and a close operator (`>&-`, `<&-`) holds none; `cmd >
/// out.txt a b` and `cmd >&- a` run `cmd a b` and `cmd a`, and the grammar files those
/// words under the redirect, where the rules could not see them. Refuse (fail closed).
fn redirect_of(node: Node, src: &[u8]) -> Result<Option<Redirect>, Unparseable> {
    let mut op = "";
    let mut words = Vec::new();
    let mut cur = node.walk();
    for child in node.children(&mut cur) {
        match child.kind() {
            _ if !child.is_named() => {
                if op.is_empty() {
                    op = text(child, src);
                }
            }
            "file_descriptor" | "comment" => {}
            _ => words.push(child),
        }
    }
    let close = matches!(op, ">&-" | "<&-");
    if close && !words.is_empty() {
        return Err(hidden_word(&format!("`{op}`")));
    }
    if words.len() > 1 {
        return Err(hidden_word(&format!("the target of `{op}`")));
    }
    let Some(dest) = words.first() else {
        // `>&-` / `2>&-` close a descriptor; anything else is unknown.
        return if close {
            Ok(None)
        } else {
            Err(Unparseable(format!("redirect `{op}` without a target")))
        };
    };
    let target = unquote(*dest, src);
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
