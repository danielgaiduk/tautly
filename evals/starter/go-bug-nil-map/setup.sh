#!/usr/bin/env bash
# Makes the repository of task go-bug-nil-map in the current (empty) directory.
set -euo pipefail
cat > 'go.mod' <<'TAUTLY_EOF'
module example.com/task

go 1.21
TAUTLY_EOF
cat > 'counter.go' <<'TAUTLY_EOF'
package task

// Counter counts words. Its zero value is ready to use.
type Counter struct {
	counts map[string]int
}

// Add counts one more w.
func (c *Counter) Add(w string) {
	c.counts[w]++
}

// Count is how many times w was added.
func (c *Counter) Count(w string) int {
	return c.counts[w]
}
TAUTLY_EOF
cat > 'counter_test.go' <<'TAUTLY_EOF'
package task

import "testing"

func TestZeroCounter(t *testing.T) {
	var c Counter
	c.Add("a")
	c.Add("a")
	c.Add("b")
	if c.Count("a") != 2 || c.Count("b") != 1 || c.Count("z") != 0 {
		t.Fatalf("counts: a=%d b=%d z=%d", c.Count("a"), c.Count("b"), c.Count("z"))
	}
}
TAUTLY_EOF
git init -q
git add -A
git -c user.name=tautly-eval -c user.email=eval@tautly.invalid commit -qm 'task go-bug-nil-map'
