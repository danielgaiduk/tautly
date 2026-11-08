#!/usr/bin/env bash
# Makes the repository of task rs-bug-empty-slice in the current (empty) directory.
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
/// The largest value of `xs`, if any.
pub fn max_of(xs: &[i32]) -> Option<i32> {
    let mut best = xs[0];
    for &x in &xs[1..] {
        if x > best {
            best = x;
        }
    }
    Some(best)
}
TAUTLY_EOF
cat > 'tests/it.rs' <<'TAUTLY_EOF'
use task::max_of;

#[test]
fn empty_is_none() {
    assert_eq!(max_of(&[]), None);
}

#[test]
fn finds_max() {
    assert_eq!(max_of(&[3, -1, 7, 2]), Some(7));
}
TAUTLY_EOF
git init -q
git add -A
git -c user.name=tautly-eval -c user.email=eval@tautly.invalid commit -qm 'task rs-bug-empty-slice'
