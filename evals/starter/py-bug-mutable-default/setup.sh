#!/usr/bin/env bash
# Makes the repository of task py-bug-mutable-default in the current (empty) directory.
set -euo pipefail
cat > 'bucket.py' <<'TAUTLY_EOF'
def add_item(item, bucket=[]):
    """bucket with item appended (a new list when no bucket is given)."""
    bucket.append(item)
    return bucket
TAUTLY_EOF
cat > 'test_bucket.py' <<'TAUTLY_EOF'
import unittest

from bucket import add_item


class AddItem(unittest.TestCase):
    def test_fresh_bucket_each_call(self):
        self.assertEqual(add_item("a"), ["a"])
        self.assertEqual(add_item("b"), ["b"])

    def test_given_bucket(self):
        b = ["x"]
        self.assertEqual(add_item("y", b), ["x", "y"])


if __name__ == "__main__":
    unittest.main()
TAUTLY_EOF
git init -q
git add -A
git -c user.name=tautly-eval -c user.email=eval@tautly.invalid commit -qm 'task py-bug-mutable-default'
