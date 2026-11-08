#!/usr/bin/env bash
# Makes the repository of task py-feat-json-export in the current (empty) directory.
set -euo pipefail
cat > 'todo.py' <<'TAUTLY_EOF'
from dataclasses import dataclass


@dataclass
class Todo:
    title: str
    done: bool = False
TAUTLY_EOF
cat > 'test_todo.py' <<'TAUTLY_EOF'
import json
import unittest

from todo import Todo


class ToJson(unittest.TestCase):
    def test_round_trip(self):
        self.assertEqual(json.loads(Todo("milk").to_json()), {"title": "milk", "done": False})

    def test_sorted_keys(self):
        self.assertEqual(Todo("x", True).to_json(), '{"done": true, "title": "x"}')


if __name__ == "__main__":
    unittest.main()
TAUTLY_EOF
git init -q
git add -A
git -c user.name=tautly-eval -c user.email=eval@tautly.invalid commit -qm 'task py-feat-json-export'
