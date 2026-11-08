#!/usr/bin/env bash
# Makes the repository of task py-feat-cli-flag in the current (empty) directory.
set -euo pipefail
cat > 'greet.py' <<'TAUTLY_EOF'
import argparse


def greeting(name):
    return f"Hello, {name}!"


def main(argv=None):
    parser = argparse.ArgumentParser(description="Greet someone.")
    parser.add_argument("name")
    args = parser.parse_args(argv)
    print(greeting(args.name))


if __name__ == "__main__":
    main()
TAUTLY_EOF
cat > 'test_greet.py' <<'TAUTLY_EOF'
import subprocess
import sys
import unittest


def run(*args):
    out = subprocess.run([sys.executable, "greet.py", *args], capture_output=True, text=True)
    return out.returncode, out.stdout.strip()


class Greet(unittest.TestCase):
    def test_plain(self):
        self.assertEqual(run("Ada"), (0, "Hello, Ada!"))

    def test_upper(self):
        self.assertEqual(run("Ada", "--upper"), (0, "HELLO, ADA!"))


if __name__ == "__main__":
    unittest.main()
TAUTLY_EOF
git init -q
git add -A
git -c user.name=tautly-eval -c user.email=eval@tautly.invalid commit -qm 'task py-feat-cli-flag'
