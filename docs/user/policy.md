# Permission policy reference

Every tool call the model makes is decided before it runs: **allow**, **ask** (you approve
or deny it) or **deny**. Headless runs (`tautly -p`, `--rpc` without an interactive
client, `tautly eval`) cannot ask, so there *ask* means *deny*.

Tautly has no OS-level sandbox yet: the policy, path confinement to the work tree and the
protected paths below are what stand between the model and your machine. Keep `yolo` for
disposable environments (containers, VMs, throwaway clones).

## Modes

| Mode | Runs without asking | Asks |
| --- | --- | --- |
| `ask` (default) | reads and searches | every file write, command, network fetch and MCP call |
| `auto-edit` | reads, and writes to unprotected files inside the work tree | commands, network fetches, MCP calls |
| `yolo` | everything no deny rule, protected path or unparseable command stops | protected paths, unparseable commands |

Set it with `[permissions] mode = "..."` or for one run with `--approve auto-edit|yolo`.

## The order of the decision

1. **Deny rules.** A matching deny rule refuses the call, in every mode. An `edit` or
   `write` of a path outside the work tree (as written, or through a symlink) is refused
   too: the sandbox never writes there, so there is nothing to approve.
2. **Always-ask checks.** Even in `yolo`, the call asks when it:
   - writes a protected path, or (a shell redirect) a path outside the work tree;
   - is a command that cannot be parsed (an `xargs` command line, heredoc tricks, words
     the parser cannot place);
   - is a command whose arguments or redirects name a protected path, that sets a
     dangerous environment variable for itself, that runs outside the work tree after a
     `cd`, that changes git configuration or chooses whose configuration and programs
     git uses (`git config`, `git -c`, `--config-env`, `--exec-path`, `--git-dir`,
     `--work-tree`), or that escapes the
     session's process cleanup (`setsid`, daemonizers, shell job control).
3. **Allow rules** (from configuration and "allow for this session" answers).
4. **The mode's default.**

Reads (`read`, `grep`, `glob`, `find`, `outline`, `symbol`, `refs`, `recall`) and the
context tools (`pin`, `state`, `remember`) never ask; only a bare deny rule such as
`deny = ["grep"]` stops them. Reads are confined to the work tree by the
sandbox. A memory entry `remember` proposes is written only after you approve it (or with
`approve_memory`).

## Protected paths

Writing any of these always asks, whatever the mode or rules (case-insensitive, at any
depth): directories `.git/`, `.tautly/`, `.vscode/`, `.idea/`, `.github/workflows/`, the
files `config` and `config.toml` inside a `.cargo/` directory, and files `.envrc`,
`.gitconfig`, `Makefile`, `GNUmakefile`, `package.json`, `Cargo.toml`, `build.rs`,
`rust-toolchain`, `rust-toolchain.toml`, `.npmrc`, `.yarnrc`, `.yarnrc.yml`,
`.pnpmfile.cjs`, `bunfig.toml`, `.bashrc`, `.zshrc`, `.profile`, `.bash_profile`, `.zprofile`, `.zshenv`,
`.zlogin`, `.zlogout`, `.bash_login`, `.bash_logout`, `config.fish`. These are files a
trusted tool later executes, or configuration that names a program it runs (a cargo
`runner` or `rustc-wrapper`, npm's `script-shell`, yarn's `yarnPath`, bun's `preload`, a
rustup toolchain `path`), so an allowed `cargo test` or `npm test` cannot be turned into
something else by a file the agent wrote. A path is judged both as written and after following
symlinks, so `NOTES.md -> .git/config` counts as `.git/config`.

## Rule syntax

A rule is `tool(glob)` or a bare `tool`. A bare tool matches every call of that tool.

| Tool | The glob matches | Example |
| --- | --- | --- |
| `bash` | each parsed command (see below) | `bash(cargo test*)` |
| `edit`, `write` | each file written, root-relative (normalized, and after following links) | `edit(src/**)`, `write(docs/*.md)` |
| `web_fetch` | the host: lowercase, no port | `web_fetch(docs.rs)` |
| MCP tools | the tool name itself, no parentheses; may be a glob | `mcp__github__create_issue`, `mcp__github__*` |
| read-only and context tools | (only bare rules; `read(src/*)` or `find(src/*)` is an error) | `grep` |

