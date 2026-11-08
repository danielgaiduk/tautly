#!/usr/bin/env bash
# Makes the repository of task py-multi-add-field in the current (empty) directory.
set -euo pipefail
cat > 'models.py' <<'TAUTLY_EOF'
from dataclasses import dataclass


@dataclass
class User:
    name: str
TAUTLY_EOF
cat > 'serialize.py' <<'TAUTLY_EOF'
def to_dict(user):
    return {"name": user.name}
TAUTLY_EOF
cat > 'validate.py' <<'TAUTLY_EOF'
def validate(user):
    """Raise ValueError when user is not valid."""
    if not user.name:
        raise ValueError("name is required")
TAUTLY_EOF
cat > 'test_user.py' <<'TAUTLY_EOF'
import unittest

from models import User
from serialize import to_dict
from validate import validate


class UserEmail(unittest.TestCase):
    def test_dict(self):
        self.assertEqual(to_dict(User("ada", "ada@x.io")), {"name": "ada", "email": "ada@x.io"})

    def test_validate(self):
        validate(User("ada", "ada@x.io"))
        with self.assertRaises(ValueError):
            validate(User("ada", "nope"))


if __name__ == "__main__":
    unittest.main()
TAUTLY_EOF
git init -q
git add -A
git -c user.name=tautly-eval -c user.email=eval@tautly.invalid commit -qm 'task py-multi-add-field'
