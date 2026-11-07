//! Commands that run other code: shells and script interpreters, wrappers that
//! run the rest of their argv, and builtins that load or evaluate code.
//!
//! The rule is fail-closed by construction: an interpreter is only analyzable
//! when every option is one we know and its code comes from a plain script file
//! (or, for python, a `-m` module whose stdin is not fed by the command line).
//! Anything else, including options we have never heard of, is [`Unparseable`].

use crate::shell::Unparseable;

/// Commands that run the rest of their argv as another command. We look through
/// them conservatively: any later word may be the inner command.
pub(crate) const WRAPPERS: &[&str] = &[
    "sudo",
    "doas",
    "env",
    "command",
    "builtin",
    "exec",
    "nohup",
    "time",
    "xargs",
    "nice",
    "timeout",
    "setsid",
    "find",
    "stdbuf",
    "ionice",
    "flock",
    "taskset",
    "chrt",
    "numactl",
    "unbuffer",
    "caffeinate",
    "pkexec",
    "runuser",
    "strace",
    "ltrace",
    "busybox",
    "nsenter",
    "unshare",
    "firejail",
    "arch",
    "xcrun",
    "sandbox-exec",
    "systemd-run",
    "setpriv",
    "prlimit",
    "fakeroot",
];

/// Commands that run their arguments through a shell (`watch`, `su -c`, `script -c`)
/// or start a shell that reads unseen input (`su`, `script`, `chroot` without a command).
const SHELL_RUNNERS: &[&str] = &["watch", "su", "script", "chroot"];

/// Commands that run a string or file as shell code.
const EVALUATORS: &[&str] = &["eval", "source", ".", "trap", "coproc"];

const SHELLS: &[&str] = &[
    "bash", "sh", "zsh", "dash", "ksh", "ash", "mksh", "fish", "csh", "tcsh",
];

pub(crate) fn base(s: &str) -> &str {
    s.rsplit('/').next().unwrap_or(s)
}

/// An interpreter's complete option table. Options not listed are refused.
struct Interp {
    shell: bool,
    /// Short flags that take no value.
    flags: &'static str,
    /// Short flags whose value is the rest of the cluster or else the next word.
    value: &'static str,
    /// Short flags whose (possibly empty) value is the rest of the cluster only.
    attached: &'static str,
    /// Short flags whose value is inline code (or, for perl `-M`, can hide code).
    inline: &'static str,
    /// Short flags that make the interpreter read code from stdin.
    stdin: &'static str,
    /// Short flag whose value is a module to run (`python -m`).
    module: Option<char>,
    long_flags: &'static [&'static str],
    /// Long options that take a value (`--opt value` or `--opt=value`).
    long_value: &'static [&'static str],
    /// Long options whose value is inline code.
    long_inline: &'static [&'static str],
    /// Long options that make the interpreter read code from stdin.
    long_stdin: &'static [&'static str],
    /// Any `--opt=value` is self-contained and accepted (node has hundreds).
    any_long_with_value: bool,
    /// Words that only print information and exit.
    info: &'static [&'static str],
}

const SHELL: Interp = Interp {
    shell: true,
    flags: "abefhkmnptuvxBCEHPTilr",
    value: "oO",
    attached: "",
    inline: "c",
    stdin: "s",
    module: None,
    long_flags: &[
        "--norc",
        "--noprofile",
        "--login",
        "--posix",
        "--restricted",
        "--verbose",
        "--noediting",
        "--debugger",
    ],
    long_value: &["--rcfile", "--init-file"],
    long_inline: &["--command"],
    long_stdin: &[],
    any_long_with_value: false,
    info: &["--version", "--help"],
};

const PYTHON: Interp = Interp {
    shell: false,
    flags: "bBdEIOPqRsSuvx",
    value: "WX",
    attached: "",
    inline: "c",
    // `-i` drops into the REPL (reading stdin) after the script.
    stdin: "i",
    module: Some('m'),
    long_flags: &["--safe-path"],
    long_value: &["--check-hash-based-pycs"],
    long_inline: &[],
    long_stdin: &[],
    any_long_with_value: false,
    info: &["--version", "--help", "-V", "-h", "-VV"],
};

