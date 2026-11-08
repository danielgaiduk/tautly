#!/usr/bin/env bash
# Makes the repository of task py-bug-off-by-one in the current (empty) directory.
set -euo pipefail
cat > 'listutil.py' <<'TAUTLY_EOF'
def last_n(items, n):
    """The last n items of items, oldest first."""
    return items[-n - 1:]
TAUTLY_EOF
cat > 'test_listutil.py' <<'TAUTLY_EOF'
import unittest

from listutil import last_n


class LastN(unittest.TestCase):
    def test_last_two(self):
        self.assertEqual(last_n([1, 2, 3, 4], 2), [3, 4])

    def test_zero(self):
        self.assertEqual(last_n([1, 2, 3], 0), [])

    def test_more_than_len(self):
        self.assertEqual(last_n([1, 2], 5), [1, 2])


if __name__ == "__main__":
    unittest.main()
TAUTLY_EOF
git init -q
git add -A
git -c user.name=tautly-eval -c user.email=eval@tautly.invalid commit -qm 'task py-bug-off-by-one'
