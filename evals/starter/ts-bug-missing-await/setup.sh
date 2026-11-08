#!/usr/bin/env bash
# Makes the repository of task ts-bug-missing-await in the current (empty) directory.
set -euo pipefail
mkdir -p 'src' 'test'
cat > 'src/sizes.ts' <<'TAUTLY_EOF'
/** The size of one item, looked up asynchronously. */
export async function sizeOf(name: string): Promise<number> {
  return name.length;
}

/** The sum of the sizes of names. */
export async function totalSize(names: string[]): Promise<number> {
  let total: any = 0;
  for (const name of names) {
    total += sizeOf(name);
  }
  return total;
}
TAUTLY_EOF
cat > 'test/sizes.test.ts' <<'TAUTLY_EOF'
import { test } from "node:test";
import assert from "node:assert/strict";
import { totalSize } from "../src/sizes.ts";

test("sums sizes", async () => {
  assert.equal(await totalSize(["ab", "cde"]), 5);
  assert.equal(await totalSize([]), 0);
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
git -c user.name=tautly-eval -c user.email=eval@tautly.invalid commit -qm 'task ts-bug-missing-await'
