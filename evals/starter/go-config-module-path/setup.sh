#!/usr/bin/env bash
# Makes the repository of task go-config-module-path in the current (empty) directory.
set -euo pipefail
mkdir -p 'mathx'
cat > 'go.mod' <<'TAUTLY_EOF'
module example.com/calculator

go 1.21
TAUTLY_EOF
cat > 'main.go' <<'TAUTLY_EOF'
package main

import (
	"fmt"

	"example.com/calc/mathx"
)

func main() {
	fmt.Println(mathx.Add(2, 3))
}
TAUTLY_EOF
cat > 'mathx/add.go' <<'TAUTLY_EOF'
package mathx

// Add is a + b.
func Add(a, b int) int {
	return a + b
}
TAUTLY_EOF
cat > 'mathx/add_test.go' <<'TAUTLY_EOF'
package mathx

import "testing"

func TestAdd(t *testing.T) {
	if Add(2, 3) != 5 {
		t.Fatal("Add")
	}
}
TAUTLY_EOF
git init -q
git add -A
git -c user.name=tautly-eval -c user.email=eval@tautly.invalid commit -qm 'task go-config-module-path'
