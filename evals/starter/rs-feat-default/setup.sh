#!/usr/bin/env bash
# Makes the repository of task rs-feat-default in the current (empty) directory.
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
/// The settings of a run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub retries: u32,
    pub verbose: bool,
    pub name: String,
}
TAUTLY_EOF
cat > 'tests/it.rs' <<'TAUTLY_EOF'
use task::Config;

#[test]
fn defaults() {
    let c = Config::default();
    assert_eq!(c.retries, 3);
    assert!(!c.verbose);
    assert_eq!(c.name, "app");
}

#[test]
fn struct_update() {
    let c = Config { verbose: true, ..Default::default() };
    assert_eq!((c.retries, c.verbose), (3, true));
}
TAUTLY_EOF
git init -q
git add -A
git -c user.name=tautly-eval -c user.email=eval@tautly.invalid commit -qm 'task rs-feat-default'