Globs use the `globset` syntax: `*`, `?`, `[...]`, `{a,b}`. In rules `*` also matches
`/`, so `edit(src/*)` covers files in subdirectories of `src/` too.
A malformed rule in your file is an error; in a project file it fails the load (a dropped
deny rule would loosen the policy).

Deny always beats allow. A call with several subjects (a pipeline, a patch touching
several files) is allowed only when **every** subject matches an allow rule, and denied
when **any** matches a deny rule.

### Commands

The shell input is parsed (pipes, `&&`, `||`, `;`, subshells, command substitution) into
simple commands, and every one of them must pass. A rule sees a command as its leading
`NAME=value` assignments, then its words, with any word holding whitespace, quotes or
shell operators shell-quoted. No command is "safe" by name.

- `bash(cargo test*)` allows `cargo test`, `cargo test -p x` and `cargo test > out.txt`.
- `bash(cargo test)` allows exactly `cargo test`, not `cargo test > src/lib.rs`.

**Write redirects** are matched structurally. A rule whose glob contains ` > ` is split
there: the left half must match the command without its redirects, the command must write
at least one file, and the right half must match every write target (root-relative,
normalized and with links followed; a target outside the work tree never matches).
`bash(cargo test > logs/*)` allows `cargo test > logs/x`, but not `> logs/../src/lib.rs`
or `> logs/x > src/main.rs`. Every write operator (`>`, `>>`, `2>`, `&>`) is written ` > `
in a rule.

**Deny rules are broad on purpose.** A deny rule also matches the command with wrappers
removed (`sudo`, `env`, `nohup`, `nice`, `timeout`, `exec`, `builtin`, `command`, `time`,
…), with the program reduced to its basename (`/usr/bin/git`), `git` without its global
options (`git -C . --no-pager push`), and an `rm` rule by flag set: `bash(rm -rf*)`
refuses `rm -fr`, `rm -r -f` and `rm --recursive --force`. It is still a pattern over
spellings: `find -delete`, a script or another program can do the same thing. Use broad
patterns for what must never run (`bash(rm -*r*)`, `bash(git*push*)`) and keep `yolo` for
disposable environments. Allow rules never use these extra forms (`bash(cargo test)` does
not allow `time cargo test`).

## Session rules ("allow for this session")

When the REPL or an editor asks about a call, you can allow it **for this session**. The
offered rule is the narrowest that names the call: `edit(<path>)`, `write(<path>)`,
`bash(<the exact command>)`, `web_fetch(<host>)` or the MCP tool's name, with glob
characters escaped. In the REPL you can type `a <rule>` to give your own pattern instead;
it is accepted only if it matches the pending call (otherwise the call is approved once,
with a warning).

Session rules live in memory until the process exits: they are never written to
configuration or to the session log, and they are gone after a resume. Deny rules,
protected paths and unparseable commands still win over them.

## Headless approvals

| Flag | Effect |
| --- | --- |
| `--approve auto-edit` | the `auto-edit` mode for this run |
| `--approve yolo` | the `yolo` mode for this run |
| `--approve-memory` | save memory proposals without asking |
| `--approve-mcp` | start repository-declared MCP servers you have not approved, for this run only |

Nothing logged in a session ever counts as approval: a fork, a resume or an eval run
decides every call afresh under the current policy.

## Examples

```toml
[permissions]
mode = "auto-edit"
allow = [
  "bash(cargo test*)",
  "bash(cargo clippy*)",
  "bash(git status)",
  "bash(git diff*)",
  "web_fetch(docs.rs)",
  "mcp__github__get_*",
]
deny = [
  "bash(git push*)",
  "bash(rm -*r*)",
  "bash(* > .env)",
  "web_fetch",            # no web_fetch (commands and MCP servers can still reach the network)
]
```

A project's `.tautly/config.toml` may add `deny` rules and make `mode` stricter; its
`allow` rules are ignored.
