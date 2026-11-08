#!/usr/bin/env bash
# Makes the repository of task rs-multi-new-module in the current (empty) directory.
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
//! Small number helpers.

/// Whether `n` is even.
pub fn is_even(n: u64) -> bool {
    n % 2 == 0
}
TAUTLY_EOF
cat > 'tests/it.rs' <<'TAUTLY_EOF'
use task::{gcd, is_even, lcm, math};

#[test]
fn gcd_and_lcm() {
    assert_eq!(gcd(12, 18), 6);
    assert_eq!(gcd(7, 0), 7);
    assert_eq!(lcm(4, 6), 12);
    assert_eq!(math::gcd(9, 3), 3);
    assert!(is_even(4));
}
TAUTLY_EOF
git init -q
git add -A
git -c user.name=tautly-eval -c user.email=eval@tautly.invalid commit -qm 'task rs-multi-new-module'
