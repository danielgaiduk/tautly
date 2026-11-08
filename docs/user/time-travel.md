# Time travel: undo, rewind, fork, switch, replay, compare, bisect

Every session is an append-only log, and in a git work tree Tautly snapshots the files
before every command and at every turn end. Together they let you go back to any step,
branch from it, and look at what happened, without losing anything: no operation here
truncates a log, and none overwrites a file you changed without asking first.

## Steps and turns

- A **user turn** is one message you send and everything the agent does for it. Turns
  are numbered 1, 2, 3, … per session.
- A **step** is one entry in the conversation: your message, or one model response
  with its tool calls. Steps are numbered once per session, so a turn's steps are not
  consecutive with the next turn's (your message takes a number too: 1, 2, 3, 5, …).
- "At step N" means steps 1..N are kept; `0` means before the first step.
- `tautly sessions show <id>` lists a session's turns with their step numbers; the REPL's
  `/tree` shows each branch's last step.

## Checkpoints

In a git work tree with `[checkpoints] enabled` (the default), Tautly records a snapshot:
at session start, before every `bash` call and every stdio MCP call, at every turn end,
and before a rewind or switch. Snapshots are commits under `refs/tautly/checkpoints/` in
your own repository, made with git plumbing and a private index: your index, `HEAD`,
branches, stash and ignored files are never touched, and no hook, filter or fsmonitor the
repository configures runs.

- They show up in `git log --all`. Hide them with
  `git log --exclude='refs/tautly/*' --all` (`--exclude` must come first).
- A plain `git push` does not send them; a mirror push or a `refs/*` push refspec would,
  and Tautly warns at session start when a remote is set up that way.
- A snapshot is skipped with a warning when git is missing, it would take longer than
  `checkpoints.timeout_ms`, or there are more untracked files than the limits allow.
  Refs older than `checkpoints.keep_days` are deleted at engine start.
- Outside git, or with `--no-checkpoints`, Tautly still keeps every file its own tools
  wrote (the pre-images are in the session log), so undo and rewind restore the agent's
  edits but not what commands changed; they say so in a note.

## Undo a turn

Revert what one user turn changed: its tool edits and, where checkpoints were taken, its
commands' changes too.

| Where | How |
| --- | --- |
| REPL | `/undo [N]`: shows a dry run, asks, then undoes turn N (default: the last) |
| CLI | `tautly undo [--turn N] [--dry-run] [--session ID]` |
| RPC | `undo {turn?, dry_run?}` → `undone` event |
| Editor (ACP) | the `/undo [N]` prompt; it asks with a permission request first |

Undo refuses (and changes nothing) when a file it would restore was changed since by you
or by a later turn that is not undone. Before writing it takes a backup snapshot
(`refs/tautly/checkpoints/<session>/undo-<n>`), so an undo can itself be reverted with
git. Ignored files (`node_modules/`, `target/`) and submodule contents are never touched.
Undo is refused while another live session uses the same work tree.

## Rewind to a step

Go back to step N with both the files and the conversation, as a **new branch**: the old
session stays as it was, and you continue on the branch.

| Where | How |
| --- | --- |
| REPL | `/rewind N`: shows the dry run, asks, then rewinds |
| RPC | `rewind {at, label?, dry_run?, overwrite?}` → `rewound`, then `session_changed` |

Only paths whose content at step N differs from what the agent left are written. A path
*you* changed since (a human edit) is a **conflict**: unless you list it in `overwrite`
(the REPL asks you separately), nothing is written and no branch is made. A path below a
symlink or a file, or where a directory is in the way, is **blocked** and never written.
A backup snapshot is taken before any write; if a rewind is cut off, the next resume warns
and `tautly sessions restore-backup <session>` puts back what it kept.

## Branches: fork, label, tree, switch

| What | REPL | CLI | RPC |
| --- | --- | --- | --- |
| Branch the conversation at the tip (files untouched) | `/fork` | | `fork {at?, label?}` |
| Rename the current branch | `/label <name>` | | `label {label}` |
| Show the project's sessions as a tree | `/tree` (`*` marks the current branch) | `tautly sessions tree` | `session/tree {root}` |
| Continue another branch, its files restored | `/switch <label\|id\|n>` (`n`: its line in `/tree`) | | `switch {session_id, dry_run?, overwrite?}` |

A switch restores the target branch's files as its agent left them, under the same
conflict, blocking and backup rules as a rewind. Switching back and forth restores each
tip exactly.

## Fork into a separate worktree

Try another approach from step N without touching your working tree:

```sh
tautly fork <session> --at N [--model ID] [--label L] [--dir PATH]
tautly fork <session> --at 7 --model openai/gpt-5 --prompt "try a different fix" --approve auto-edit
```

It creates a new git worktree (by default `<data home>/worktrees/<project-id>/<session-id>`)
holding the files of step N, and a new session there holding the conversation up to step
N. Without `--prompt` it prints the worktree, the session id and how to resume it
(`cd <dir> && tautly --resume <id>`); with `--prompt` it runs one headless turn there, like
`tautly -p` (`--approve`, `--budget-usd` and `--output` apply). Outside git it is refused.
Over RPC: `session/new {root, fork: {session_id, at, worktree: true, model?, label?, dir?}}`.

## Replay a session

Play a session back from its log. No model is called, no tool runs, and no API key is
needed:

```sh
tautly replay <session> [--speed 2] [--from-step N] [--output text|json]
```

`--speed 0` is instant; gaps longer than 2 s are shortened. `--output json` prints each
event wrapped as `replayed {offset, t_ms, event}`, then `replay_finished`. In the REPL:
`/replay [id]` (Ctrl+C stops it). Over RPC: `replay {session_id?, speed?, from_step?}`.

## Compare sessions

```sh
tautly compare <a> <b> [<c> …] [--json]
```

Two to five sessions side by side: models, turns, steps, tokens, cost, wall time, tool
calls, blocked calls, edit failures, the last test evidence, files touched and the diff
stat. When the sessions share a fork point, everything is counted from it. Files and diff
stats count the agent's tool edits only (what commands changed is not in the logs).
Compare reads only the logs and the cost ledger.

## Bisect a regression

Find the step at which a check started failing:

```sh
tautly bisect <session> --test "cargo test -p mycrate" [--good N] [--bad M] [--timeout S] [--yes] [--json]
```

Each candidate step's files are restored into one temporary worktree under the data home
and the command runs there (exit 0 good, 125 skip, anything else bad; a timeout skips).
Only steps whose file state is exact are candidates. Logged runs of the same command
narrow the range first. `--good` defaults to 0 and `--bad` to the last step, or to what the
session's own runs of the command show; both ends are checked before the search (exit 2
if they do not hold). It prints the command and the maximum number of runs and asks first,
unless `--yes`; a deny rule matching the command refuses it. The result names the first
failing step, its turn, its tool calls and some of its assistant text. Your working tree is
never touched, and the temporary worktree is removed at the end.

## Deleting sessions

`tautly sessions delete <id>` removes a session's log and its checkpoint refs, and a
worktree fork's worktree (refused when that worktree has changes, unless `--force`).
Blobs shared with other sessions of the project are kept.
