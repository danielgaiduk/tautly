# Output filter fixtures

These files are **hand-authored** from knowledge of each tool's text output
format (no tool was run to produce them, and the environment had no network or
toolchains for the non-Rust tools). They follow the non-TTY, uncolored output of:

- `cargo_test_fail.txt` - `cargo test` (Rust 1.8x), one compiler warning, two failing tests
- `cargo_build_err.txt` - `cargo build`, two errors and one warning
- `clippy_warn.txt` - `cargo clippy`, three lint warnings
- `pytest_fail.txt` - `pytest` (default verbosity), 2 failed + 1 error
- `pytest_quiet_fail.txt` - `pytest -q`, 2 failed
- `jest_fail.txt` - `jest`, two failing tests in one suite
- `vitest_fail.txt` - `vitest run`, two failing tests
- `gotest_fail.txt` - `go test -v ./...`, a failing test with a failing subtest
- `tsc_err.txt` - `tsc --noEmit`, 4 errors in 2 files
- `eslint_err.txt` - `eslint .` (stylish formatter), 5 problems

If a real tool's format drifts, replace a fixture with a captured run and adjust
the expectations in `tests/filters.rs`; the parsers key on stable markers
(`--> file:line`, `FAILED path::name`, `--- FAIL:`, `file(line,col): error TS`,
stylish blocks, `●` blocks) rather than on exact spacing.
