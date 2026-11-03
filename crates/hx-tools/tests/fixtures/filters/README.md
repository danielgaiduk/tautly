# Output filter fixtures

These files are **hand-authored** from knowledge of each tool's text output
format (no tool was run to produce them, and the environment had no network or
toolchains for the non-Rust tools). They follow the non-TTY, uncolored output of:

- `cargo_test_fail.txt` - `cargo test` (Rust 1.8x), one compiler warning, two failing tests
- `cargo_build_err.txt` - `cargo build`, three errors and one warning
- `clippy_warn.txt` - `cargo clippy`, four lint warnings
- `pytest_fail.txt` - `pytest` (default verbosity), 2 failed + 1 error
- `pytest_quiet_fail.txt` - `pytest -q`, 3 failed
- `jest_fail.txt` - `jest`, three failing tests in one suite plus one suite that fails to run
- `vitest_fail.txt` - `vitest run`, three failing tests
- `gotest_fail.txt` - `go test -v ./...`, five failing tests/subtests (two with failing parents)
- `tsc_err.txt` - `tsc --noEmit`, 20 errors in 6 files
- `eslint_err.txt` - `eslint .` (stylish formatter), 19 problems (12 errors, 7 warnings)

If a real tool's format drifts, replace a fixture with a captured run and adjust
the expectations in `tests/filters.rs`; the parsers key on stable markers
(`--> file:line`, `FAILED path::name`, `--- FAIL:`, `file(line,col): error TS`,
stylish blocks, `●` blocks) rather than on exact spacing.
