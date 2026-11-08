#!/usr/bin/env bash
# Makes the repository of task go-feat-contains in the current (empty) directory.
set -euo pipefail
cat > 'go.mod' <<'TAUTLY_EOF'
module example.com/task

go 1.21
TAUTLY_EOF
cat > 'strs.go' <<'TAUTLY_EOF'
package task

// Index is the position of s in xs, or -1.
func Index(xs []string, s string) int {
	for i, x := range xs {
		if x == s {
			return i
		}
	}
	return -1
}
TAUTLY_EOF
cat > 'strs_test.go' <<'TAUTLY_EOF'
package task

import "testing"

func TestContains(t *testing.T) {
	xs := []string{"a", "b"}
	if !Contains(xs, "b") || Contains(xs, "c") || Contains(nil, "a") {
		t.Fatal("Contains is wrong")
	}
}
TAUTLY_EOF
git init -q
git add -A
git -c user.name=tautly-eval -c user.email=eval@tautly.invalid commit -qm 'task go-feat-contains'
