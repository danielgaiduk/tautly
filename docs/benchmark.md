# Benchmarks

Tautly is measured in three ways. Only the deterministic replay gate runs in CI. Everything
that calls a real model is run by hand, or by the nightly workflow with repository secrets.

| What | Measures | Real calls | Where |
|---|---|---|---|
| `tautly eval --replay` | token use on scripted sessions (regression gate) | no | CI `perf` job |
| `tautly eval --tasks` | pass rate, tokens and cost on the 50 starter tasks | yes | `benchmarks/tokens/run.sh` |
| Terminal-Bench 2.0 | pass rate on an external benchmark, compared with other harnesses | yes | `benchmarks/terminal-bench/run.sh` |
| Live smoke | each adapter completes a real fix and a tool loop | yes | `.github/workflows/nightly.yml` |

## Methodology

- **Fix the model.** Every comparison holds the model id fixed, written as
  `<provider>/<model>`. A result without its model id is not a result.
- **Repeat.** Model output varies from run to run. Terminal-Bench runs 5 attempts per task
  (`TRIALS`) and `tautly eval` takes `--trials N`. Report the mean and the spread, not the
  best run.
- **Record the versions.** Note `tautly --version`, the harness version (`harbor==0.25.0`,
  `terminal-bench@2.0`), the date, and the provider's model snapshot if it publishes one.
- **Tokens come from the event stream.** `--output json` emits one `usage` event per
  provider request. Totals are sums of `tokens_in`, `tokens_out`, `cache_read` and
  `cache_write`. `tokens_in` excludes cache reads. Cost is priced from the built-in catalog,
  and a model without a price reports no cost (`?`), never zero.
- **Derived figures**, as `tautly eval` computes them:
  - tokens per solved task = sum(`tokens_in` + `tokens_out` + `side_tokens`) / solved;
  - cache hit ratio = sum(`cache_read`) / sum(`tokens_in` + `cache_read`).
- **Approvals.** Benchmarks run with `--approve yolo` inside a container or a throwaway
  directory. Compare harnesses only at equivalent autonomy (no approvals asked for).

## Reproduce

### `tautly eval` (Task 34)

```sh
# Deterministic replay gate, no network (what CI runs):
cargo build -p tautly --features mock
target/debug/tautly eval --replay evals/replay
# After an intended change in token use, rewrite the baseline:
target/debug/tautly eval --replay evals/replay --update-baseline

# The starter set on a real model, 3 trials:
tautly eval --tasks evals/starter --model anthropic/claude-sonnet-5-5 --trials 3 --jobs 4 \
  --out results.json
tautly eval report results.json other.json --markdown

# Fixed models and trials, raw JSON under benchmarks/tokens/results/<date>/:
benchmarks/tokens/run.sh
```

- **Replay token gate.** `--replay` runs the scripted sessions in `evals/replay` against
  `evals/starter` with a mock provider. It compares tokens per solved task with
  `evals/baseline.json` and exits 1 when the result is more than 10% over. It needs a
  binary built with `--features mock`.
- **`--trials N`** (default 1) keeps one result entry per task and trial. The summary adds
  `trials` and each metric's `mean` and `stdev` across trials.
- **`eval report FILE... [--markdown]`** prints one row per results file: model, pass rate
  (with ± stdev when trials > 1), tokens per solved task, cache hit ratio and cost.
- **Warning: a task directory is trusted code.** `setup.sh` and `verify.sh` run as you, and a
  task's `approve = "yolo"` (all 50 starter tasks set it) lets the model run any command that
  no deny rule stops. Run `benchmarks/tokens/run.sh` and any third-party task set in a VM or
  container, never on a machine with credentials you care about.

### Terminal-Bench 2.0

See [benchmarks/terminal-bench/README.md](../benchmarks/terminal-bench/README.md).

```sh
pip install 'harbor==0.25.0'
export TAUTLY_BINARY=/path/to/tautly-x86_64-unknown-linux-musl/tautly
export ANTHROPIC_API_KEY=...
TAUTLY_TB_MODEL=anthropic/claude-sonnet-5-5 TRIALS=5 benchmarks/terminal-bench/run.sh
```

Harbor writes a job directory per run under `benchmarks/terminal-bench/results/<date>/`,
with per-trial rewards and the agent logs. Each trial's `tautly.jsonl` is the full event
stream, and Harbor's `n_input_tokens`, `n_cache_tokens`, `n_output_tokens` and `cost_usd`
are filled from it.

### Live smoke

```sh
export ANTHROPIC_API_KEY=... OPENAI_API_KEY=... GEMINI_API_KEY=...   # any subset
cargo test -p tautly --features mock --test live_smoke -- --ignored --nocapture
```

- Each provider without a key prints `SKIPPED` and passes. Ollama runs only when
  localhost:11434 answers and the model is pulled.
