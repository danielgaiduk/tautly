#!/usr/bin/env bash
# Makes the repository of task ts-tests-parse-duration in the current (empty) directory.
set -euo pipefail
mkdir -p 'src'
cat > 'package.json' <<'TAUTLY_EOF'
{
  "name": "task",
  "private": true,
  "type": "module"
}
TAUTLY_EOF
cat > 'src/duration.ts' <<'TAUTLY_EOF'
/** "1h30m15s" -> seconds. Throws on anything else. */
export function parseDuration(text: string): number {
  const m = /^(?:(\d+)h)?(?:(\d+)m)?(?:(\d+)s)?$/.exec(text);
  if (!m || text === "") throw new Error(`not a duration: ${text}`);
  const [, h = "0", min = "0", s = "0"] = m;
  return Number(h) * 3600 + Number(min) * 60 + Number(s);
}
TAUTLY_EOF
git init -q
git add -A
git -c user.name=tautly-eval -c user.email=eval@tautly.invalid commit -qm 'task ts-tests-parse-duration'
