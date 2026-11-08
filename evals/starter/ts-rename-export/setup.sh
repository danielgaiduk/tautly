#!/usr/bin/env bash
# Makes the repository of task ts-rename-export in the current (empty) directory.
set -euo pipefail
mkdir -p 'src' 'test'
cat > 'src/dates.ts' <<'TAUTLY_EOF'
/** A date as YYYY-MM-DD (UTC). */
export function fmtDate(d: Date): string {
  return d.toISOString().slice(0, 10);
}
TAUTLY_EOF
cat > 'src/report.ts' <<'TAUTLY_EOF'
import { fmtDate } from "./dates.ts";

export function header(title: string, when: Date): string {
  return `${title} (${fmtDate(when)})`;
}
TAUTLY_EOF
cat > 'test/dates.test.ts' <<'TAUTLY_EOF'
import { test } from "node:test";
import assert from "node:assert/strict";
import { formatDate } from "../src/dates.ts";
import { header } from "../src/report.ts";

const day = new Date(Date.UTC(2026, 0, 2));

test("formatDate", () => {
  assert.equal(formatDate(day), "2026-01-02");
});

test("header", () => {
  assert.equal(header("Sales", day), "Sales (2026-01-02)");
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
git -c user.name=tautly-eval -c user.email=eval@tautly.invalid commit -qm 'task ts-rename-export'