- Override the models with `TAUTLY_LIVE_ANTHROPIC_MODEL`, `TAUTLY_LIVE_OPENAI_MODEL`,
  `TAUTLY_LIVE_GOOGLE_MODEL` and `TAUTLY_LIVE_OLLAMA_MODEL`. The defaults are
  `anthropic/claude-sonnet-5-5`, `openai/gpt-5`, `google/gemini-3-pro` and
  `compat:ollama/qwen2.5-coder:7b`.
- `*_fixes_failing_test` creates a git repo with a failing Python unit test. It runs
  `tautly -p "make the failing test pass" --approve auto-edit --output json --budget-usd 0.50`,
  then asserts exit 0, an unchanged test file and a passing test. It needs `python3` and `git`.
- `--features mock` makes the scratch keychain file effective, so the run never opens your OS
  keychain. Keys come only from the environment.

## Reasoning continuity

The working state is sent as a tail message after the history (`<working_state>`). The
question is whether that tail costs the provider its prompt cache or its carried-over
reasoning in a tool loop. `*_reasoning_continuity` runs a three-hop read chain (`hop1.txt`
names `hop2.txt`, and so on) and prints one row per provider request:

```
CONTINUITY [openai] openai/gpt-5
  step  tokens_in  cache_read  tokens_out  cache_hit
     1       4210           0         310       0.00
     2        180        4224         95       0.96
     ...
```

How to read it:

- **Pass:** exit 0, the codeword `PERIWINKLE-42` in the answer, and at least 3 steps. For
  OpenAI this also shows that the live API accepts the replayed encrypted reasoning items next
  to id-less items. If it does not, the request fails and so does the test.
- **Cache (asserted for Anthropic and OpenAI):** from step 2 on, `cache_read` should hold
  most of the prompt (cache_hit close to 1). If the last step reads nothing from the cache,
  the test fails with "continuity lost". Gemini's implicit cache is best-effort, so its
  numbers are printed but not asserted.
- **Reasoning re-done (read by eye):** `tokens_out` includes reasoning tokens. A hop only has
  to read one file, so it should stay small. If every hop spends about as many output tokens
  as step 1, the model is reasoning from scratch each step: its earlier reasoning was dropped.
- **Limits:**
  - The engine sends no explicit reasoning setting (`CanonicalRequest.reasoning` is `None`).
    Anthropic therefore runs without extended thinking, and its row measures only the cache.
  - OpenAI and Gemini reason by their defaults.
  - The CLI has no switch to drop the working-state tail, so there is no without-tail run to
    compare against. Use the step-1 row as the reference instead.

**If continuity is lost** (the cache assertion fails twice in a row, or hop rows re-reason),
switch that adapter to the documented `volatile_tail` placement:

- Put the working state inside the last tool result's content instead of a separate message:
  - Anthropic: in the `tool_result` content blocks;
  - OpenAI: in `function_call_output.output`;
  - Gemini: in `functionResponse.response`.
- The engine marks such requests with `CanonicalRequest.volatile_tail`.
- Put the last cache breakpoint on the block before the tail.

`crates/tautly-provider/tests/volatile_tail.rs` pins that the marker changes nothing on the
wire until an adapter uses it. Update that test with the adapter change.

## Comparing harnesses on the same model

1. Pick one model id and use it for every harness, for example
   `anthropic/claude-sonnet-5-5`. Use the same provider account and region, and runs within
   the same day or two, because providers change serving and caching.
2. Run the same dataset and harness version. Terminal-Bench: `terminal-bench@2.0` on
   `harbor==0.25.0`, with `-k 5` attempts each. For other agents, use Harbor's built-in agents
   (`-a claude-code`, `-a codex`, `-a terminus-2`, ...) with the same `-m` and `-k`.
3. Compare:
   - pass rate (mean ± spread over the attempts);
   - tokens per solved task;
   - cache hit ratio;
   - cost per solved task.
   Count tokens the same way for every harness: Harbor's `n_input_tokens` includes cache
   reads for every agent.
4. Report agent-side limits that differ (turn caps, timeouts, budgets). Tautly's per-task
   budget is unset in the Terminal-Bench run. Its `max_turns` and turn timeout are the
   defaults.
5. For `tautly eval`, run each configuration (model, edit format, Tautly version) to its own
   results file and put them side by side with
   `tautly eval report a.json b.json --markdown`.

## Known failures

- **Containers without `git`.** Tautly takes git checkpoints before commands. Where a task
  image has no git, Tautly warns and skips the checkpoint, and `/undo` reverts only the
  agent's own edits. This does not affect the score.
- **Unpriced models.** Ollama and models missing from the catalog report no `cost_usd`, and
  `--budget-usd` cannot stop them.
- **Exit 2 on budget or turn limit.** The adapter records the exit code in `tautly.exit` and
  the trial counts as failed. It is not a harness error.
- **Gemini cache figures** vary from run to run: the implicit cache is best-effort.
- **Harbor API drift.** The adapter targets Harbor 0.25.0. Newer Harbor releases have renamed
  agent hooks before.
- Add each new failure here with the date, model and Tautly version.
