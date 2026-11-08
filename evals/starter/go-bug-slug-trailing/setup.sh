#!/usr/bin/env bash
# Makes the repository of task go-bug-slug-trailing in the current (empty) directory.
set -euo pipefail
cat > 'go.mod' <<'TAUTLY_EOF'
module example.com/task

go 1.21
TAUTLY_EOF
cat > 'slug.go' <<'TAUTLY_EOF'
package task

import "strings"

// Slug lower-cases s and joins its words with dashes.
func Slug(s string) string {
	return strings.ReplaceAll(strings.ToLower(s), " ", "-")
}
TAUTLY_EOF
cat > 'slug_test.go' <<'TAUTLY_EOF'
package task

import "testing"

func TestSlug(t *testing.T) {
	for in, want := range map[string]string{
		"Hello World ":   "hello-world",
		"  many   spaces": "many-spaces",
		"one":            "one",
	} {
		if got := Slug(in); got != want {
			t.Errorf("Slug(%q) = %q, want %q", in, got, want)
		}
	}
}
TAUTLY_EOF
git init -q
git add -A
git -c user.name=tautly-eval -c user.email=eval@tautly.invalid commit -qm 'task go-bug-slug-trailing'
