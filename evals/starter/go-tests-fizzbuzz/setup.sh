#!/usr/bin/env bash
# Makes the repository of task go-tests-fizzbuzz in the current (empty) directory.
set -euo pipefail
cat > 'go.mod' <<'TAUTLY_EOF'
module example.com/task

go 1.21
TAUTLY_EOF
cat > 'fizzbuzz.go' <<'TAUTLY_EOF'
package task

import "strconv"

// FizzBuzz is "Fizz" for multiples of 3, "Buzz" for 5, "FizzBuzz" for both, else n.
func FizzBuzz(n int) string {
	switch {
	case n%15 == 0:
		return "FizzBuzz"
	case n%3 == 0:
		return "Fizz"
	case n%5 == 0:
		return "Buzz"
	}
	return strconv.Itoa(n)
}
TAUTLY_EOF
git init -q
git add -A
git -c user.name=tautly-eval -c user.email=eval@tautly.invalid commit -qm 'task go-tests-fizzbuzz'
