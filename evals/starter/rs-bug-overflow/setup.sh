#!/usr/bin/env bash
# Makes the repository of task rs-bug-overflow in the current (empty) directory.
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
/// The arithmetic mean of `xs` (0.0 when empty).
pub fn mean(xs: &[u8]) -> f64 {
    if xs.is_empty() {
        return 0.0;
    }
    let mut sum: u8 = 0;
    for x in xs {
        sum += x;
    }
    sum as f64 / xs.len() as f64
}
TAUTLY_EOF
cat > 'tests/it.rs' <<'TAUTLY_EOF'
use task::mean;

#[test]
fn large_values() {
    assert_eq!(mean(&[200, 200, 255]), 655.0 / 3.0);
}

#[test]
fn empty() {
    assert_eq!(mean(&[]), 0.0);
}
TAUTLY_EOF
git init -q
git add -A
git -c user.name=tautly-eval -c user.email=eval@tautly.invalid commit -qm 'task rs-bug-overflow'
