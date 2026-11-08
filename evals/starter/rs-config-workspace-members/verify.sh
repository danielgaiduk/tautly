#!/usr/bin/env bash
# Exits 0 when task rs-config-workspace-members is solved (run at the root of its repository).
set -euo pipefail
# tautly eval passes the commit setup made; run by hand, the first commit.
base=${EVAL_BASE:-$(git rev-list --max-parents=0 HEAD | tail -n 1)}
for f in 'corelib/src/lib.rs' 'cli/src/main.rs'; do
  git diff --quiet "$base" -- "$f" || { echo "$f was changed; it must stay as the task gave it"; exit 1; }
done
test -d corelib || { echo 'corelib was moved'; exit 1; }
cargo test --quiet --offline --workspace
[ "$(cargo run --quiet --offline -p cli)" = 42 ]
