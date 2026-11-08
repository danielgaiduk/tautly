#!/usr/bin/env bash
# Makes the repository of task ts-feat-clamp in the current (empty) directory.
set -euo pipefail
mkdir -p 'src' 'test'
cat > 'src/math.ts' <<'TAUTLY_EOF'
export function lerp(a: number, b: number, t: number): number {
  return a + (b - a) * t;
}
TAUTLY_EOF
cat > 'test/math.test.ts' <<'TAUTLY_EOF'
import { test } from "node:test";
import assert from "node:assert/strict";
import { clamp } from "../src/math.ts";

test("clamps", () => {
  assert.equal(clamp(5, 0, 3), 3);
  assert.equal(clamp(-1, 0, 3), 0);
  assert.equal(clamp(2, 0, 3), 2);
});

test("bad range", () => {
  assert.throws(() => clamp(1, 3, 0), RangeError);
});
TAUTLY_EOF
cat > 'package.json' <<'TAUTLY_EOF'
{
  "name": "task",
  "private": true,
  "type": "module"
}
TAUTLY_EOF
git init -q
git add -A
git -c user.name=tautly-eval -c user.email=eval@tautly.invalid commit -qm 'task ts-feat-clamp'
