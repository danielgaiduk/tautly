//! Shell state carried from one command to the next within a single input:
//! the working directory (`cd`/`pushd`) and shell options (`set`/`shopt`).
//!
//! Directories are tracked lexically and conservatively. Every directory a
//! command may run in is kept: a `cd` can fail, so the directories it leaves
//! behind stay possible after the statement (or on the `||` side). Changes we
//! cannot follow (dynamic targets, `popd`, `cd` in loops, option changes such as
//! `lastpipe`) make every later command unparseable.

use std::path::PathBuf;

use crate::runners::base;
use crate::shell::{SimpleCommand, Unparseable};

/// Most working directories tracked at once before giving up.
const MAX_CWDS: usize = 8;

/// Walk state: every directory the next command may run in (lexical, relative
/// to the workspace root or absolute).
pub(crate) struct Cwd {
    /// Directories possible when the commands so far succeeded.
    pub(crate) cwds: Vec<PathBuf>,
    /// Directories left behind by a `cd` that may have failed; possible again
    /// after the current statement, or for the right side of `||`.
    failed: Vec<PathBuf>,
    /// Set once the state changed in a way we cannot follow; any later command
    /// is then unparseable.
    lost: Option<String>,
    /// Inside a loop or function body.
    pub(crate) loops: usize,
    /// No command other than an allowed `set` has run yet.
    prefix: bool,
}

/// Saved state for scopes that do not leak changes (subshells, substitutions,
/// pipeline elements).
pub(crate) struct Saved {
    cwds: Vec<PathBuf>,
    failed: Vec<PathBuf>,
    lost: Option<String>,
}

impl Default for Cwd {
    fn default() -> Self {
        Cwd {
            cwds: vec![PathBuf::new()],
            failed: Vec::new(),
            lost: None,
            loops: 0,
            prefix: true,
        }
    }
}

fn union(into: &mut Vec<PathBuf>, more: impl IntoIterator<Item = PathBuf>) {
    for c in more {
        if !into.contains(&c) {
            into.push(c);
        }
    }
}

impl Cwd {
    /// Record `cmd` with the directories it may run in.
    pub(crate) fn push(
        &mut self,
        out: &mut Vec<SimpleCommand>,
        mut cmd: SimpleCommand,
    ) -> Result<(), Unparseable> {
        if let Some(why) = &self.lost {
            return Err(Unparseable(format!(
                "{why}, so `{}` cannot be analyzed",
                cmd.argv.join(" ")
            )));
        }
        if !is_allowed_leading_set(unwrap_prefixes(&cmd.argv)) {
            self.prefix = false;
        }
        cmd.cwds = self.cwds.clone();
        out.push(cmd);
        Ok(())
    }

    pub(crate) fn save(&self) -> Saved {
        Saved {
            cwds: self.cwds.clone(),
            failed: self.failed.clone(),
            lost: self.lost.clone(),
        }
    }

    pub(crate) fn restore(&mut self, s: Saved) {
        self.cwds = s.cwds;
        self.failed = s.failed;
        self.lost = s.lost;
    }

    /// A function named `name` was defined.
    pub(crate) fn define_function(&mut self, name: &str) {
        if is_sensitive_command(name) {
            self.lose(format!("a function redefines `{name}`"));
        }
    }

    fn lose(&mut self, why: String) {
        self.lost.get_or_insert(why);
    }

    fn check_size(&mut self) {
        if self.cwds.len() > MAX_CWDS {
            self.lose("too many possible working directories".into());
        }
    }

    /// Union with directories that remain possible (`before` first).
    pub(crate) fn merge(&mut self, before: Vec<PathBuf>) {
        let mut all = before;
        union(&mut all, self.cwds.drain(..));
        self.cwds = all;
        self.check_size();
    }

    /// A statement ended: whatever failed inside it, the next statement runs.
    pub(crate) fn end_statement(&mut self, before: Vec<PathBuf>) {
        let failed = std::mem::take(&mut self.failed);
        self.merge(before);
        union(&mut self.cwds, failed);
        self.check_size();
    }

    /// Continue on the path where the previous command failed (`a || b`,
    /// `! a`): any directory reached so far is possible.
    pub(crate) fn assume_failure(&mut self) {
        let failed = self.failed.clone();
        union(&mut self.cwds, failed);
        self.check_size();
    }

    pub(crate) fn in_loop<T>(&mut self, f: impl FnOnce(&mut Self) -> T) -> T {
        self.loops += 1;
        let r = f(self);
        self.loops -= 1;
        r
    }

    /// Apply the state change of a command just recorded (prefixes such as
    /// `builtin`/`command`/`time` already removed).
    pub(crate) fn after_command(&mut self, argv: &[String]) {
        let Some(name) = argv.first().map(|a| base(a)) else {
            return;
        };
        match name {
            "cd" | "pushd" | "popd" | "dirs" => self.change_dir(argv),
            "set" if set_changes_options(&argv[1..]) => {
                if !(self.prefix && is_allowed_leading_set(argv)) {
                    self.lose(format!("`{}` changes shell options", argv.join(" ")));
                }
            }
            "shopt" if shopt_changes_options(&argv[1..]) => {
                self.lose(format!("`{}` changes shell options", argv.join(" ")));
            }
            "alias" | "enable" => {
                let redefined = argv[1..]
                    .iter()
                    .filter(|a| !a.starts_with('-'))
                    .map(|a| a.split_once('=').map_or(a.as_str(), |(n, _)| n))
                    .find(|n| is_sensitive_command(n));
                if let Some(n) = redefined {
                    self.lose(format!("`{}` redefines `{n}`", argv.join(" ")));
                }
            }
            _ => {}
        }
    }

