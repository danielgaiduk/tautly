#!/usr/bin/env bash
# Makes the repository of task py-config-ini-section in the current (empty) directory.
set -euo pipefail
cat > 'settings.ini' <<'TAUTLY_EOF'
[sever]
host = 0.0.0.0
port = 8000
TAUTLY_EOF
cat > 'app.py' <<'TAUTLY_EOF'
import configparser
import os


def address():
    config = configparser.ConfigParser()
    config.read(os.path.join(os.path.dirname(__file__) or ".", "settings.ini"))
    return f"{config.get('server', 'host')}:{config.getint('server', 'port')}"


if __name__ == "__main__":
    print(address())
TAUTLY_EOF
cat > 'test_app.py' <<'TAUTLY_EOF'
import unittest

from app import address


class Address(unittest.TestCase):
    def test_address(self):
        self.assertEqual(address(), "0.0.0.0:8000")


if __name__ == "__main__":
    unittest.main()
TAUTLY_EOF
git init -q
git add -A
git -c user.name=tautly-eval -c user.email=eval@tautly.invalid commit -qm 'task py-config-ini-section'
