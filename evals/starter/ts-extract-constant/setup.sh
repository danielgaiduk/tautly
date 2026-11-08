#!/usr/bin/env bash
# Makes the repository of task ts-extract-constant in the current (empty) directory.
set -euo pipefail
mkdir -p 'src' 'test'
cat > 'src/prices.ts' <<'TAUTLY_EOF'
export function withTax(net: number): number {
  return net * (1 + 0.2);
}

export function taxOf(net: number): number {
  return net * 0.2;
}

export function netOf(gross: number): number {
  return gross / (1 + 0.2);
}
TAUTLY_EOF
cat > 'test/prices.test.ts' <<'TAUTLY_EOF'
import { test } from "node:test";
import assert from "node:assert/strict";
import { TAX_RATE, netOf, taxOf, withTax } from "../src/prices.ts";

test("constant", () => {
  assert.equal(TAX_RATE, 0.2);
});

test("prices", () => {
  assert.equal(withTax(100), 120);
  assert.equal(taxOf(100), 20);
  assert.equal(netOf(120), 100);
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
git -c user.name=tautly-eval -c user.email=eval@tautly.invalid commit -qm 'task ts-extract-constant'
