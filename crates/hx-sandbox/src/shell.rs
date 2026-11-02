//! Static shell command parser built on tree-sitter-bash.
//!
//! Splits a shell string into [`SimpleCommand`]s (pipelines, lists, subshells,
//! command/process substitutions and compound statements are all flattened) so
//! the permission policy can judge each one. Anything that cannot be analyzed
//! statically is reported as [`Unparseable`].

use tree_sitter::{Node, Parser};

/// A single command invocation with its words unquoted. Variable expansions
/// stay literal (`$X`). Wrappers such as `sudo` or `env` are not unwrapped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimpleCommand {
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
    walk(root, src.as_bytes(), &mut out)?;
    Ok(out)
}

fn describe_error(root: Node, src: &str) -> String {
    fn find<'a>(n: Node<'a>) -> Option<Node<'a>> {
        if n.is_error() || n.is_missing() {
            return Some(n);
        }
        let mut c = n.walk();
        n.children(&mut c).find_map(find)
    }
    match find(root) {
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

fn walk(node: Node, src: &[u8], out: &mut Vec<SimpleCommand>) -> Result<(), Unparseable> {
    match node.kind() {
        "command" => {
            let mut argv = Vec::new();
            let mut redirects = Vec::new();
            let mut cur = node.walk();
            for child in node.children(&mut cur) {
                match child.kind() {
                    "command_name" => argv.push(unquote(child, src)),
                    "file_redirect" => redirects.extend(redirect_of(child, src)),
                    "variable_assignment" | "comment" => {}
                    k if child.is_named() && !k.ends_with("_redirect") => {
                        argv.push(unquote(child, src))
                    }
                    _ => {}
                }
            }
            if !argv.is_empty() {
                reject_unanalyzable(&argv)?;
                out.push(SimpleCommand { argv, redirects });
            }
            walk_children(node, src, out)
        }
        "redirected_statement" => {
            let start = out.len();
            let mut redirects = Vec::new();
            let mut body = None;
            let mut cur = node.walk();
            for child in node.children(&mut cur) {
                match child.kind() {
                    "file_redirect" => redirects.extend(redirect_of(child, src)),
                    "heredoc_redirect" | "herestring_redirect" => {}
                    _ if child.is_named() && body.is_none() => body = Some(child),
                    _ => {}
                }
            }
            if let Some(b) = body {
                walk(b, src, out)?;
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
            // Redirect operands and heredoc bodies may hide substitutions.
            for child in node.children(&mut cur) {
                if child.is_named() && Some(child) != body {
                    walk(child, src, out)?;
                }
            }
            Ok(())
        }
        _ => walk_children(node, src, out),
    }
}

fn walk_children(node: Node, src: &[u8], out: &mut Vec<SimpleCommand>) -> Result<(), Unparseable> {
    let mut cur = node.walk();
    for child in node.children(&mut cur) {
        // `command` children handled their own words above; descending into
        // them still finds nested substitutions.
        walk(child, src, out)?;
    }
    Ok(())
}

/// `eval`, `source`, `.` and `{bash,sh,zsh} -c` execute strings we cannot see.
fn reject_unanalyzable(argv: &[String]) -> Result<(), Unparseable> {
    let name = argv[0].rsplit('/').next().unwrap_or(&argv[0]);
    let bad = match name {
        "eval" | "source" | "." => true,
        "bash" | "sh" | "zsh" => argv[1..]
            .iter()
            .take_while(|a| a.starts_with('-') && !a.starts_with("--"))
            .any(|a| a[1..].contains('c')),
        _ => false,
    };
    if bad {
        Err(Unparseable(format!(
            "`{}` runs code that cannot be statically analyzed",
            argv[0]
        )))
    } else {
        Ok(())
    }
}

fn redirect_of(node: Node, src: &[u8]) -> Option<Redirect> {
    let mut op = "";
    let mut cur = node.walk();
    for child in node.children(&mut cur) {
        if !child.is_named() {
            op = text(child, src);
            break;
        }
    }
    let dest = node.child_by_field_name("destination")?;
    let target = unquote(dest, src);
    let dup = matches!(op, ">&" | "<&")
        && (target == "-" || (!target.is_empty() && target.bytes().all(|b| b.is_ascii_digit())));
    if dup {
        return None;
    }
    let write = matches!(op, ">" | ">>" | "&>" | "&>>" | ">|" | ">&" | "<>");
    Some(Redirect { target, write })
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
