#!/usr/bin/env bash
# Makes the repository of task ts-config-test-script in the current (empty) directory.
set -euo pipefail
mkdir -p 'src' 'test'
cat > 'package.json' <<'TAUTLY_EOF'
{
  "name": "task",
  "private": true,
  "type": "module",
  "scripts": {
    "test": "node --test tests/greet.spec.ts"
  }
}
TAUTLY_EOF
cat > 'src/greet.ts' <<'TAUTLY_EOF'
export function greet(name: string): string {
  return `Hello, ${name}!`;
}
TAUTLY_EOF
cat > 'test/greet.test.ts' <<'TAUTLY_EOF'
import { test } from "node:test";
import assert from "node:assert/strict";
import { greet } from "../src/greet.ts";

test("greet", () => {
  assert.equal(greet("Ada"), "Hello, Ada!");
});
TAUTLY_EOF
git init -q
git add -A
git -c user.name=tautly-eval -c user.email=eval@tautly.invalid commit -qm 'task ts-config-test-script'
