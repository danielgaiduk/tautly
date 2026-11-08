#!/usr/bin/env bash
# Makes the repository of task go-bug-reverse in the current (empty) directory.
set -euo pipefail
cat > 'go.mod' <<'TAUTLY_EOF'
module example.com/task

go 1.21
TAUTLY_EOF
cat > 'reverse.go' <<'TAUTLY_EOF'
package task

// Reverse reverses xs in place.
func Reverse(xs []int) {
	for i := 0; i < len(xs)/2; i++ {
		j := len(xs) - i
		if j < len(xs) {
			xs[i], xs[j] = xs[j], xs[i]
		}
	}
}
TAUTLY_EOF
cat > 'reverse_test.go' <<'TAUTLY_EOF'
package task

import (
	"reflect"
	"testing"
)

func TestReverse(t *testing.T) {
	for _, c := range []struct{ in, want []int }{
		{[]int{1, 2, 3, 4}, []int{4, 3, 2, 1}},
		{[]int{1, 2, 3}, []int{3, 2, 1}},
		{[]int{}, []int{}},
	} {
		got := append([]int{}, c.in...)
		Reverse(got)
		if !reflect.DeepEqual(got, c.want) {
			t.Errorf("Reverse(%v) = %v, want %v", c.in, got, c.want)
		}
	}
}
TAUTLY_EOF
git init -q
git add -A
git -c user.name=tautly-eval -c user.email=eval@tautly.invalid commit -qm 'task go-bug-reverse'
