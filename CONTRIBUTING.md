# Contributing to Tautly

Thanks for helping. This page covers building, testing and the checks a change must pass.
The design and its reasoning are in
[`docs/superpowers/specs/2026-11-02-hx-core-engine-design.md`](docs/superpowers/specs/2026-11-02-hx-core-engine-design.md)
(section numbers such as "spec 7.11.8" in code comments refer to it).

## Requirements

- Rust **1.90** or newer (edition 2024), on macOS or Linux. The workspace builds on stable.
- `git` (checkpoints, worktree forks and many tests use it).
- Optional: `hyperfine` (start-up gate), `cargo-deny` 0.20.2, a nightly toolchain and
  `cargo-fuzz` 0.13.2 (fuzzing), `shellcheck`, and Python 3, Node.js 22.18+, Go and npm
  for the eval starter tasks.

## Build and run

```sh
cargo build                          # every crate; the binary is target/debug/tautly
cargo run -p tautly -- --help        # the CLI package is `tautly` (crates/tautly-cli)
cargo run -p tautly -- doctor --offline
```

The `mock` feature adds `--mock-script FILE`, a scripted provider used by the end-to-end
tests, the perf gates and the eval replay gate. It also lets tests swap the OS keychain
for a file (`TAUTLY_TEST_SECRET_STORE`) and the Ollama port (`TAUTLY_TEST_OLLAMA_PORT`), so
no test touches your real keychain or a local Ollama.

Set `TAUTLY_HOME` to a scratch directory while you experiment: it moves both the config
home and the data home (sessions, index, ledger) there.

## The checks CI runs

Run these before you send a change; they are what the `ci` workflow gates on:

```sh
cargo fmt --all --check
cargo test --workspace --features tautly/mock
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy -p tautly --all-targets --features mock -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
cargo deny check                     # advisories, licences, bans, sources (deny.toml)
```

Without `--features tautly/mock` the binary's end-to-end tests (`mock_cli`, `first_run`,
`auth`, `doctor`, `repl_pty`, `fork`, `bisect`, `eval`) are skipped.

Other CI jobs: `perf` (below), `eval-tasks` (every starter task must fail before it is
solved), `benchmark-kit` (shellcheck and a doctest on `benchmarks/terminal-bench/`),
`config-hooks` (checkpoint hardening against git 2.54 config hooks, with
`TAUTLY_REQUIRE_CONFIG_HOOKS=1`), `dist-plan` (the cargo-dist release plan,
`scripts/check-dist-plan.sh`) and `musl-static` (the Linux binary links statically). The
nightly workflow runs the live provider smoke tests and a 60 s fuzz run per target.

## Code layout

| Crate | What it holds |
| --- | --- |
| `tautly-core` | The engine: sessions, the turn loop, commands and events (`protocol.rs`), configuration, auth, budgets, the cost ledger, checkpoints, undo and time travel |
| `tautly-provider` | Provider-neutral requests, the Anthropic / OpenAI / Google / OpenAI-compatible adapters, retries, the model catalog (`catalog/models.toml`) and pricing |
| `tautly-tools` | The `Tool` trait and the built-in tools, the tolerant edit applier, output filters |
| `tautly-sandbox` | The `Sandbox` trait and `LocalSandbox`, the shell parser and the permission policy |
| `tautly-context` | Context assembly, token estimates, pins, working state, compaction and cleanup |
| `tautly-index` | The tree-sitter symbol, reference and trigram index |
| `tautly-memory` | Project memory (the `.tautly/memory/` vault), episodes and `recall` |
| `tautly-session` | The append-only JSONL session store and its blobs |
| `tautly-mcp` | The MCP client (tools only), its configuration and repository-server approvals |
| `tautly-rpc` | The JSON-RPC server and the `-p` one-shot mode |
| `tautly-acp` | The Agent Client Protocol bridge |
| `tautly-tui` | The line REPL |
| `tautly-cli` | The `tautly` binary (package `tautly`) and the utility commands |
| `tautly-testkit` | Test helpers shared by the crates (not published) |
| `tautly-perf` | The performance gates (`benches/perf/`, not published) |
| `tautly-mcp-fixture` | A test MCP server for the MCP tests |

Rules the code keeps:

