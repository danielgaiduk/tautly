//! The parse deadline, in its own test binary: the stuck parse it starts refuses every
//! other parse in the process until it ends (minutes), so it cannot share one.

use tautly_sandbox::parse_commands;

/// Fuzzing (shell_policy): on this 899-byte input tree-sitter's error recovery runs for
/// minutes (inside `ts_stack_merge`, where no progress callback is consulted), which
/// hung the policy check. The parse has a deadline: past it the input is unparseable, and
/// while that parse still runs every other input is refused at once (one stuck worker at
/// most), then parses again once it ends.
#[test]
fn a_parse_that_runs_too_long_is_unparseable() {
    let src = std::str::from_utf8(include_bytes!("fixtures/slow_parse.sh")).unwrap();
    let started = std::time::Instant::now();
    let err = parse_commands(src).unwrap_err();
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "{err}"
    );
    assert!(err.0.contains("too long"), "{err}");
    // The stuck parse is still running: the next input is refused without waiting.
    let started = std::time::Instant::now();
    let err = parse_commands("ls").unwrap_err();
    assert!(started.elapsed() < std::time::Duration::from_millis(500));
    assert!(err.0.contains("still running"), "{err}");
}
