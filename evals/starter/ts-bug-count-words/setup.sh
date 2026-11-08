#!/usr/bin/env bash
# Makes the repository of task ts-bug-count-words in the current (empty) directory.
set -euo pipefail
mkdir -p 'src' 'test'
cat > 'src/words.ts' <<'TAUTLY_EOF'
/** How many whitespace-separated words text has. */
export function countWords(text: string): number {
  return text.split(" ").length;
}
TAUTLY_EOF
cat > 'test/words.test.ts' <<'TAUTLY_EOF'
import { test } from "node:test";
import assert from "node:assert/strict";
import { countWords } from "../src/words.ts";

test("empty and blank", () => {
  assert.equal(countWords(""), 0);
  assert.equal(countWords("   "), 0);
});

test("runs of whitespace", () => {
  assert.equal(countWords(" one  two\tthree\n"), 3);
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
git -c user.name=tautly-eval -c user.email=eval@tautly.invalid commit -qm 'task ts-bug-count-words'
