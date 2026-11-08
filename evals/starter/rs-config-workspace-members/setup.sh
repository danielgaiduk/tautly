#!/usr/bin/env bash
# Makes the repository of task rs-config-workspace-members in the current (empty) directory.
set -euo pipefail
mkdir -p 'cli' 'cli/src' 'corelib' 'corelib/src'
cat > '.gitignore' <<'TAUTLY_EOF'
target/
TAUTLY_EOF
cat > 'Cargo.toml' <<'TAUTLY_EOF'
[workspace]
members = ["core", "cli"]
resolver = "2"
TAUTLY_EOF
cat > 'corelib/Cargo.toml' <<'TAUTLY_EOF'
[package]
name = "corelib"
version = "0.1.0"
edition = "2021"
TAUTLY_EOF
cat > 'corelib/src/lib.rs' <<'TAUTLY_EOF'
pub fn double(n: i32) -> i32 {
    n * 2
}

#[cfg(test)]
mod tests {
    #[test]
    fn doubles() {
        assert_eq!(super::double(2), 4);
    }
}
TAUTLY_EOF
cat > 'cli/Cargo.toml' <<'TAUTLY_EOF'
[package]
name = "cli"
version = "0.1.0"
edition = "2021"

[dependencies]
corelib = { path = "../corelib" }
TAUTLY_EOF
cat > 'cli/src/main.rs' <<'TAUTLY_EOF'
fn main() {
    println!("{}", corelib::double(21));
}

#[cfg(test)]
mod tests {
    #[test]
    fn uses_core() {
        assert_eq!(corelib::double(21), 42);
    }
}
TAUTLY_EOF
git init -q
git add -A
git -c user.name=tautly-eval -c user.email=eval@tautly.invalid commit -qm 'task rs-config-workspace-members'
