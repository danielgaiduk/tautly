#!/usr/bin/env bash
# Makes the repository of task rs-extract-method in the current (empty) directory.
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
/// An axis-aligned rectangle.
pub struct Rect {
    pub w: u32,
    pub h: u32,
}

/// The area of `r`.
pub fn area(r: &Rect) -> u64 {
    u64::from(r.w) * u64::from(r.h)
}
TAUTLY_EOF
cat > 'tests/it.rs' <<'TAUTLY_EOF'
use task::Rect;

#[test]
fn area_method() {
    assert_eq!(Rect { w: 3, h: 4 }.area(), 12);
    assert_eq!(Rect { w: u32::MAX, h: 2 }.area(), u64::from(u32::MAX) * 2);
}
TAUTLY_EOF
git init -q
git add -A
git -c user.name=tautly-eval -c user.email=eval@tautly.invalid commit -qm 'task rs-extract-method'
