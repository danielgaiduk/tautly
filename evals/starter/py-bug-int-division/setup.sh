#!/usr/bin/env bash
# Makes the repository of task py-bug-int-division in the current (empty) directory.
set -euo pipefail
cat > 'stats.py' <<'TAUTLY_EOF'
def average(values):
    """The arithmetic mean of values (0.0 when there are none)."""
    if not values:
        return 0.0
    return sum(values) // len(values)
TAUTLY_EOF
cat > 'test_stats.py' <<'TAUTLY_EOF'
import unittest

from stats import average


class Average(unittest.TestCase):
    def test_fraction(self):
        self.assertEqual(average([1, 2]), 1.5)

    def test_empty(self):
        self.assertEqual(average([]), 0.0)

    def test_whole(self):
        self.assertEqual(average([2, 4, 6]), 4)


if __name__ == "__main__":
    unittest.main()
TAUTLY_EOF
git init -q
git add -A
git -c user.name=tautly-eval -c user.email=eval@tautly.invalid commit -qm 'task py-bug-int-division'
