//! Permission policy: decides whether a tool call runs, needs approval, or is denied.

use std::borrow::Cow;
use std::path::{Path, PathBuf};

use globset::{Glob, GlobMatcher};
use serde::{Deserialize, Serialize};

use crate::effect::Effect;
use crate::protected::{
    Resolved, argv_word_reason, normalize, path_reason, rebase, redirect_reason, resolve_links,
};
use crate::runners::{WRAPPERS, base};
use crate::shell::{SimpleCommand, is_dangerous_env, parse_commands};
use crate::shell_state::unwrap_prefixes;

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

/// MCP tools (`mcp__<server>__<tool>`) are matched by name only, and the name may be a
/// glob (`mcp__github__*`).
const MCP_PREFIX: &str = "mcp__";

#[derive(Debug)]
struct Rule {
    text: String,
    tool: String,
    /// For an MCP rule with glob characters: what tool names it matches.
    tool_glob: Option<GlobMatcher>,
    /// `None` matches any invocation of the tool.
    glob: Option<GlobMatcher>,
    /// A command rule whose pattern names a write redirect (holds ` > `): the pattern
    /// split at its first ` > `, matched structurally ([`Redirected`]).
    redirect: Option<Redirected>,
}

/// The two halves of a rule pattern that names a write redirect (`cargo test > *`): the
/// command half matches the bare command ([`command_line`]); the target half, split at
/// each further ` > ` outside quotes ([`split_targets`]), matches the command's write
/// targets one by one ([`Policy::targets_match`]). Never one flattened string, so neither
/// a target nor a quoted `'>'` argument can supply the other half's text, and no target
/// can stretch a `*` over its neighbour.
#[derive(Debug)]
struct Redirected {
    command: GlobMatcher,
    /// One pattern: every target must match it. Several: positional, the same count.
    targets: Vec<GlobMatcher>,
}

/// Split a rule's target half at each ` > ` outside single quotes. Targets are written
/// the way [`quote_word`] renders them, so `'a > b'` stays one target; a backslash
/// outside quotes escapes the next character (glob syntax, and `'\''`).
fn split_targets(half: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let (mut start, mut quoted, mut i) = (0, false, 0);
    let bytes = half.as_bytes();
    while i < bytes.len() {
        match bytes[i] {
            b'\'' => quoted = !quoted,
            b'\\' if !quoted => i += 1,
            b' ' if !quoted && half[i..].starts_with(" > ") => {
                out.push(&half[start..i]);
                start = i + 3;
                i += 2;
            }
            _ => {}
        }
        i += 1;
    }
    out.push(&half[start..]);
    out
}

/// What write target `t` names from working directory `cwd`, root-relative.
struct TargetForms {
    /// Its lexical normalization, when that lands inside the root.
    lexical: Option<String>,
    /// The path reached once symlinks on disk are followed, when that differs.
    real: Option<String>,
    /// Whether it stays inside the root, lexically and through every link.
    inside: bool,
}

impl TargetForms {
    /// Every root-relative form the target has.
    fn iter(&self) -> impl Iterator<Item = &str> {
        self.lexical.iter().chain(&self.real).map(String::as_str)
    }
}

/// The directories `c` may run in: the root when none is recorded.
fn cwds_of(c: &SimpleCommand) -> Cow<'_, [PathBuf]> {
    if c.cwds.is_empty() {
        Cow::Owned(vec![PathBuf::new()])
    } else {
        Cow::Borrowed(&c.cwds)
    }
}

