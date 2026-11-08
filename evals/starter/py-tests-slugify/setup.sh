#!/usr/bin/env bash
# Makes the repository of task py-tests-slugify in the current (empty) directory.
set -euo pipefail
cat > 'slug.py' <<'TAUTLY_EOF'
import re


def slugify(text):
    """`Hello, World!` -> `hello-world`."""
    text = re.sub(r"[^a-z0-9\s-]", "", text.lower())
    return re.sub(r"[\s-]+", "-", text).strip("-")
TAUTLY_EOF
git init -q
git add -A
git -c user.name=tautly-eval -c user.email=eval@tautly.invalid commit -qm 'task py-tests-slugify'
