#!/usr/bin/env bash
# Exits 0 when task py-tests-stack is solved (run at the root of its repository).
set -euo pipefail
# tautly eval passes the commit setup made; run by hand, the first commit.
base=${EVAL_BASE:-$(git rev-list --max-parents=0 HEAD | tail -n 1)}
for f in 'bounded.py'; do
  git diff --quiet "$base" -- "$f" || { echo "$f was changed; it must stay as the task gave it"; exit 1; }
done
test -f test_bounded.py || { echo 'test_bounded.py is missing'; exit 1; }
python3 -m unittest -q test_bounded
# The new tests must fail against broken implementation 1.
dir=$(mktemp -d)
cp -R . "$dir/"
cat > "$dir/bounded.py" <<'TAUTLY_EOF'
class BoundedStack:
    def __init__(self, capacity):
        self.capacity = capacity
        self.items = []

    def push(self, item):
        self.items.append(item)

    def pop(self):
        if not self.items:
            raise IndexError('stack is empty')
        return self.items.pop()
TAUTLY_EOF
if (cd "$dir" && python3 -m unittest -q test_bounded) >/dev/null 2>&1; then
  rm -rf "$dir"
  echo 'the tests pass against a broken bounded.py'
  exit 1
fi
rm -rf "$dir"
# The new tests must fail against broken implementation 2.
dir=$(mktemp -d)
cp -R . "$dir/"
cat > "$dir/bounded.py" <<'TAUTLY_EOF'
class BoundedStack:
    def __init__(self, capacity):
        self.capacity = capacity
        self.items = []

    def push(self, item):
        if len(self.items) >= self.capacity:
            raise OverflowError('stack is full')
        self.items.append(item)

    def pop(self):
        if not self.items:
            raise IndexError('stack is empty')
        return self.items.pop(0)
TAUTLY_EOF
if (cd "$dir" && python3 -m unittest -q test_bounded) >/dev/null 2>&1; then
  rm -rf "$dir"
  echo 'the tests pass against a broken bounded.py'
  exit 1
fi
rm -rf "$dir"