impl Rule {
    fn parse(text: &str) -> Result<Rule, PolicyError> {
        let bad = |why: &str| PolicyError::MalformedRule {
            rule: text.to_string(),
            why: why.to_string(),
        };
        if text.starts_with(MCP_PREFIX) {
            if text.contains(['(', ')']) {
                return Err(bad(
                    "MCP tools are matched by name only, without parentheses",
                ));
            }
            let pattern = text.contains(['*', '?', '[']);
            let name_ok = text.chars().all(|c| {
                c.is_ascii_alphanumeric()
                    || c == '_'
                    || c == '-'
                    || (pattern && "*?[]!".contains(c))
            });
            if !name_ok {
                return Err(bad(
                    "an MCP tool name has only [A-Za-z0-9_-] (and glob characters)",
                ));
            }
            let tool_glob = if pattern {
                let g = globset::GlobBuilder::new(text)
                    .literal_separator(false)
                    .build()
                    .map_err(|e| bad(&e.to_string()))?;
                Some(g.compile_matcher())
            } else {
                None
            };
            return Ok(Rule {
                text: text.to_string(),
                tool: text.to_string(),
                tool_glob,
                glob: None,
                redirect: None,
            });
        }
        let mut redirect = None;
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
                if let Some((command, targets)) = inner.split_once(" > ") {
                    let glob = |p: &str| {
                        Glob::new(p)
                            .map(|g| g.compile_matcher())
                            .map_err(|e| bad(&e.to_string()))
                    };
                    redirect = Some(Redirected {
                        command: glob(command)?,
                        targets: split_targets(targets)
                            .into_iter()
                            .map(glob)
                            .collect::<Result<_, _>>()?,
                    });
                }
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
            tool_glob: None,
            glob,
            redirect,
        })
    }

    fn is_for(&self, tool: &str) -> bool {
        match &self.tool_glob {
            Some(g) => tool.starts_with(MCP_PREFIX) && g.is_match(tool),
            None => self.tool == tool,
        }
    }

    fn matches_str(&self, tool: &str, s: &str) -> bool {
        self.is_for(tool) && self.glob.as_ref().is_none_or(|g| g.is_match(s))
    }
}

/// What a tool call touches, normalized for rule matching.
enum Subjects {
    Commands(Vec<SimpleCommand>),
    Unparseable {
        reason: String,
        raw: String,
    },
    Paths(Vec<PathSubject>),
    /// The host a network call reaches.
    Host(String),
    None,
}

/// A write path as the tool named it, plus the root-relative forms rules match.
struct PathSubject {
    raw: String,
    /// Lexical normalization, if inside the root.
    rel: Option<PathBuf>,
    /// The path after following symlinks, when that differs from `rel`.
    real: Option<PathBuf>,
}

impl PathSubject {
    /// Every form a rule may match: the normalized path (else the raw one) and the
    /// symlink-resolved one.
    fn forms(&self) -> impl Iterator<Item = String> + '_ {
        std::iter::once(subject_str(&self.raw, &self.rel)).chain(self.real.as_deref().map(path_str))
    }
}

#[derive(Debug)]
pub struct Policy {
    mode: Mode,
    allow: Vec<Rule>,
    deny: Vec<Rule>,
    /// Allow rules added for this session ([`Policy::add_session_allow`]); memory only.
    session: Vec<Rule>,
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
            session: Vec::new(),
            root,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Add an in-memory allow rule for the rest of this policy's life ("allow for this
    /// session", spec 7.11.8). It is checked with the configured allow rules, so deny
    /// rules, protected paths and unparseable commands still win over it.
    pub fn add_session_allow(&mut self, rule: &str) -> Result<(), PolicyError> {
        let rule = Rule::parse(rule)?;
        if !self.session.iter().any(|r| r.text == rule.text) {
            self.session.push(rule);
        }
        Ok(())
    }

    /// Whether `rule` alone, as an allow rule, would allow this call: every subject the
    /// call has (each parsed command, each write path, the host) must match it. A
    /// read-only call matches a rule for its tool.
    pub fn rule_matches(
        &self,
        rule: &str,
        tool: &str,
        effect: &Effect,
        raw_cmd: Option<&str>,
    ) -> Result<bool, PolicyError> {
        let rule = Rule::parse(rule)?;
        if matches!(effect, Effect::ReadOnly) {
            return Ok(rule.is_for(tool));
        }
        let subjects = self.subjects(effect, raw_cmd);
        Ok(self.allows(&[&rule], tool, &subjects))
    }

