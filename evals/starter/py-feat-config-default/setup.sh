#!/usr/bin/env bash
# Makes the repository of task py-feat-config-default in the current (empty) directory.
set -euo pipefail
cat > 'config.py' <<'TAUTLY_EOF'
def load_config(raw):
    """The configuration from the dict raw."""
    return dict(raw)
TAUTLY_EOF
cat > 'test_config.py' <<'TAUTLY_EOF'
import unittest

from config import load_config


class Defaults(unittest.TestCase):
    def test_defaults(self):
        self.assertEqual(load_config({}), {"port": 8080, "host": "127.0.0.1"})

    def test_given_wins(self):
        c = load_config({"port": 9000, "debug": True})
        self.assertEqual(c, {"port": 9000, "host": "127.0.0.1", "debug": True})

    def test_input_untouched(self):
        raw = {}
        load_config(raw)
        self.assertEqual(raw, {})


if __name__ == "__main__":
    unittest.main()
TAUTLY_EOF
git init -q
git add -A
git -c user.name=tautly-eval -c user.email=eval@tautly.invalid commit -qm 'task py-feat-config-default'
