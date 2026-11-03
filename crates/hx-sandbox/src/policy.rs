//! Permission policy: decides whether a tool call runs, needs approval, or is denied.

use std::path::{Path, PathBuf};

use globset::{Glob, GlobMatcher};
use serde::{Deserialize, Serialize};

use crate::effect::Effect;
use crate::protected::{argv_word_reason, normalize, path_reason, redirect_reason};
use crate::shell::{SimpleCommand, is_dangerous_env, parse_commands};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    /// Ask before any write or command.
    Ask,
    /// Allow in-workspace edits; ask before commands.
    #[serde(alias = "auto_edit")]
    AutoEdit,
    /// Allow everything except deny rules, protected paths and unparseable commands.
    Yolo,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Ask { reason: String },
    Deny { rule: String },
}

/// Rules use the syntax `tool(glob)`; a bare `tool` matches any invocation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyConfig {
    pub mode: Mode,
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub deny: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum PolicyError {
    #[error("malformed rule {rule:?}: {why}")]
    MalformedRule { rule: String, why: String },
}

/// Tools whose effect is always `ReadOnly` and carries no paths, so a glob could never match.
const READ_ONLY_TOOLS: &[&str] = &[
    "read", "grep", "glob", "outline", "symbol", "refs", "recall",
];

#[derive(Debug)]
struct Rule {
    text: String,
    tool: String,
    /// `None` matches any invocation of the tool.
    glob: Option<GlobMatcher>,
}

impl Rule {
    fn parse(text: &str) -> Result<Rule, PolicyError> {
        let bad = |why: &str| PolicyError::MalformedRule {
            rule: text.to_string(),
            why: why.to_string(),
        };
        let (tool, glob) = match text.find(['(', ')']) {
            None => (text, None),
            Some(i) if text.as_bytes()[i] == b'(' => {
                let inner = text[i + 1..]
                    .strip_suffix(')')
                    .ok_or_else(|| bad("expected closing ')' at end"))?;
                if inner.contains(['(', ')']) {
                    return Err(bad("unexpected parenthesis in pattern"));
                }
                let m = Glob::new(inner).map_err(|e| bad(&e.to_string()))?;
                (&text[..i], Some(m.compile_matcher()))
            }
            Some(_) => return Err(bad("unexpected ')'")),
        };
        if tool.is_empty()
            || !tool
                .chars()
                .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
        {
            return Err(bad("tool name must be non-empty alphanumeric"));
        }
        if glob.is_some() && READ_ONLY_TOOLS.contains(&tool) {
            return Err(bad(&format!(
                "path-scoped rules are not supported for read-only tool {tool}; use bare `{tool}`"
            )));
        }
        Ok(Rule {
            text: text.to_string(),
            tool: tool.to_string(),
            glob,
        })
    }

    fn matches_str(&self, tool: &str, s: &str) -> bool {
        self.tool == tool && self.glob.as_ref().is_none_or(|g| g.is_match(s))
    }
}

/// What a tool call touches, normalized for rule matching.
enum Subjects {
    Commands(Vec<SimpleCommand>),
    Unparseable {
        reason: String,
        raw: String,
    },
    /// Raw path and its root-relative lexical normalization, if inside the root.
    Paths(Vec<(String, Option<PathBuf>)>),
    None,
}

#[derive(Debug)]
pub struct Policy {
    mode: Mode,
    allow: Vec<Rule>,
    deny: Vec<Rule>,
    root: PathBuf,
}

