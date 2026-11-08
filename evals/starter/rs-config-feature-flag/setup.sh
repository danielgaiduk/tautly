#!/usr/bin/env bash
# Makes the repository of task rs-config-feature-flag in the current (empty) directory.
set -euo pipefail
mkdir -p 'src' 'tests'
cat > 'Cargo.toml' <<'TAUTLY_EOF'
[package]
name = "task"
version = "0.1.0"
edition = "2021"

[dependencies]
TAUTLY_EOF
cat > '.gitignore' <<'TAUTLY_EOF'
target/
TAUTLY_EOF
cat > 'src/lib.rs' <<'TAUTLY_EOF'
/// The work function: the fast path when the `fast` feature is on.
pub fn compute(n: u64) -> u64 {
    if cfg!(feature = "fast") {
        n * (n + 1) / 2
    } else {
        (1..=n).sum()
    }
}

/// Whether the fast path is compiled in.
pub fn is_fast() -> bool {
    cfg!(feature = "fast")
}
TAUTLY_EOF
cat > 'tests/it.rs' <<'TAUTLY_EOF'
#[test]
fn computes() {
    assert_eq!(task::compute(100), 5050);
}
TAUTLY_EOF
git init -q
git add -A
git -c user.name=tautly-eval -c user.email=eval@tautly.invalid commit -qm 'task rs-config-feature-flag'
