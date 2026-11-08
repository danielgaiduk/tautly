#!/usr/bin/env bash
# Makes the repository of task rs-rename-struct in the current (empty) directory.
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
pub mod bank;

/// A bank account.
#[derive(Debug, Default)]
pub struct Acct {
    pub owner: String,
    pub balance: i64,
}

impl Acct {
    pub fn deposit(&mut self, amount: i64) {
        self.balance += amount;
    }
}
TAUTLY_EOF
cat > 'tests/it.rs' <<'TAUTLY_EOF'
use task::bank::open;
use task::Account;

#[test]
fn opens_and_deposits() {
    let mut a: Account = open("ada");
    a.deposit(5);
    assert_eq!((a.owner.as_str(), a.balance), ("ada", 5));
}
TAUTLY_EOF
cat > 'src/bank.rs' <<'TAUTLY_EOF'
use crate::Acct;

/// A new, empty account for `owner`.
pub fn open(owner: &str) -> Acct {
    Acct {
        owner: owner.to_string(),
        balance: 0,
    }
}
TAUTLY_EOF
git init -q
git add -A
git -c user.name=tautly-eval -c user.email=eval@tautly.invalid commit -qm 'task rs-rename-struct'
