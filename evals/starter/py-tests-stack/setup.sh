#!/usr/bin/env bash
# Makes the repository of task py-tests-stack in the current (empty) directory.
set -euo pipefail
cat > 'bounded.py' <<'TAUTLY_EOF'
class BoundedStack:
    def __init__(self, capacity):
        self.capacity = capacity
        self.items = []

    def push(self, item):
        if len(self.items) >= self.capacity:
            raise OverflowError("stack is full")
        self.items.append(item)

    def pop(self):
        if not self.items:
            raise IndexError("stack is empty")
        return self.items.pop()
TAUTLY_EOF
git init -q
git add -A
git -c user.name=tautly-eval -c user.email=eval@tautly.invalid commit -qm 'task py-tests-stack'
