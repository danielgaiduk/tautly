#!/usr/bin/env bash
# Makes the repository of task rs-feat-display in the current (empty) directory.
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
/// A point on the integer grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Point {
    pub x: i64,
    pub y: i64,
}
TAUTLY_EOF
cat > 'tests/it.rs' <<'TAUTLY_EOF'
use task::Point;

#[test]
fn displays() {
    assert_eq!(Point { x: 1, y: -2 }.to_string(), "(1, -2)");
    assert_eq!(format!("{}", Point { x: 0, y: 0 }), "(0, 0)");
}
TAUTLY_EOF
git init -q
git add -A
git -c user.name=tautly-eval -c user.email=eval@tautly.invalid commit -qm 'task rs-feat-display'
