//! Permission policy: decides whether a tool call runs, needs approval, or is denied.

use std::path::{Path, PathBuf};

use globset::{Glob, GlobMatcher};
use serde::{Deserialize, Serialize};

use crate::effect::Effect;
use crate::protected::approval_reason;
use crate::shell::{SimpleCommand, parse_commands};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// Ask before any write or command.
    Ask,
    /// Allow in-workspace edits; ask before commands.
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
    Unparseable { reason: String, raw: String },
    Paths(Vec<String>),
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
                    .map(|p| p.to_string_lossy().into_owned())
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
                    r.glob.is_none() || cmds.iter().any(|c| r.matches_str(tool, &c.argv.join(" ")))
                }
                Subjects::Unparseable { raw, .. } => r.matches_str(tool, raw) || r.glob.is_none(),
                Subjects::Paths(ps) => {
                    r.glob.is_none() || ps.iter().any(|p| r.matches_str(tool, &self.rel_str(p)))
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
            Subjects::Paths(ps) => ps.iter().find_map(|p| approval_reason(&self.root, p)),
            Subjects::Commands(cmds) => cmds
                .iter()
                .flat_map(|c| c.redirects.iter().filter(|r| r.write))
                .find_map(|r| approval_reason(&self.root, &r.target)),
            Subjects::Unparseable { reason, .. } => Some(format!("unparseable command: {reason}")),
            Subjects::None => None,
        }
    }

    fn allowed(&self, tool: &str, subjects: &Subjects) -> bool {
        let any = |s: &str| self.allow.iter().any(|r| r.matches_str(tool, s));
        match subjects {
            Subjects::Commands(cmds) => {
                !cmds.is_empty() && cmds.iter().all(|c| any(&c.argv.join(" ")))
            }
            Subjects::Paths(ps) => !ps.is_empty() && ps.iter().all(|p| any(&self.rel_str(p))),
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

    /// Path as matched by rules: relative to root when it lies inside it.
    fn rel_str(&self, p: &str) -> String {
        Path::new(p)
            .strip_prefix(&self.root)
            .map_or_else(|_| p.to_string(), |r| r.to_string_lossy().into_owned())
    }
}
