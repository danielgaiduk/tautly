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
            let mut assignments = Vec::new();
            let mut argv = Vec::new();
            let mut redirects = Vec::new();
            let mut extra = Vec::new();
            let mut rest = Vec::new();
            let mut cur = node.walk();
            for child in node.children(&mut cur) {
                match child.kind() {
                    "command_name" => argv.push(unquote(child, src)),
                    "variable_assignment" => {
                        assignments.push(text(child, src).to_string());
                        rest.push(child);
                    }
                    "file_redirect" | "heredoc_redirect" | "herestring_redirect" => {
                        collect_redirects(child, src, &mut redirects, &mut extra)?
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
                    walk_children(child, src, out)?;
                } else {
                    walk(child, src, out)?;
                }
            }
            out.append(&mut extra);
            Ok(())
        }
        "declaration_command" | "unset_command" => {
            let mut argv = Vec::new();
            let mut cur = node.walk();
            for child in node.children(&mut cur) {
                if !child.is_named() || child.kind() == "variable_assignment" {
                    argv.push(text(child, src).to_string());
                } else {
                    argv.push(unquote(child, src));
                }
            }
            out.push(SimpleCommand {
                assignments: Vec::new(),
                argv,
                redirects: Vec::new(),
            });
            // Assignments here are argv words the policy judges; only look inside them.
            let mut cur = node.walk();
            for child in node.children(&mut cur) {
                if child.kind() == "variable_assignment" {
                    walk_children(child, src, out)?;
                } else {
                    walk(child, src, out)?;
                }
            }
            Ok(())
        }
        "test_command" => {
            out.push(SimpleCommand {
                assignments: Vec::new(),
                argv: vec!["[[".into()],
                redirects: Vec::new(),
            });
            walk_children(node, src, out)
        }
        "redirected_statement" | "function_definition" => {
            let start = out.len();
            let mut redirects = Vec::new();
            let mut extra = Vec::new();
            let mut cur = node.walk();
            for child in node.children(&mut cur) {
                match child.kind() {
                    "file_redirect" | "heredoc_redirect" | "herestring_redirect" => {
                        collect_redirects(child, src, &mut redirects, &mut extra)?
                    }
                    _ => walk(child, src, out)?,
                }
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
        // A bare assignment changes how every later command in the input runs.
        "variable_assignment" => {
            let t = text(node, src);
            let name = t.split(['=', '[', '+']).next().unwrap_or(t);
            if is_dangerous_env(name) {
                return Err(Unparseable(format!(
                    "assignment to `{name}` changes how later commands run"
                )));
            }
            walk_children(node, src, out)
        }
        // Redirects are only understood in the contexts above; anywhere else
        // they could hide a write, so refuse.
        "file_redirect" | "heredoc_redirect" | "herestring_redirect" => {
            Err(Unparseable("redirection in an unsupported position".into()))
        }
        _ => walk_children(node, src, out),
    }
}

fn walk_children(node: Node, src: &[u8], out: &mut Vec<SimpleCommand>) -> Result<(), Unparseable> {
    let mut cur = node.walk();
    for child in node.children(&mut cur) {
        walk(child, src, out)?;
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
) -> Result<(), Unparseable> {
    let mut cur = node.walk();
    match node.kind() {
        "file_redirect" => {
            redirects.extend(redirect_of(node, src)?);
            walk_children(node, src, extra)
        }
        "heredoc_redirect" => {
            for child in node.children(&mut cur) {
                match child.kind() {
                    "file_redirect" | "heredoc_redirect" | "herestring_redirect" => {
                        collect_redirects(child, src, redirects, extra)?
                    }
                    _ => walk(child, src, extra)?,
                }
            }
            Ok(())
        }
        _ => walk_children(node, src, extra),
    }
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

const WRAPPERS: &[&str] = &[
    "sudo", "doas", "env", "command", "builtin", "exec", "nohup", "time", "xargs", "nice",
    "timeout", "setsid", "find",
];

fn base(s: &str) -> &str {
    s.rsplit('/').next().unwrap_or(s)
}

/// Reject commands that execute strings we cannot see, directly or through a
/// wrapper (wrapper argv is kept as written; we only look through it).
fn reject_unanalyzable(argv: &[String]) -> Result<(), Unparseable> {
    if argv[0].contains('$') || argv[0].contains('`') {
        return Err(Unparseable(format!("dynamic command name `{}`", argv[0])));
    }
    let bad = |a: &[String]| -> bool {
        match base(&a[0]) {
            "eval" | "source" | "." | "trap" | "coproc" => true,
            "bash" | "sh" | "zsh" | "dash" | "ksh" => a[1..].iter().any(|x| {
                x == "--command"
                    || (!x.starts_with("--")
                        && (x.starts_with('-') || x.starts_with('+'))
                        && x.contains('c'))
            }),
            _ => false,
        }
    };
    let wrapped = WRAPPERS.contains(&base(&argv[0]));
    // Conservative: through a wrapper, any later word may be the inner command.
    let found = bad(argv) || (wrapped && (1..argv.len()).any(|i| bad(&argv[i..])));
    if found {
        Err(Unparseable(format!(
            "`{}` runs code that cannot be statically analyzed",
            argv.join(" ")
        )))
    } else {
        Ok(())
    }
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
