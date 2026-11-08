#!/usr/bin/env bash
# Makes the repository of task ts-rename-class in the current (empty) directory.
set -euo pipefail
mkdir -p 'src' 'test'
cat > 'src/user.ts' <<'TAUTLY_EOF'
export class Usr {
  name: string;
  admin: boolean;

  constructor(name: string, admin = false) {
    this.name = name;
    this.admin = admin;
  }

  greet(): string {
    return `hi ${this.name}`;
  }
}
TAUTLY_EOF
cat > 'src/admin.ts' <<'TAUTLY_EOF'
import { Usr } from "./user.ts";

export function makeAdmin(name: string): Usr {
  return new Usr(name, true);
}
TAUTLY_EOF
cat > 'test/user.test.ts' <<'TAUTLY_EOF'
import { test } from "node:test";
import assert from "node:assert/strict";
import { User } from "../src/user.ts";
import { makeAdmin } from "../src/admin.ts";

test("admin", () => {
  const u = makeAdmin("ada");
  assert.ok(u instanceof User);
  assert.equal(u.admin, true);
  assert.equal(u.greet(), "hi ada");
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
git -c user.name=tautly-eval -c user.email=eval@tautly.invalid commit -qm 'task ts-rename-class'
