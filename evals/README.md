# Eval tasks

`tautly eval` runs a set of small repository tasks headless and reports pass rate, cost,
tokens, tokens per solved task, cache hit ratio and edit failures as JSON.

```sh
tautly eval --tasks evals/starter --model anthropic/claude-sonnet-5-5 --jobs 4 --out results.json
tautly eval report results.json --markdown
```

> **Task directories are trusted code.** `setup.sh` and `verify.sh` run as you, unconfined
> (Tautly has no OS sandbox before 1.5), and every starter task sets `approve = "yolo"`,
> so the shell commands the model chooses run unconfined too, apart from deny rules and
> protected paths. Run `benchmarks/tokens/run.sh` and any third-party task set in a VM or
> container, not on a machine whose files you care about.

`starter/` holds 50 tasks (15 bug fixes, 10 small features, 10 refactors or renames,
5 multi-file edits, 5 test-writing tasks and 5 configuration or build fixes) in Python,
TypeScript, Rust and Go. Running them needs `git`, `bash`, `python3`, Node.js 22.18 or
newer (it runs `.ts` files directly), `npm`, `cargo` and `go` on `PATH`. No task
downloads anything.

## Task format

Each task is a directory whose name is the task's id, holding three files.

`task.toml`:

```toml
id = "py-bug-off-by-one"   # must match the directory's name
prompt = "..."             # the one message the agent gets
timeout_s = 600            # default 600: the run is stopped after this
budget_usd = 1.0           # default 1.0: the run's spending limit (`--budget-usd`)
tags = ["python", "bug"]   # default []: for grouping results
approve = "yolo"           # optional: "auto-edit" or "yolo"
```

`approve` is the only source of approvals. Without it, anything the policy would ask
about is denied (the run is headless). `auto-edit` lets edits in the workspace through,
and `yolo` lets through everything that no deny rule or protected path stops. The model
never chooses this, and nothing logged in a session counts as approval.

`setup.sh` runs in a new, empty temporary directory and must make the task's repository
there: write the files, then `git init` and commit them. A setup that exits non-zero
fails the task with reason `setup_failed`.

`verify.sh` runs at the root of the repository after the agent's turn ends. Exit 0
passes. `EVAL_BASE` holds the commit setup made (the runner reads it before the agent
starts, so rewriting history cannot move it). The starter tasks check that their
protected files (tests, or the code under test) still match it, then run the tests. The test-writing tasks also run the new tests against broken
implementations, which must fail.

Both scripts run through the bash tool with a time limit of `timeout_s` (at most 600 s),
without provider keys in their environment, and offline: `GOPROXY=off`,
`GOTOOLCHAIN=local`, `GOFLAGS=-mod=readonly`, `CARGO_NET_OFFLINE=true`,
`npm_config_offline=true` and `npm_config_update_notifier=false`.

## Runs

Each run gets its own private (0700) temporary directory, never the current work tree.
The directory is removed afterwards, whatever happened, and so are the code index, lock
and memory episodes the run left in the data home. Its session (marked `origin: eval`,
tag `eval`) is removed too unless `--keep-logs` is given; each result names it as
`session_id`. The cost ledger keeps the runs' rows: they are real spending. `tautly -p <prompt> --output json` works there
under the normal policy and sandbox. A run passes when its turn completes, it spent no
more than `budget_usd` (the session-end episode included), and `verify.sh` exits 0.
Otherwise `reason` says why it failed: `setup_failed`, `budget`, `timeout`, `max_turns`,
`cancelled`, `error` or `verify_failed`.

`--trials N` runs every task N times. The results hold one entry per task and trial, and
`summary.mean` and `summary.stdev` give each figure across trials.

## Replay gate

`replay/<id>.json` is a scripted provider session for the starter task `<id>`. Ten tasks
have one, and their responses read, re-read, outline, look up symbols, edit, run the
tests and recall. `tautly eval --replay evals/replay` replays them with the mock provider
and compares tokens per solved task with `baseline.json`. The scripts carry no usage, so
the figure is the engine's own estimate of each request it sends: system prompt, tools,
context and tool output all count. It varies by well under 1% between runs (the code
index may or may not be ready, and temporary paths differ); every replayed task must
pass. It needs a
build with the `mock` feature:

```sh
cargo run -p tautly --features mock -- eval --replay evals/replay
cargo run -p tautly --features mock -- eval --replay evals/replay --update-baseline
```

The gate fails when tokens per solved task exceed the baseline by more than 10%. After an
intended change, update the baseline and commit it.