    /// The narrowest rule that allows this call, as "allow for this session" offers it:
    /// `tool(<subject>)` with glob characters escaped (the command as rules see it, the
    /// root-relative path or the host), or the name of an MCP tool. None when no single
    /// rule can name the call: several commands, an unparseable one, several paths, a path
    /// outside the root or reached through a symlink, a read-only call, or a subject with
    /// parentheses (which a rule cannot hold). The rule returned always
    /// [matches](Self::rule_matches) the call.
    ///
    /// Every write operator is written ` > ` in the rule ([`allow_subject`]):
    /// `cargo test 2> out.txt` offers `bash(cargo test > out.txt)`, which also allows `>`,
    /// `>>` or `&>` to that same file (the same target, so nothing more is written to).
    pub fn exact_rule(&self, tool: &str, effect: &Effect, raw_cmd: Option<&str>) -> Option<String> {
        let rule = if tool.starts_with(MCP_PREFIX) {
            tool.to_string()
        } else {
            let subject = match self.subjects(effect, raw_cmd) {
                // A bare command reading ` > ` could not be told from a redirect.
                Subjects::Commands(cmds)
                    if cmds.len() == 1 && !command_line(&cmds[0]).contains(" > ") =>
                {
                    allow_subject(&cmds[0])
                }
                Subjects::Paths(ps) if ps.len() == 1 && ps[0].real.is_none() => {
                    path_str(ps[0].rel.as_ref()?)
                }
                Subjects::Host(h) => h,
                _ => return None,
            };
            if subject.is_empty() || subject.contains(['(', ')']) {
                return None;
            }
            format!("{tool}({})", escape_glob(&subject))
        };
        self.rule_matches(&rule, tool, effect, raw_cmd)
            .ok()
            .filter(|m| *m)
            .map(|_| rule)
    }