const NODE: Interp = Interp {
    shell: false,
    flags: "",
    value: "rC",
    attached: "",
    inline: "ep",
    stdin: "i",
    module: None,
    long_flags: &[
        "--inspect",
        "--inspect-brk",
        "--enable-source-maps",
        "--no-warnings",
        "--no-deprecation",
        "--trace-warnings",
        "--trace-deprecation",
        "--trace-uncaught",
        "--throw-deprecation",
        "--pending-deprecation",
        "--experimental-vm-modules",
        "--expose-gc",
        "--preserve-symlinks",
        "--abort-on-uncaught-exception",
        "--test",
        "--watch",
    ],
    long_value: &[
        "--require",
        "--import",
        "--loader",
        "--conditions",
        "--input-type",
        "--title",
    ],
    long_inline: &["--eval", "--print"],
    long_stdin: &["--interactive"],
    any_long_with_value: true,
    info: &["--version", "--help", "-v", "-h"],
};

const PERL: Interp = Interp {
    shell: false,
    flags: "acnpsStTuUwWX",
    value: "I",
    attached: "0CdDFilx",
    // `-M'Mod;code'` becomes `use Mod;code;`.
    inline: "eEmM",
    stdin: "",
    module: None,
    long_flags: &[],
    long_value: &[],
    long_inline: &[],
    long_stdin: &[],
    any_long_with_value: false,
    info: &["--version", "--help", "-v", "-V", "-h"],
};

const RUBY: Interp = Interp {
    shell: false,
    flags: "acdlnpsSvwy",
    value: "CEIr",
    attached: "0FiWxK",
    inline: "e",
    stdin: "",
    module: None,
    long_flags: &["--disable-gems", "--verbose", "--yydebug"],
    long_value: &["--encoding", "--external-encoding", "--internal-encoding"],
    long_inline: &[],
    long_stdin: &[],
    any_long_with_value: false,
    info: &["--version", "--help", "-h"],
};

fn interp(name: &str) -> Option<&'static Interp> {
    let versioned = |prefix: &str| {
        name.strip_prefix(prefix)
            .is_some_and(|v| v.chars().all(|c| c.is_ascii_digit() || c == '.'))
    };
    if SHELLS.contains(&name) {
        Some(&SHELL)
    } else if versioned("python") || versioned("pypy") {
        Some(&PYTHON)
    } else if matches!(name, "node" | "nodejs") {
        Some(&NODE)
    } else if versioned("perl") {
        Some(&PERL)
    } else if versioned("ruby") {
        Some(&RUBY)
    } else {
        None
    }
}

/// Where an analyzable interpreter invocation gets its code.
#[derive(Debug, PartialEq, Eq)]
enum Source {
    /// A script file named on the command line.
    Script,
    /// A module (`python -m mod`); it may read stdin as code (`-m code`).
    Module,
    /// Prints information and exits.
    Info,
}

/// A script operand we cannot treat as a file in the workspace: devices and
/// `/proc` (`/dev/stdin`, `/dev/fd/0`), process substitution, dynamic paths, or a
/// relative path that climbs out of wherever it starts.
fn opaque_script(op: &str) -> bool {
    if op.starts_with("/dev/") || op.starts_with("/proc/") || op == "/dev" || op == "/proc" {
        return true;
    }
    if op.starts_with("<(") || op.starts_with(">(") || op.contains(['$', '`']) {
        return true;
    }
    let mut depth: i64 = 0;
    for part in op.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                depth -= 1;
                if depth < 0 {
                    return true;
                }
            }
            _ => depth += 1,
        }
    }
    false
}

/// `None` if `a` is not an interpreter invocation; otherwise its code source, or
/// why it cannot be analyzed.
fn interpreter_source(a: &[String]) -> Option<Result<Source, &'static str>> {
    let spec = interp(base(&a[0]))?;
    Some(source_of(spec, &a[1..]))
}

