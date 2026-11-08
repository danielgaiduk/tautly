#!/usr/bin/env bash
# Makes the repository of task go-feat-stringer in the current (empty) directory.
set -euo pipefail
cat > 'go.mod' <<'TAUTLY_EOF'
module example.com/task

go 1.21
TAUTLY_EOF
cat > 'color.go' <<'TAUTLY_EOF'
package task

// Color is a primary color.
type Color int

const (
	Red Color = iota
	Green
	Blue
)
TAUTLY_EOF
cat > 'color_test.go' <<'TAUTLY_EOF'
package task

import (
	"fmt"
	"testing"
)

func TestString(t *testing.T) {
	got := fmt.Sprint(Red, " ", Green, " ", Blue, " ", Color(7))
	if got != "red green blue Color(7)" {
		t.Fatalf("got %q", got)
	}
}
TAUTLY_EOF
git init -q
git add -A
git -c user.name=tautly-eval -c user.email=eval@tautly.invalid commit -qm 'task go-feat-stringer'
