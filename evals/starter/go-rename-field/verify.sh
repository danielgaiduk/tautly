#!/usr/bin/env bash
# Exits 0 when task go-rename-field is solved (run at the root of its repository).
set -euo pipefail
base=$(git rev-list --max-parents=0 HEAD | tail -n 1)
for f in 'item_test.go'; do
  git diff --quiet "$base" -- "$f" || { echo "$f was changed; it must stay as the task gave it"; exit 1; }
done
if grep -qw Nm ./*.go; then echo 'the old name Nm is still used'; exit 1; fi
go test ./...
