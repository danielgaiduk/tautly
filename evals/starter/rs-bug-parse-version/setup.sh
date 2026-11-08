#!/usr/bin/env bash
# Makes the repository of task rs-bug-parse-version in the current (empty) directory.
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
/// `"1.2.3"` as `(1, 2, 3)`.
pub fn parse_version(s: &str) -> Option<(u32, u32, u32)> {
    let parts: Vec<&str> = s.split(',').collect();
    if parts.len() != 3 {
        return None;
    }
    Some((
        parts[0].parse().ok()?,
        parts[1].parse().ok()?,
        parts[2].parse().ok()?,
    ))
}
TAUTLY_EOF
cat > 'tests/it.rs' <<'TAUTLY_EOF'
use task::parse_version;

#[test]
fn parses() {
    assert_eq!(parse_version("1.2.3"), Some((1, 2, 3)));
    assert_eq!(parse_version("10.0.42"), Some((10, 0, 42)));
}

#[test]
fn rejects() {
    assert_eq!(parse_version("1.2"), None);
    assert_eq!(parse_version("1,2,3"), None);
    assert_eq!(parse_version("a.b.c"), None);
}
TAUTLY_EOF
git init -q
git add -A
git -c user.name=tautly-eval -c user.email=eval@tautly.invalid commit -qm 'task rs-bug-parse-version'
