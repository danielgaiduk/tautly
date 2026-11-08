#!/usr/bin/env bash
# Makes the repository of task go-rename-func in the current (empty) directory.
set -euo pipefail
cat > 'go.mod' <<'TAUTLY_EOF'
module example.com/task

go 1.21
TAUTLY_EOF
cat > 'orders.go' <<'TAUTLY_EOF'
package task

// Order is a line of a purchase.
type Order struct {
	Qty   int
	Price int
}

// proc is the order's total in cents.
func proc(o Order) int {
	return o.Qty * o.Price
}
TAUTLY_EOF
cat > 'batch.go' <<'TAUTLY_EOF'
package task

// Total is the sum of the orders' totals.
func Total(orders []Order) int {
	sum := 0
	for _, o := range orders {
		sum += proc(o)
	}
	return sum
}
TAUTLY_EOF
cat > 'orders_test.go' <<'TAUTLY_EOF'
package task

import "testing"

func TestProcessOrder(t *testing.T) {
	if ProcessOrder(Order{Qty: 2, Price: 150}) != 300 {
		t.Fatal("ProcessOrder")
	}
	if Total([]Order{{1, 1}, {2, 3}}) != 7 {
		t.Fatal("Total")
	}
}
TAUTLY_EOF
git init -q
git add -A
git -c user.name=tautly-eval -c user.email=eval@tautly.invalid commit -qm 'task go-rename-func'
