#!/usr/bin/env bash
# Makes the repository of task py-feat-stack-peek in the current (empty) directory.
set -euo pipefail
cat > 'stack.py' <<'TAUTLY_EOF'
class Stack:
    """A last-in, first-out stack."""

    def __init__(self):
        self._items = []

    def push(self, item):
        self._items.append(item)

    def pop(self):
        return self._items.pop()

    def __len__(self):
        return len(self._items)
TAUTLY_EOF
cat > 'test_stack.py' <<'TAUTLY_EOF'
import unittest

from stack import Stack


class Peek(unittest.TestCase):
    def test_peek_keeps_item(self):
        s = Stack()
        s.push(1)
        s.push(2)
        self.assertEqual(s.peek(), 2)
        self.assertEqual(len(s), 2)

    def test_peek_empty(self):
        self.assertIsNone(Stack().peek())


if __name__ == "__main__":
    unittest.main()
TAUTLY_EOF
git init -q
git add -A
git -c user.name=tautly-eval -c user.email=eval@tautly.invalid commit -qm 'task py-feat-stack-peek'
