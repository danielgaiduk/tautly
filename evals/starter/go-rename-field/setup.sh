#!/usr/bin/env bash
# Makes the repository of task go-rename-field in the current (empty) directory.
set -euo pipefail
cat > 'go.mod' <<'TAUTLY_EOF'
module example.com/task

go 1.21
TAUTLY_EOF
cat > 'item.go' <<'TAUTLY_EOF'
package task

// Item is a thing for sale.
type Item struct {
	Nm    string
	Price int
}
TAUTLY_EOF
cat > 'catalog.go' <<'TAUTLY_EOF'
package task

import "sort"

// Names lists the items' names, sorted.
func Names(items []Item) []string {
	out := make([]string, 0, len(items))
	for _, it := range items {
		out = append(out, it.Nm)
	}
	sort.Strings(out)
	return out
}
TAUTLY_EOF
cat > 'item_test.go' <<'TAUTLY_EOF'
package task

import (
	"reflect"
	"testing"
)

func TestNames(t *testing.T) {
	got := Names([]Item{{Name: "b", Price: 1}, {Name: "a", Price: 2}})
	if !reflect.DeepEqual(got, []string{"a", "b"}) {
		t.Fatalf("got %v", got)
	}
}
TAUTLY_EOF
git init -q
git add -A
git -c user.name=tautly-eval -c user.email=eval@tautly.invalid commit -qm 'task go-rename-field'