fn source_of(spec: &Interp, args: &[String]) -> Result<Source, &'static str> {
    const INLINE: &str = "runs inline code";
    const STDIN: &str = "reads its script from stdin";
    const UNKNOWN: &str = "has an option we cannot analyze";
    const OPAQUE: &str = "runs a script from a device, process substitution or dynamic path";
    let script = |op: &str| {
        if opaque_script(op) {
            Err(OPAQUE)
        } else {
            Ok(Source::Script)
        }
    };
    let mut it = args.iter();
    while let Some(w) = it.next() {
        if spec.info.contains(&w.as_str()) {
            return Ok(Source::Info);
        }
        if w == "-" {
            return Err(STDIN);
        }
        if w == "--" {
            return match it.next() {
                Some(op) => script(op),
                None => Err(STDIN),
            };
        }
        if let Some(long) = w.strip_prefix("--") {
            let (key, has_value) = match long.split_once('=') {
                Some((k, _)) => (format!("--{k}"), true),
                None => (w.clone(), false),
            };
            let key = key.as_str();
            if spec.long_inline.contains(&key) {
                return Err(INLINE);
            }
            if spec.long_stdin.contains(&key) {
                return Err(STDIN);
            }
            if spec.long_value.contains(&key) {
                if !has_value {
                    it.next();
                }
            } else if !(spec.long_flags.contains(&key) && !has_value
                || has_value && spec.any_long_with_value)
            {
                return Err(UNKNOWN);
            }
            continue;
        }
        let cluster = w
            .strip_prefix('-')
            .or_else(|| w.strip_prefix('+').filter(|_| spec.shell));
        let Some(cluster) = cluster else {
            return script(w);
        };
        for (i, ch) in cluster.char_indices() {
            let rest = &cluster[i + ch.len_utf8()..];
            if spec.inline.contains(ch) {
                return Err(INLINE);
            }
            if spec.stdin.contains(ch) {
                return Err(STDIN);
            }
            if spec.module == Some(ch) {
                return Ok(Source::Module);
            }
            if spec.value.contains(ch) {
                if rest.is_empty() {
                    it.next();
                }
                break;
            }
            if spec.attached.contains(ch) {
                break;
            }
            if !spec.flags.contains(ch) {
                return Err(UNKNOWN);
            }
        }
    }
    Err(STDIN)
}

/// Why `a` (a command, or a wrapper's inner command) runs code we cannot see.
fn runs_unseen_code(a: &[String]) -> Option<&'static str> {
    let name = base(&a[0]);
    let has_flag = |flags: &str| {
        a[1..].iter().any(|w| {
            w.starts_with('-') && !w.starts_with("--") && w[1..].contains(|c| flags.contains(c))
        })
    };
    match name {
        _ if EVALUATORS.contains(&name) => Some("runs a string as code"),
        _ if SHELL_RUNNERS.contains(&name) => Some("runs its arguments through a shell"),
        // Its command line is completed from stdin, which no rule sees (CP5 review M2).
        "xargs" => Some("runs its input as part of a command"),
        "env" if has_flag("S") || a[1..].iter().any(|w| w.starts_with("--split-string")) => {
            Some("splits a string into a command")
        }
        "flock" if has_flag("c") || a[1..].iter().any(|w| w.starts_with("--command")) => {
            Some("runs its arguments through a shell")
        }
        "compgen" if has_flag("CF") => Some("runs a completion command or function"),
        "enable" if has_flag("f") => Some("loads a builtin from a shared object"),
        "bind" if has_flag("x") => Some("binds a key to a shell command"),
        _ => interpreter_source(a).and_then(Result::err),
    }
}

/// Reject commands that execute code we cannot see, directly or through a
/// wrapper (wrapper argv is kept as written; we only look through it).
pub(crate) fn reject_unanalyzable(argv: &[String]) -> Result<(), Unparseable> {
    if argv[0].contains('$') || argv[0].contains('`') {
        return Err(Unparseable(format!("dynamic command name `{}`", argv[0])));
    }
    let found = runs_unseen_code(argv).or_else(|| {
        let name = base(&argv[0]);
        // `command -v x` only looks `x` up.
        let lookup = name == "command"
            && argv[1..]
                .iter()
                .any(|w| w.starts_with('-') && !w.starts_with("--") && w.contains(['v', 'V']));
        if !WRAPPERS.contains(&name) || lookup {
            return None;
        }
        if argv[1..].iter().any(|w| w.contains(['$', '`'])) {
            return Some("may run a dynamic command");
        }
        // Conservative: through a wrapper, any later word may be the inner command.
        (1..argv.len()).find_map(|i| runs_unseen_code(&argv[i..]))
    });
    match found {
        Some(why) => Err(Unparseable(format!("`{}` {why}", argv.join(" ")))),
        None => Ok(()),
    }
}

/// Whether `argv` (directly or through a wrapper) runs a module rather than a
/// script file; such an interpreter must not have its stdin fed by the command
/// line (`python3 -m code <<< ...` runs the herestring).
pub(crate) fn runs_module(argv: &[String]) -> bool {
    let direct = |a: &[String]| matches!(interpreter_source(a), Some(Ok(Source::Module)));
    direct(argv)
        || (WRAPPERS.contains(&base(&argv[0])) && (1..argv.len()).any(|i| direct(&argv[i..])))
}