    fn change_dir(&mut self, argv: &[String]) {
        let target = match base(&argv[0]) {
            "cd" | "pushd" if self.loops == 0 => literal_dir(&argv[1..]),
            _ => None,
        };
        let Some(t) = target else {
            self.lose(format!(
                "`{}` changes directory in a way we cannot follow",
                argv.join(" ")
            ));
            return;
        };
        // If the cd fails, the shell stays where it was.
        let old = std::mem::take(&mut self.cwds);
        for c in &old {
            union(&mut self.cwds, [c.join(&t)]); // an absolute target replaces `c`
        }
        union(&mut self.failed, old);
        self.check_size();
    }
}

/// The command that actually runs behind `builtin` (`--`), `command` (`-p`, `--`) and
/// `time` (`-p`) prefixes. `command -v`/`-V` only look a name up, so they are
/// returned as is.
pub(crate) fn unwrap_prefixes(argv: &[String]) -> &[String] {
    let mut a = argv;
    loop {
        let Some(first) = a.first() else {
            return a;
        };
        let skip_opts = |a: &[String], allowed: &str| -> Option<usize> {
            let mut i = 1;
            while let Some(w) = a.get(i) {
                if w == "--" {
                    return Some(i + 1);
                }
                match w.strip_prefix('-') {
                    Some(flags) if !flags.is_empty() => {
                        if !flags.chars().all(|c| allowed.contains(c)) {
                            return None;
                        }
                        i += 1;
                    }
                    _ => break,
                }
            }
            Some(i)
        };
        let next = match base(first) {
            "builtin" => skip_opts(a, ""),
            "command" => skip_opts(a, "p"),
            "time" => skip_opts(a, "p"),
            _ => return a,
        };
        match next {
            Some(i) if i <= a.len() => a = &a[i..],
            _ => return a,
        }
    }
}

/// The target of `cd`/`pushd` when it is one literal path: no glob, expansion,
/// `~`, `-` (previous directory), stack index or missing argument (home).
fn literal_dir(args: &[String]) -> Option<PathBuf> {
    let mut operands = Vec::new();
    let mut options_done = false;
    for a in args {
        if !options_done && a == "--" {
            options_done = true;
        } else if !options_done && a.len() > 1 && a.starts_with('-') {
            if !matches!(a.as_str(), "-P" | "-L") {
                return None;
            }
        } else {
            operands.push(a.as_str());
        }
    }
    let [t] = operands[..] else {
        return None;
    };
    let unsafe_target = t.is_empty()
        || t == "-"
        || t.starts_with(['~', '+'])
        || t.contains(['*', '?', '[', '{', '$', '`']);
    (!unsafe_target).then(|| PathBuf::from(t))
}

/// Whether `set` arguments change options (rather than only setting positional
/// parameters or printing).
fn set_changes_options(args: &[String]) -> bool {
    for a in args {
        if a == "--" {
            return false;
        }
        if a.starts_with('-') || a.starts_with('+') {
            return true;
        }
    }
    false
}

/// `set` with only the flags agent commands commonly start with:
/// `-e`, `-u`, `-x` and `-o pipefail|errexit|nounset|xtrace`.
fn is_allowed_leading_set(argv: &[String]) -> bool {
    if argv.first().map(|a| base(a)) != Some("set") {
        return false;
    }
    let mut it = argv[1..].iter();
    while let Some(a) = it.next() {
        if a == "--" {
            return true;
        }
        let Some(flags) = a.strip_prefix('-') else {
            return !a.starts_with('+');
        };
        for c in flags.chars() {
            match c {
                'e' | 'u' | 'x' => {}
                'o' => {
                    let ok = it.next().is_some_and(|o| {
                        matches!(o.as_str(), "pipefail" | "errexit" | "nounset" | "xtrace")
                    });
                    if !ok {
                        return false;
                    }
                }
                _ => return false,
            }
        }
    }
    true
}

/// `shopt -s`/`-u` set or unset options (e.g. `lastpipe`, which runs the last
/// pipeline element, and its `cd`, in the current shell).
fn shopt_changes_options(args: &[String]) -> bool {
    args.iter()
        .any(|a| a.starts_with('-') && !a.starts_with("--") && a.contains(['s', 'u']))
}

/// Commands whose meaning the analysis relies on. Redefining one (function,
/// `alias`, `enable -n`) makes every later command unanalyzable.
fn is_sensitive_command(name: &str) -> bool {
    const SENSITIVE: &[&str] = &[
        "cd", "pushd", "popd", "dirs", "builtin", "command", "set", "shopt", "eval", "source", ".",
        "exec", "test", "[", "printf", "read", "export", "declare", "typeset", "local", "readonly",
        "unset", "time", "alias", "unalias", "enable", "trap",
    ];
    SENSITIVE.contains(&name)
}
