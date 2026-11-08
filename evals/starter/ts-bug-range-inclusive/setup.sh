#!/usr/bin/env bash
# Makes the repository of task ts-bug-range-inclusive in the current (empty) directory.
set -euo pipefail
mkdir -p 'src' 'test'
cat > 'src/range.ts' <<'TAUTLY_EOF'
/** The integers from a to b, both included. */
export function range(a: number, b: number): number[] {
  const out: number[] = [];
  for (let i = a; i < b; i++) out.push(i);
  return out;
}
TAUTLY_EOF
cat > 'test/range.test.ts' <<'TAUTLY_EOF'
import { test } from "node:test";
import assert from "node:assert/strict";
import { range } from "../src/range.ts";

test("inclusive", () => {
  assert.deepEqual(range(1, 3), [1, 2, 3]);
  assert.deepEqual(range(2, 2), [2]);
});

test("empty when reversed", () => {
  assert.deepEqual(range(3, 1), []);
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
git -c user.name=tautly-eval -c user.email=eval@tautly.invalid commit -qm 'task ts-bug-range-inclusive'
