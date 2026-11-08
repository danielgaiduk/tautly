#!/usr/bin/env bash
# Makes the repository of task ts-bug-sort-numbers in the current (empty) directory.
set -euo pipefail
mkdir -p 'src' 'test'
cat > 'src/sort.ts' <<'TAUTLY_EOF'
/** The numbers in ascending order (a new array). */
export function sortNumbers(xs: number[]): number[] {
  return [...xs].sort();
}
TAUTLY_EOF
cat > 'test/sort.test.ts' <<'TAUTLY_EOF'
import { test } from "node:test";
import assert from "node:assert/strict";
import { sortNumbers } from "../src/sort.ts";

test("numeric order", () => {
  assert.deepEqual(sortNumbers([10, 9, 1, 100]), [1, 9, 10, 100]);
});

test("input untouched", () => {
  const xs = [3, 1];
  sortNumbers(xs);
  assert.deepEqual(xs, [3, 1]);
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
git -c user.name=tautly-eval -c user.email=eval@tautly.invalid commit -qm 'task ts-bug-sort-numbers'
