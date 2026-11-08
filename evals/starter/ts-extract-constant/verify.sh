#!/usr/bin/env bash
# Exits 0 when task ts-extract-constant is solved (run at the root of its repository).
set -euo pipefail
base=$(git rev-list --max-parents=0 HEAD | tail -n 1)
for f in 'test/prices.test.ts'; do
  git diff --quiet "$base" -- "$f" || { echo "$f was changed; it must stay as the task gave it"; exit 1; }
done
if [ "$(grep -o '0\.2' src/prices.ts | wc -l)" -ne 1 ]; then echo '0.2 should appear once'; exit 1; fi
node --test test/prices.test.ts
