#!/usr/bin/env bash
# Makes the repository of task py-rename-class in the current (empty) directory.
set -euo pipefail
cat > 'sessions.py' <<'TAUTLY_EOF'
class Mgr:
    """Keeps the open sessions by id."""

    def __init__(self):
        self.open = {}

    def start(self, sid, user):
        self.open[sid] = user

    def end(self, sid):
        return self.open.pop(sid, None)
TAUTLY_EOF
cat > 'app.py' <<'TAUTLY_EOF'
from sessions import Mgr

manager = Mgr()


def login(sid, user):
    manager.start(sid, user)
    return len(manager.open)
TAUTLY_EOF
cat > 'test_sessions.py' <<'TAUTLY_EOF'
import unittest

from app import login, manager
from sessions import SessionManager


class Sessions(unittest.TestCase):
    def test_login(self):
        self.assertIsInstance(manager, SessionManager)
        self.assertEqual(login("s1", "ada"), 1)
        self.assertEqual(manager.end("s1"), "ada")


if __name__ == "__main__":
    unittest.main()
TAUTLY_EOF
git init -q
git add -A
git -c user.name=tautly-eval -c user.email=eval@tautly.invalid commit -qm 'task py-rename-class'
