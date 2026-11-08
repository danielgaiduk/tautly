#!/usr/bin/env bash
# Makes the repository of task ts-feat-capitalize in the current (empty) directory.
set -euo pipefail
mkdir -p 'src' 'test'
cat > 'src/strings.ts' <<'TAUTLY_EOF'
/** text reversed, by UTF-16 code units. */
export function reverse(text: string): string {
  return text.split("").reverse().join("");
}
TAUTLY_EOF
cat > 'test/strings.test.ts' <<'TAUTLY_EOF'
import { test } from "node:test";
import assert from "node:assert/strict";
import { capitalize, reverse } from "../src/strings.ts";

test("capitalize", () => {
  assert.equal(capitalize("hello world"), "Hello world");
  assert.equal(capitalize("ABC"), "ABC");
  assert.equal(capitalize(""), "");
});

test("reverse still works", () => {
  assert.equal(reverse("ab"), "ba");
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
git -c user.name=tautly-eval -c user.email=eval@tautly.invalid commit -qm 'task ts-feat-capitalize'
