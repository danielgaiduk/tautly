#!/usr/bin/env bash
# Exits 0 when task py-rename-class is solved (run at the root of its repository).
set -euo pipefail
base=$(git rev-list --max-parents=0 HEAD | tail -n 1)
for f in 'test_sessions.py'; do
  git diff --quiet "$base" -- "$f" || { echo "$f was changed; it must stay as the task gave it"; exit 1; }
done
if grep -rqw Mgr --include='*.py' .; then echo 'the old name Mgr is still used'; exit 1; fi
python3 -m unittest -q test_sessions
