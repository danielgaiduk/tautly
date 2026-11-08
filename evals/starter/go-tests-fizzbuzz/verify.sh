#!/usr/bin/env bash
# Exits 0 when task go-tests-fizzbuzz is solved (run at the root of its repository).
set -euo pipefail
test -f fizzbuzz_test.go || { echo 'fizzbuzz_test.go is missing'; exit 1; }
go test ./...
# The new tests must fail against broken implementation 1.
dir=$(mktemp -d)
cp -R . "$dir/"
cat > "$dir/fizzbuzz.go" <<'TAUTLY_EOF'
package task

import "strconv"

func FizzBuzz(n int) string {
	switch {
	case n%3 == 0:
		return "Fizz"
	case n%5 == 0:
		return "Buzz"
	}
	return strconv.Itoa(n)
}
TAUTLY_EOF
if (cd "$dir" && go test ./...) >/dev/null 2>&1; then
  rm -rf "$dir"
  echo 'the tests pass against a broken fizzbuzz.go'
  exit 1
fi
rm -rf "$dir"
# The new tests must fail against broken implementation 2.
dir=$(mktemp -d)
cp -R . "$dir/"
cat > "$dir/fizzbuzz.go" <<'TAUTLY_EOF'
package task

import "strconv"

func FizzBuzz(n int) string {
	switch {
	case n%15 == 0:
		return "FizzBuzz"
	case n%3 == 0:
		return "Fizz"
	}
	return strconv.Itoa(n)
}
TAUTLY_EOF
if (cd "$dir" && go test ./...) >/dev/null 2>&1; then
  rm -rf "$dir"
  echo 'the tests pass against a broken fizzbuzz.go'
  exit 1
fi
rm -rf "$dir"