    /// Order: deny rules, protected paths, unparseable commands, allow rules (configured
    /// and session), mode default.
    ///
    /// Write paths are judged both as written and after following symlinks under
    /// the workspace root (read from disk at decision time), so a committed
    /// `NOTES.md -> .git/config` asks like `.git/config` does. The sandbox
    /// re-checks at write time (see `LocalSandbox::write_atomic`).
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
        let rules: Vec<&Rule> = self.allow.iter().chain(&self.session).collect();
        if self.allows(&rules, tool, &subjects) {
            return Decision::Allow;
        }
        self.mode_default(effect, &subjects)
    }

    fn subjects(&self, effect: &Effect, raw_cmd: Option<&str>) -> Subjects {
        match effect {
            Effect::ReadOnly => Subjects::None,
            Effect::Network { host } => Subjects::Host(host.clone()),
            Effect::Write { paths } => Subjects::Paths(
                paths
                    .iter()
                    .map(|p| {
                        let raw = p.to_string_lossy().into_owned();
                        let rel = normalize(&self.root, &raw);
                        let real = match resolve_links(&self.root, p) {
                            Some(Resolved::Inside(real)) if Some(&real) != rel.as_ref() => {
                                Some(real)
                            }
                            _ => None,
                        };
                        PathSubject { raw, rel, real }
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
                    cwds: vec![PathBuf::new()],
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
        for r in self.deny.iter().filter(|r| r.is_for(tool)) {
            let hit = match subjects {
                Subjects::None => r.glob.is_none(),
                Subjects::Commands(cmds) => {
                    r.glob.is_none() || cmds.iter().any(|c| self.deny_hits(r, tool, c))
                }
                Subjects::Unparseable { raw, .. } => r.matches_str(tool, raw) || r.glob.is_none(),
                Subjects::Paths(ps) => {
                    r.glob.is_none()
                        || ps
                            .iter()
                            .any(|p| p.forms().any(|f| r.matches_str(tool, &f)))
                }
                Subjects::Host(h) => r.matches_str(tool, h),
            };
            if hit {
                return deny(r);
            }
        }
        None
    }

    /// Whether deny rule `r` refuses command `c`. A rule that names a redirect
    /// ([`Redirected`]) needs a write redirect, its command half matching the bare command
    /// (or argv), and one of its target patterns matching some target in any spelling: as
    /// written, quoted, or root-relative from each directory the command may run in,
    /// lexically normalized and with symlinks followed ([`TargetForms`]; so
    /// `bash(* > .env)` refuses `> ./.env`, `> src/../.env` and `> envlink` when `envlink
    /// -> .env`). Any other rule matches the bare command or argv only, never a file name a
    /// redirect writes to.
    fn deny_hits(&self, r: &Rule, tool: &str, c: &SimpleCommand) -> bool {
        let forms: Vec<String> = deny_argvs(&c.argv)
            .into_iter()
            .flat_map(|a| [a.join(" "), quoted_words(a)])
            .chain(std::iter::once(command_line(c)))
            .collect();
        let Some(red) = &r.redirect else {
            return forms.iter().any(|f| r.matches_str(tool, f));
        };
        if !forms.iter().any(|f| red.command.is_match(f)) {
            return false;
        }
        write_targets(c).iter().any(|t| {
            let relative: Vec<String> = cwds_of(c)
                .iter()
                .flat_map(|cwd| {
                    let forms = self.target_forms(cwd, t);
                    forms
                        .iter()
                        .flat_map(|f| [f.to_string(), quote_word(f)])
                        .collect::<Vec<_>>()
                })
                .collect();
            [t.to_string(), quote_word(t)]
                .into_iter()
                .chain(relative)
                .any(|form| red.targets.iter().any(|g| g.is_match(&form)))
        })
    }

    /// What write target `t` names from working directory `cwd`, root-relative (see
    /// [`TargetForms`]). Links are followed on disk at decision time, as for write paths.
    fn target_forms(&self, cwd: &Path, t: &str) -> TargetForms {
        let full = rebase(cwd, t);
        let lexical = normalize(&self.root, &full);
        let (real, inside) = match resolve_links(&self.root, Path::new(&full)) {
            Some(Resolved::Inside(real)) => (
                (lexical.as_ref() != Some(&real)).then(|| path_str(&real)),
                lexical.is_some(),
            ),
            Some(Resolved::Outside(_) | Resolved::Loop) => (None, false),
            // Nothing on disk to follow (or an error on the way): the lexical form stands.
            None => (None, lexical.is_some()),
        };
        TargetForms {
            lexical: lexical.as_deref().map(path_str),
            real,
            inside,
        }
    }

    fn needs_approval(&self, subjects: &Subjects) -> Option<String> {
        match subjects {
            Subjects::Paths(ps) => ps
                .iter()
                .find_map(|p| path_reason(&self.root, Path::new(""), &p.raw)),
            Subjects::Commands(cmds) => cmds.iter().find_map(|c| self.command_reason(c)),
            Subjects::Unparseable { reason, .. } => Some(format!("unparseable command: {reason}")),
            Subjects::Host(_) | Subjects::None => None,
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
            (_, Effect::Network { host }) => {
                ask(&format!("network access to {host} requires approval"))
            }
            _ => match subjects {
                Subjects::Commands(cmds) if cmds.is_empty() => Decision::Allow,
                _ => ask("command requires approval"),
            },
        }
    }

    /// Approval needed for one command: dangerous environment assignments,
    /// protected paths touched by write redirects or any non-flag argv word, and
    /// git config/`-c` invocations.
    ///
    /// Relative paths are judged from every directory an earlier `cd` in the same
    /// input may have left the command in.
    fn command_reason(&self, c: &SimpleCommand) -> Option<String> {
        let cwds = cwds_of(c);
        env_reason(c)
            .or_else(|| {
                cwds.iter().find_map(|cwd| {
                    normalize(&self.root, &cwd.to_string_lossy())
                        .is_none()
                        .then(|| format!("runs outside the workspace after cd: {}", cwd.display()))
                })
            })
            .or_else(|| {
                cwds.iter().find_map(|cwd| {
                    c.redirects
                        .iter()
                        .filter(|r| r.write)
                        .find_map(|r| redirect_reason(&self.root, cwd, &r.target))
                        .or_else(|| {
                            c.argv
                                .iter()
                                .skip(1)
                                .find_map(|w| argv_word_reason(&self.root, cwd, w))
                        })
                })
            })
            .or_else(|| git_reason(&c.argv))
            .or_else(|| escape_reason(&c.argv))
    }
}

impl Policy {
    /// Whether `rules` allow every subject of a call of `tool`: each parsed command, each
    /// write path (as normalized inside the root, and after following symlinks), the host.
    fn allows(&self, rules: &[&Rule], tool: &str, subjects: &Subjects) -> bool {
        let any = |s: &str| rules.iter().any(|r| r.matches_str(tool, s));
        match subjects {
            Subjects::Commands(cmds) => {
                !cmds.is_empty() && cmds.iter().all(|c| self.allows_command(rules, tool, c))
            }
            Subjects::Paths(ps) => {
                !ps.is_empty()
                    && ps.iter().all(|p| {
                        p.rel.as_ref().is_some_and(|r| any(&path_str(r)))
                            && p.real.as_ref().is_none_or(|r| any(&path_str(r)))
                    })
            }
            Subjects::Host(h) => any(h),
            Subjects::Unparseable { .. } | Subjects::None => false,
        }
    }

    /// Whether one of `rules` allows command `c`, matched structurally (never one
    /// flattened string a target or a quoted `'>'` could supply text to):
    /// - a rule that names a redirect ([`Redirected`]) needs at least one write redirect,
    ///   its command half matching the bare command ([`command_line`]) and its target half
    ///   every target ([`Self::targets_match`]): `bash(cargo test > *)` allows
    ///   `cargo test > out.txt`, not `cargo test '>' x --config …` (no redirect) nor
    ///   `cargo test --release > out.txt`;
    /// - any other rule must match the bare command and, when there are write redirects,
    ///   the command with them ([`allow_subject`]): `bash(cargo test*)` allows
    ///   `cargo test > out.txt`, while `bash(* --version)` does not allow
    ///   `rm -rf src > ' --version'`.
    fn allows_command(&self, rules: &[&Rule], tool: &str, c: &SimpleCommand) -> bool {
        let bare = command_line(c);
        let targets = write_targets(c);
        let subject = allow_subject(c);
        rules
            .iter()
            .filter(|r| r.is_for(tool))
            .any(|r| match &r.redirect {
                Some(red) => {
                    !targets.is_empty()
                        && red.command.is_match(&bare)
                        && self.targets_match(red, c, &targets)
                }
                None => {
                    r.matches_str(tool, &bare)
                        && (targets.is_empty() || r.matches_str(tool, &subject))
                }
            })
    }

    /// Whether the target half of `red` covers every write target of `c`, each on its
    /// own: a half without ` > ` must match each target; one with ` > ` is matched
    /// positionally, with the same count. A target matches by its raw spelling
    /// ([quoted](quote_word)) and by every root-relative form it has from each directory
    /// the command may run in, lexically normalized and with symlinks followed
    /// ([`TargetForms`]); one with no root-relative form never matches. So
    /// `bash(cargo test > logs/*)` allows `> logs/x`, not `> logs/x > src/main.rs`,
    /// `> logs/../src/lib.rs` nor `> logs/link` when the link leaves `logs/`.
    fn targets_match(&self, red: &Redirected, c: &SimpleCommand, targets: &[&str]) -> bool {
        let patterns: Vec<&GlobMatcher> = match red.targets.as_slice() {
            [one] => vec![one; targets.len()],
            many => many.iter().collect(),
        };
        patterns.len() == targets.len()
            && targets.iter().zip(patterns).all(|(t, g)| {
                g.is_match(quote_word(t))
                    && cwds_of(c).iter().all(|cwd| {
                        let forms = self.target_forms(cwd, t);
                        forms.inside && forms.iter().all(|f| g.is_match(quote_word(f)))
                    })
            })
    }
}

/// A command that leaves its process group, so the session's cleanup (killing the groups
/// its commands started) cannot reach what it leaves running: `setsid`, daemonizers, and
/// job control (`set -m`, `set -o monitor`, a shell started with `-m`), which puts every
/// background job in a group of its own.
fn escape_reason(argv: &[String]) -> Option<String> {
    const LEAVERS: &[&str] = &["setsid", "daemonize", "start-stop-daemon"];
    const SHELLS: &[&str] = &["sh", "bash", "zsh", "dash", "ksh"];
    let base = |w: &str| w.rsplit('/').next().unwrap_or(w).to_string();
    let reason = |what: &str| Some(format!("{what} escapes session cleanup"));
    if let Some(w) = argv.iter().find(|w| LEAVERS.contains(&base(w).as_str())) {
        return reason(&format!("`{}`", base(w)));
    }
    let first = argv.first().map(|w| base(w))?;
    let job_control = first == "set" || SHELLS.contains(&first.as_str());
    if !job_control {
        return None;
    }
    let mut words = argv.iter().skip(1);
    while let Some(w) = words.next() {
        if w == "-o" && words.next().is_some_and(|o| o == "monitor") {
            return reason("job control (`-o monitor`)");
        }
        // Options end at the first operand (`sh script.sh -m` passes `-m` on).
        if !w.starts_with('-') || w == "-" || w == "--" {
            break;
        }
        if !w.starts_with("--") && w[1..].contains('m') {
            return reason("job control (`-m`)");
        }
    }
    None
}

/// The command as rules see it: leading assignments (as written), then argv, each word
/// that could read as shell syntax [quoted](quote_word). So a quoted `'>'` argument stays
/// `'>'` and never looks like a redirect.
fn command_line(c: &SimpleCommand) -> String {
    c.assignments
        .iter()
        .cloned()
        .chain(std::iter::once(quoted_words(&c.argv)))
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// `argv` as one line, each word [quoted](quote_word).
fn quoted_words(argv: &[String]) -> String {
    argv.iter()
        .map(|w| quote_word(w))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The argv forms a deny rule is tried against: as written; with `builtin`, `command`
/// and `time` prefixes removed ([`unwrap_prefixes`]); and, when the command starts with a
/// wrapper that runs the rest of its argv (`sudo`, `env`, `nohup`, `nice`, `timeout`, …),
/// every suffix starting at a non-option word, so `deny bash(git push*)` refuses
/// `timeout 5 git push` and `env FOO=1 git push`. Deliberately broad (a deny rule may
/// refuse an odd command, never miss a wrapped one); allow rules never use these forms.
fn deny_argvs(argv: &[String]) -> Vec<&[String]> {
    let mut out = vec![argv, unwrap_prefixes(argv)];
    if argv.first().is_some_and(|w| WRAPPERS.contains(&base(w))) {
        out.extend(
            (1..argv.len())
                .filter(|&i| !argv[i].starts_with('-'))
                .map(|i| &argv[i..]),
        );
    }
    out
}

/// `w` as a shell word: as it is, or in single quotes (a `'` written `'\''`) when it is
/// empty or holds whitespace, `>`, `<`, `|`, `&`, `;` or a quote.
fn quote_word(w: &str) -> String {
    let special =
        |c: char| c.is_whitespace() || matches!(c, '>' | '<' | '|' | '&' | ';' | '\'' | '"');
    if !w.is_empty() && !w.chars().any(special) {
        return w.to_string();
    }
    format!("'{}'", w.replace('\'', "'\\''"))
}

/// A command's write-redirect targets, in order: every write operator (`>`, `>>`, `&>`,
/// `2>`, ...), except to `/dev/null`. Reads and fd duplications (`2>&1`) are not targets.
fn write_targets(c: &SimpleCommand) -> Vec<&str> {
    c.redirects
        .iter()
        .filter(|r| r.write && r.target != "/dev/null")
        .map(|r| r.target.as_str())
        .collect()
}

/// The command as allow rules see it: [`command_line`], then ` > <target>` for each write
/// redirect (`>`, `>>`, `&>`, ...) other than `/dev/null`. So `bash(cargo test)` does not
/// allow `cargo test > src/lib.rs`; a pattern must reach the redirect (`bash(cargo test*)`
/// does) or name it. Reads and fd duplications (`2>&1`) are not included.
fn allow_subject(c: &SimpleCommand) -> String {
    let mut s = command_line(c);
    for t in write_targets(c) {
        s.push_str(" > ");
        s.push_str(&quote_word(t));
    }
    s
}

/// `s` as a glob that matches only itself.
fn escape_glob(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '*' | '?' | '[' | ']' | '{' | '}' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
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
