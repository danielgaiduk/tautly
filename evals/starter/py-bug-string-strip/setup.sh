#!/usr/bin/env bash
# Makes the repository of task py-bug-string-strip in the current (empty) directory.
set -euo pipefail
cat > 'kv.py' <<'TAUTLY_EOF'
def parse_kv(text):
    """Parse `key = value` lines into a dict; blank lines and `#` comments are skipped."""
    out = {}
    for line in text.splitlines():
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        key, value = line.split("=", 1)
        out[key] = value
    return out
TAUTLY_EOF
cat > 'test_kv.py' <<'TAUTLY_EOF'
import unittest

from kv import parse_kv


class ParseKv(unittest.TestCase):
    def test_strips(self):
        self.assertEqual(parse_kv("a = b\n  c=d  \n"), {"a": "b", "c": "d"})

    def test_comments_and_blank(self):
        self.assertEqual(parse_kv("# x\n\nk = v = w\n"), {"k": "v = w"})


if __name__ == "__main__":
    unittest.main()
TAUTLY_EOF
git init -q
git add -A
git -c user.name=tautly-eval -c user.email=eval@tautly.invalid commit -qm 'task py-bug-string-strip'
