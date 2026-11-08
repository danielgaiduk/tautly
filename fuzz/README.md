# Fuzzing

libFuzzer targets for the parsers that read untrusted input: shell commands from the
model, session logs, a repository's `.tautly/config.toml` and MCP launch lines, and what
editor and protocol clients send. Each target asserts its invariants, not only "no
panic"; the doc comment at the top of each file in `fuzz_targets/` lists them.

| Target | What it reads |
| --- | --- |
| `shell_policy` | `parse_commands`, `Policy::exact_rule`, allow-rule matching (`tautly-sandbox`) |
| `session_reader` | `SessionReader` on arbitrary bytes, record paths (`tautly-session`, `tautly-core`) |
| `timetravel` | generated record sequences through `state_at`, `step_at_offset`, `rebuild` |
| `config_project` | user plus project config loading: the project layer never loosens |
| `rpc_acp` | JSON-RPC framing and parsing, the RPC server and ACP bridge, ACP helpers |
| `mcp_launch` | the approval key of a repository stdio MCP server (`launch_files.rs`) |
| `escaping` | `printable`, `printable_multiline` |
| `edit_apply` | `str_replace`, the tolerant locator, `hashline`, `patch` |

This directory is its own Cargo workspace: the main workspace, its CI and `cargo test`
never build it and stay on the pinned stable toolchain.

## Running

libFuzzer needs a nightly toolchain and `cargo-fuzz`:

```sh
rustup toolchain install nightly --profile minimal
cargo install --locked cargo-fuzz --version 0.13.2
cd fuzz
cargo +nightly fuzz list
# New inputs go to the first directory; the committed seeds stay as they are.
mkdir -p /tmp/tautly-fuzz/shell_policy
cargo +nightly fuzz run shell_policy /tmp/tautly-fuzz/shell_policy corpus/shell_policy -- \
  -max_total_time=900 -timeout=10 -rss_limit_mb=2048 -max_len=4096
```

`mcp_launch` needs long inputs to reach its bounds (a repository config may hold
256 KiB): run it with `-max_len=65536` or more.

A crash leaves its input under `fuzz/artifacts/<target>/` (or `-artifact_prefix`).
Reproduce with `cargo +nightly fuzz run <target> <file>`, shrink with
`cargo +nightly fuzz tmin <target> <file>`, then add the minimal input as a unit test in
the owning crate before fixing it.

## Time budget

- Nightly CI (`.github/workflows/nightly.yml`, job `fuzz`): 60 s per target, a smoke run
  that catches regressions in the committed corpus and the shallow paths.

The shell parse has a 2 s deadline (`tautly-sandbox`, `PARSE_DEADLINE`): an input past
it is Unparseable, and until that parse ends (minutes for the known tree-sitter case,
`crates/tautly-sandbox/tests/fixtures/slow_parse.sh`) every later parse in the process
is refused. `shell_policy` accepts those refusals, but a run then checks little: never
add a slow input to `corpus/`.
- Before a release or after changing a parser: 10 to 15 minutes each for
  `shell_policy`, `session_reader` and `rpc_acp`, 5 minutes each for the rest. The
  targets are independent; run them in parallel, one per core.

## Corpus

`corpus/<target>/` holds small seed corpora made from the workspace's own test inputs
by `seed.py` (`python3 fuzz/seed.py` rewrites them; files are named by content hash).
Commit only seeds that add something (a regression input, a new shape), never a whole
grown corpus.
