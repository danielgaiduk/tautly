#!/usr/bin/env bash
# Exits 0 when task ts-rename-class is solved (run at the root of its repository).
set -euo pipefail
# tautly eval passes the commit setup made; run by hand, the first commit.
base=${EVAL_BASE:-$(git rev-list --max-parents=0 HEAD | tail -n 1)}
for f in 'test/user.test.ts'; do
  git diff --quiet "$base" -- "$f" || { echo "$f was changed; it must stay as the task gave it"; exit 1; }
done
if grep -rqw Usr src; then echo 'the old name Usr is still used'; exit 1; fi
node --test test/user.test.ts
