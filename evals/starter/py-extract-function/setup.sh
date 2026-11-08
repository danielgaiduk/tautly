#!/usr/bin/env bash
# Makes the repository of task py-extract-function in the current (empty) directory.
set -euo pipefail
cat > 'signup.py' <<'TAUTLY_EOF'
import re


def register(email, password):
    if not re.fullmatch(r"[^@\s]+@[^@\s]+\.[a-z]{2,}", email):
        raise ValueError("invalid email")
    if len(password) < 8:
        raise ValueError("password too short")
    return {"email": email}


def change_email(user, email):
    if not re.fullmatch(r"[^@\s]+@[^@\s]+\.[a-z]{2,}", email):
        raise ValueError("invalid email")
    user["email"] = email
    return user
TAUTLY_EOF
cat > 'test_signup.py' <<'TAUTLY_EOF'
import unittest

from signup import change_email, is_valid_email, register


class Email(unittest.TestCase):
    def test_valid(self):
        self.assertTrue(is_valid_email("a@b.io"))
        self.assertFalse(is_valid_email("a@b"))

    def test_register(self):
        self.assertEqual(register("a@b.io", "longpassword"), {"email": "a@b.io"})
        with self.assertRaises(ValueError):
            register("nope", "longpassword")

    def test_change(self):
        with self.assertRaises(ValueError):
            change_email({}, "x y@z.com")


if __name__ == "__main__":
    unittest.main()
TAUTLY_EOF
git init -q
git add -A
git -c user.name=tautly-eval -c user.email=eval@tautly.invalid commit -qm 'task py-extract-function'
