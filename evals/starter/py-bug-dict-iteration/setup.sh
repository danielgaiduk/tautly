#!/usr/bin/env bash
# Makes the repository of task py-bug-dict-iteration in the current (empty) directory.
set -euo pipefail
cat > 'cleanup.py' <<'TAUTLY_EOF'
def remove_empty(d):
    """Remove the keys of d whose value is empty (None, "", [] or {}); return d."""
    for key, value in d.items():
        if value in (None, "", [], {}):
            del d[key]
    return d
TAUTLY_EOF
cat > 'test_cleanup.py' <<'TAUTLY_EOF'
import unittest

from cleanup import remove_empty


class RemoveEmpty(unittest.TestCase):
    def test_removes_empty_values(self):
        d = {"a": 1, "b": "", "c": None, "d": [], "e": "x"}
        out = remove_empty(d)
        self.assertIs(out, d)
        self.assertEqual(d, {"a": 1, "e": "x"})

    def test_keeps_zero(self):
        self.assertEqual(remove_empty({"n": 0}), {"n": 0})


if __name__ == "__main__":
    unittest.main()
TAUTLY_EOF
git init -q
git add -A
git -c user.name=tautly-eval -c user.email=eval@tautly.invalid commit -qm 'task py-bug-dict-iteration'
