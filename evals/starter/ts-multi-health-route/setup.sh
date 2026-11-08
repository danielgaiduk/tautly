#!/usr/bin/env bash
# Makes the repository of task ts-multi-health-route in the current (empty) directory.
set -euo pipefail
mkdir -p 'src' 'test'
cat > 'src/routes.ts' <<'TAUTLY_EOF'
export type Response = { status: number; body: string };

export function home(): Response {
  return { status: 200, body: "welcome" };
}
TAUTLY_EOF
cat > 'src/app.ts' <<'TAUTLY_EOF'
import { home, type Response } from "./routes.ts";

const table: Record<string, () => Response> = {
  "GET /": home,
};

export function handle(method: string, path: string): Response {
  const h = table[`${method} ${path}`];
  return h ? h() : { status: 404, body: "not found" };
}
TAUTLY_EOF
cat > 'test/app.test.ts' <<'TAUTLY_EOF'
import { test } from "node:test";
import assert from "node:assert/strict";
import { handle } from "../src/app.ts";

test("health", () => {
  assert.deepEqual(handle("GET", "/health"), { status: 200, body: "ok" });
});

test("others unchanged", () => {
  assert.equal(handle("GET", "/").body, "welcome");
  assert.equal(handle("POST", "/health").status, 404);
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
git -c user.name=tautly-eval -c user.email=eval@tautly.invalid commit -qm 'task ts-multi-health-route'
