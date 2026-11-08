#!/usr/bin/env bash
# Exits 0 when task ts-feat-clamp is solved (run at the root of its repository).
set -euo pipefail
base=$(git rev-list --max-parents=0 HEAD | tail -n 1)
for f in 'test/math.test.ts'; do
  git diff --quiet "$base" -- "$f" || { echo "$f was changed; it must stay as the task gave it"; exit 1; }
done
node --test test/math.test.ts
