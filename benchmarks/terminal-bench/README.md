# Terminal-Bench 2.0 adapter

Runs Tautly on [Terminal-Bench 2.0](https://www.tbench.ai) as a Harbor installed agent.

## Pinned versions

| Component | Version |
|---|---|
| Harness | `harbor==0.25.0` (PyPI) |
| Dataset | `terminal-bench@2.0` |
| Agent | the Tautly release binary named by `TAUTLY_BINARY` (record its `tautly --version`) |

Harbor's agent API changes between releases. The adapter is written against 0.25.0
(`BaseInstalledAgent.install/run/populate_context_post_run`, `environment_logs_dir`,
`logs_dir`, `exec_as_root`, `exec_as_agent`, `environment.upload_file`). If you change
the pin, check those names first.

## Files

- `tautly_agent.py`: the agent.
  - `install()` uploads the binary and copies it to `/usr/local/bin/tautly`. Nothing else
    runs at install time: no package manager and no network.
  - `run()` executes `tautly -p "<instruction>" --approve yolo --output json --model <m>`.
    It writes the event stream to `tautly.jsonl` in the trial's agent logs, stderr to
    `tautly.stderr` and the exit code to `tautly.exit`.
  - Provider keys are forwarded from the host environment, never on a command line.
- `tautly_usage.py`: sums the `usage` events into Harbor's `n_input_tokens` (including
  cache reads), `n_cache_tokens`, `n_output_tokens` and `cost_usd`. Self-check:
  `python3 -m doctest tautly_usage.py`.
- `run.sh`: runs `TRIALS` attempts per task (default 5) on one fixed model.
  - It checks first that the binary is executable and the model's key variable is set.
  - Extra arguments go to `harbor run`, for example `run.sh --n-tasks 1` for a smoke run.
  - Each job directory is named after the model and the start time, so runs never collide.
    `results/` is ignored by git.

## Run

```sh
python3 -m venv .venv && . .venv/bin/activate && pip install 'harbor==0.25.0'
# The x86_64 Linux release binary (task containers are Linux), e.g. from a GitHub release:
tar -xJf tautly-x86_64-unknown-linux-musl.tar.xz
export TAUTLY_BINARY="$PWD/tautly-x86_64-unknown-linux-musl/tautly"
export ANTHROPIC_API_KEY=...            # the key for the model you pick
TAUTLY_TB_MODEL=anthropic/claude-sonnet-5-5 TRIALS=5 benchmarks/terminal-bench/run.sh
```

Docker must be running: Harbor runs each task in a local container by default. Results go
to `benchmarks/terminal-bench/results/<date>/`. For methodology, comparing harnesses and
known failures, see [docs/benchmark.md](../../docs/benchmark.md).

`--approve yolo` lets the agent run any command in the task container. That is the
benchmark's design, and the container is the sandbox. Do not point this adapter at a
machine you care about.
