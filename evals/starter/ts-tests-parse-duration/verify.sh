#!/usr/bin/env bash
# Exits 0 when task ts-tests-parse-duration is solved (run at the root of its repository).
set -euo pipefail
test -f test/duration.test.ts || { echo 'test/duration.test.ts is missing'; exit 1; }
node --test test/duration.test.ts
# The new tests must fail against broken implementation 1.
dir=$(mktemp -d)
cp -R . "$dir/"
cat > "$dir/src/duration.ts" <<'TAUTLY_EOF'
export function parseDuration(text: string): number {
  const m = /^(?:(\d+)h)?(?:(\d+)m)?(?:(\d+)s)?$/.exec(text);
  if (!m || text === "") throw new Error("bad");
  const [, h = "0", min = "0", s = "0"] = m;
  return Number(h) * 3600 + Number(min) * 6 + Number(s);
}
TAUTLY_EOF
if (cd "$dir" && node --test test/duration.test.ts) >/dev/null 2>&1; then
  rm -rf "$dir"
  echo 'the tests pass against a broken src/duration.ts'
  exit 1
fi
rm -rf "$dir"
# The new tests must fail against broken implementation 2.
dir=$(mktemp -d)
cp -R . "$dir/"
cat > "$dir/src/duration.ts" <<'TAUTLY_EOF'
export function parseDuration(text: string): number {
  const m = /(?:(\d+)h)?(?:(\d+)m)?(?:(\d+)s)?/.exec(text);
  const [, h = "0", min = "0", s = "0"] = m ?? [];
  return Number(h) * 3600 + Number(min) * 60 + Number(s);
}
TAUTLY_EOF
if (cd "$dir" && node --test test/duration.test.ts) >/dev/null 2>&1; then
  rm -rf "$dir"
  echo 'the tests pass against a broken src/duration.ts'
  exit 1
fi
rm -rf "$dir"