- **Clients only talk to the engine.** The REPL, RPC and ACP send `Command`s and read
  `Event`s; nothing reaches into engine internals. Protocol additions must stay readable by
  older clients (new fields `Option` or `#[serde(default)]`; unknown values read as
  `Unknown`), see the module docs of `tautly-core/src/protocol.rs`.
- **Tools go through the sandbox.** `crates/tautly-tools/clippy.toml` forbids `std::fs`,
  `tokio::fs` and `std::process::Command` there; use `&dyn Sandbox`.
- **Repository content is untrusted.** A project config, `AGENTS.md`, `.mcp.json`, session
  logs and anything a model writes may be hostile. Keep the existing checks (project-layer
  restrictions, protected paths, escaping with `printable`) when you touch those paths.
- **Bounded by construction.** Channels are bounded, tool output is capped and spilled to
  disk, logs are streamed. New buffers need a cap.
- Errors are typed per crate (`thiserror`); `anyhow` only in `tautly-cli`.
- Every public item has a doc comment (`cargo doc` runs with `-D warnings`).

## Tests and the testkit

Each crate has unit tests next to the code and integration tests in its `tests/`. The
engine's are in `crates/tautly-core/tests/` and use the **`tautly-testkit`** crate (a
dev-dependency):

- `Fx`: a temporary work tree and data home, with `builder(cfg, &mock)` for an engine on a
  `MockProvider` script; `start(builder)` runs it as a `Run` (the engine and its events),
  with helpers to send commands and wait for events (each wait fails after `WAIT`, 30 s);
- `Repo`: a temporary work tree (`Repo::git_repo` for a git repository with its files
  committed, `Repo::plain` without git) and data home; `git_in` runs git with no user or
  system configuration;
- `catalog(window, max_output, edit_format)`: a one-model catalog for `mock/test`;
- `Delegate` / `Delegating`: a sandbox that forwards to a `LocalSandbox` except where a test
  overrides a method (to inject failures).

Prefer these over hand-built engines and fixtures in new tests. Provider adapters are
tested against hand-written SSE fixtures (`crates/tautly-provider/tests/fixtures/`); the
CLI's `-p` JSON output has golden tests (`insta`).

## Performance gates

Merge-blocking, run by the CI `perf` job in release on Linux and macOS (spec 9.1):

```sh
cargo test --release -p tautly -p tautly-perf --features tautly/mock --no-run
bash benches/perf/startup.sh                    # start-up time and idle RSS; TAUTLY_BASELINE_BIN=<binary> for the relative gate
cargo test --release -p tautly -p tautly-perf --features tautly/mock -- --ignored --test-threads=1 --nocapture
./target/release/tautly eval --replay evals/replay --jobs 4    # token regression gate
```

The gates cover cold start (within 1.10× + 2 ms of the merge-base's binary), idle RSS
under 30 MB, a 4-hour simulated session without memory growth, 100 MB of tool output,
index build and reindex times, grep and glob on large generated repositories, command
floods, ACP `session/load` of a 200 MB log, log size and the compaction cut search. The
token gate fails when tokens per solved task on the scripted starter sessions exceed
`evals/baseline.json` by more than 10%. After an intended change in token use, run it with
`--update-baseline` and commit the new baseline with an explanation. See
[`evals/README.md`](evals/README.md) and [`docs/benchmark.md`](docs/benchmark.md).

## Fuzzing

`fuzz/` is a separate Cargo workspace of libFuzzer targets for every parser that reads
untrusted input (shell commands, session logs, project config, RPC and ACP input, MCP
launch lines, escaping, edits). It needs nightly and `cargo-fuzz`; see
[`fuzz/README.md`](fuzz/README.md). After changing a parser, run its target for 10 to 15
minutes, and turn any crash into a unit test in the owning crate before fixing it.

## Dependencies

`cargo deny check` must pass (`deny.toml`: advisories, licences, duplicate and wildcard
bans, sources). Add a dependency only when it pays for itself, pin it in the workspace
`Cargo.toml`, and prefer versions that have been out for a while.

## Releasing

Releases are built by cargo-dist from `v*` tags; see [`docs/release.md`](docs/release.md).

## Commits and pull requests

Keep commits focused, with a message saying what changed and why (the history uses
`type(scope): summary`, e.g. `fix(provider): …`). Update the docs (`README.md`,
`docs/user/`) when you change a command, flag, config key or protocol message. Tautly is
Apache-2.0; by contributing you agree your work is licensed the same way.
