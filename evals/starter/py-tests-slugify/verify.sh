#!/usr/bin/env bash
# Exits 0 when task py-tests-slugify is solved (run at the root of its repository).
set -euo pipefail
test -f test_slug.py || { echo 'test_slug.py is missing'; exit 1; }
python3 -m unittest -q test_slug
# The new tests must fail against broken implementation 1.
dir=$(mktemp -d)
cp -R . "$dir/"
cat > "$dir/slug.py" <<'TAUTLY_EOF'
import re


def slugify(text):
    return re.sub(r"\s+", "-", text.lower())
TAUTLY_EOF
if (cd "$dir" && python3 -m unittest -q test_slug) >/dev/null 2>&1; then
  rm -rf "$dir"
  echo 'the tests pass against a broken slug.py'
  exit 1
fi
rm -rf "$dir"
# The new tests must fail against broken implementation 2.
dir=$(mktemp -d)
cp -R . "$dir/"
cat > "$dir/slug.py" <<'TAUTLY_EOF'
import re


def slugify(text):
    text = re.sub(r"[^a-z0-9\s-]", "", text.lower())
    return text.replace(" ", "-")
TAUTLY_EOF
if (cd "$dir" && python3 -m unittest -q test_slug) >/dev/null 2>&1; then
  rm -rf "$dir"
  echo 'the tests pass against a broken slug.py'
  exit 1
fi
rm -rf "$dir"
