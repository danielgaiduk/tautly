#!/usr/bin/env bash
# Makes the repository of task py-multi-rename-config-key in the current (empty) directory.
set -euo pipefail
cat > 'defaults.py' <<'TAUTLY_EOF'
DEFAULTS = {"db_url": "sqlite:///app.db", "pool": 5}
TAUTLY_EOF
cat > 'loader.py' <<'TAUTLY_EOF'
from defaults import DEFAULTS


def load(overrides):
    config = dict(DEFAULTS)
    config.update(overrides)
    return config
TAUTLY_EOF
cat > 'db.py' <<'TAUTLY_EOF'
def connect_string(config):
    return config["db_url"] + f"?pool={config['pool']}"
TAUTLY_EOF
cat > 'test_config.py' <<'TAUTLY_EOF'
import unittest

from db import connect_string
from loader import load


class Keys(unittest.TestCase):
    def test_default(self):
        c = load({})
        self.assertEqual(c["database_url"], "sqlite:///app.db")
        self.assertNotIn("db_url", c)

    def test_alias(self):
        self.assertEqual(load({"db_url": "pg://x"})["database_url"], "pg://x")

    def test_connect(self):
        self.assertEqual(connect_string(load({"database_url": "pg://y"})), "pg://y?pool=5")


if __name__ == "__main__":
    unittest.main()
TAUTLY_EOF
git init -q
git add -A
git -c user.name=tautly-eval -c user.email=eval@tautly.invalid commit -qm 'task py-multi-rename-config-key'