impl Policy {
    pub fn new(cfg: PolicyConfig, root: PathBuf) -> Result<Policy, PolicyError> {
        let parse = |v: &[String]| {
            v.iter()
                .map(|r| Rule::parse(r))
                .collect::<Result<Vec<_>, _>>()
        };
        Ok(Policy {
            mode: cfg.mode,
            allow: parse(&cfg.allow)?,
            deny: parse(&cfg.deny)?,
            root,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Order: deny rules, protected paths, unparseable commands, allow rules, mode default.
    pub fn decide(&self, tool: &str, effect: &Effect, raw_cmd: Option<&str>) -> Decision {
        if matches!(effect, Effect::ReadOnly) {
            // Deny rules still apply to reads of a bare-tool rule.
            return self
                .denied(tool, &Subjects::None)
                .unwrap_or(Decision::Allow);
        }
        let subjects = self.subjects(effect, raw_cmd);
        if let Some(d) = self.denied(tool, &subjects) {
            return d;
        }
        if let Some(reason) = self.needs_approval(&subjects) {
            return Decision::Ask { reason };
        }
        if self.allowed(tool, &subjects) {
            return Decision::Allow;
        }
        self.mode_default(effect, &subjects)
    }

    fn subjects(&self, effect: &Effect, raw_cmd: Option<&str>) -> Subjects {
        match effect {
            Effect::ReadOnly => Subjects::None,
            Effect::Write { paths } => Subjects::Paths(
                paths
                    .iter()
                    .map(|p| {
                        let raw = p.to_string_lossy().into_owned();
                        let rel = normalize(&self.root, &raw);
                        (raw, rel)
                    })
                    .collect(),
            ),
            Effect::Exec { argv } => match raw_cmd {
                Some(raw) => match parse_commands(raw) {
                    Ok(cmds) => Subjects::Commands(cmds),
                    Err(e) => Subjects::Unparseable {
                        reason: e.0,
                        raw: raw.to_string(),
                    },
                },
                None => Subjects::Commands(vec![SimpleCommand {
                    assignments: Vec::new(),
                    argv: argv.clone(),
                    redirects: vec![],
                }]),
            },
        }
    }

    fn denied(&self, tool: &str, subjects: &Subjects) -> Option<Decision> {
        let deny = |r: &Rule| {
            Some(Decision::Deny {
                rule: r.text.clone(),
            })
        };
        for r in self.deny.iter().filter(|r| r.tool == tool) {
            let hit = match subjects {
                Subjects::None => r.glob.is_none(),
                Subjects::Commands(cmds) => {
                    r.glob.is_none()
                        || cmds.iter().any(|c| {
                            r.matches_str(tool, &c.argv.join(" "))
                                || r.matches_str(tool, &command_line(c))
                        })
                }
                Subjects::Unparseable { raw, .. } => r.matches_str(tool, raw) || r.glob.is_none(),
                Subjects::Paths(ps) => {
                    r.glob.is_none()
                        || ps
                            .iter()
                            .any(|(raw, rel)| r.matches_str(tool, &subject_str(raw, rel)))
                }
            };
            if hit {
                return deny(r);
            }
        }
        None
    }

    fn needs_approval(&self, subjects: &Subjects) -> Option<String> {
        match subjects {
            Subjects::Paths(ps) => ps.iter().find_map(|(raw, _)| path_reason(&self.root, raw)),
            Subjects::Commands(cmds) => cmds.iter().find_map(|c| self.command_reason(c)),
            Subjects::Unparseable { reason, .. } => Some(format!("unparseable command: {reason}")),
            Subjects::None => None,
        }
    }

    fn allowed(&self, tool: &str, subjects: &Subjects) -> bool {
        let any = |s: &str| self.allow.iter().any(|r| r.matches_str(tool, s));
        match subjects {
            Subjects::Commands(cmds) => {
                // Leading assignments are part of what the rule must match.
                !cmds.is_empty() && cmds.iter().all(|c| any(&command_line(c)))
            }
            Subjects::Paths(ps) => {
                !ps.is_empty()
                    && ps
                        .iter()
                        .all(|(_, rel)| rel.as_ref().is_some_and(|r| any(&path_str(r))))
            }
            Subjects::Unparseable { .. } | Subjects::None => false,
        }
    }

    fn mode_default(&self, effect: &Effect, subjects: &Subjects) -> Decision {
        let ask = |reason: &str| Decision::Ask {
            reason: reason.to_string(),
        };
        match (self.mode, effect) {
            (Mode::Yolo, _) => Decision::Allow,
            // Paths were already checked to be inside the workspace and unprotected.
            (Mode::AutoEdit, Effect::Write { .. }) => Decision::Allow,
            (_, Effect::Write { .. }) => ask("write requires approval"),
            _ => match subjects {
                Subjects::Commands(cmds) if cmds.is_empty() => Decision::Allow,
                _ => ask("command requires approval"),
            },
        }
    }

    /// Approval needed for one command: dangerous environment assignments,
    /// protected paths touched by write redirects or any non-flag argv word, and
    /// git config/`-c` invocations.
    fn command_reason(&self, c: &SimpleCommand) -> Option<String> {
        env_reason(c)
            .or_else(|| {
                c.redirects
                    .iter()
                    .filter(|r| r.write)
                    .find_map(|r| redirect_reason(&self.root, &r.target))
            })
            .or_else(|| {
                c.argv
                    .iter()
                    .skip(1)
                    .find_map(|w| argv_word_reason(&self.root, w))
            })
            .or_else(|| git_reason(&c.argv))
    }
}

/// The command as rules see it: leading assignments, then argv.
fn command_line(c: &SimpleCommand) -> String {
    c.assignments
        .iter()
        .chain(&c.argv)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(" ")
}

/// A dangerous `NAME=VALUE` in the command's own environment, or anywhere in its
/// argv (`env NAME=VALUE cmd`, `sudo NAME=VALUE cmd`, `export NAME=VALUE`, `make
/// NAME=VALUE` all pass it on).
fn env_reason(c: &SimpleCommand) -> Option<String> {
    c.assignments
        .iter()
        .map(String::as_str)
        .chain(c.argv.iter().skip(1).map(String::as_str))
        .find_map(|w| {
            let (name, _) = w.split_once('=')?;
            let name = name.strip_suffix('+').unwrap_or(name);
            let ident = !name.is_empty()
                && !name.starts_with(|ch: char| ch.is_ascii_digit())
                && name
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || ch == '_');
            (ident && is_dangerous_env(name))
                .then(|| format!("sets environment variable {name}, which can run other code"))
        })
}

fn path_str(rel: &Path) -> String {
    rel.to_string_lossy().into_owned()
}

/// The string path rules match: normalized when possible, else the raw path.
fn subject_str(raw: &str, rel: &Option<PathBuf>) -> String {
    rel.as_deref().map_or_else(|| raw.to_string(), path_str)
}

/// `git config` that may set values, or any `git -c`, can plant hooks/aliases.
fn git_reason(argv: &[String]) -> Option<String> {
    let i = argv
        .iter()
        .position(|w| w == "git" || w.ends_with("/git"))?;
    let mut args = argv[i + 1..].iter();
    while let Some(a) = args.next() {
        if a == "-C" {
            args.next();
        } else if a == "-c" || (a.starts_with("-c") && !a.starts_with("--")) {
            return Some("git -c overrides configuration".into());
        } else if !a.starts_with('-') {
            if a != "config" {
                return None;
            }
            const READ: &[&str] = &["--get", "--get-all", "--get-regexp", "--list", "-l"];
            let rest: Vec<&String> = args.collect();
            return if rest.iter().any(|r| READ.contains(&r.as_str())) {
                None
            } else {
                Some("git config may modify configuration".into())
            };
        }
    }
    None
}
