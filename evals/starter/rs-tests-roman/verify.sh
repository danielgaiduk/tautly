#!/usr/bin/env bash
# Exits 0 when task rs-tests-roman is solved (run at the root of its repository).
set -euo pipefail
test -f tests/roman.rs || { echo 'tests/roman.rs is missing'; exit 1; }
cargo test --quiet --offline
# The new tests must fail against broken implementation 1.
dir=$(mktemp -d)
cp -R . "$dir/"
cat > "$dir/src/lib.rs" <<'TAUTLY_EOF'
pub fn to_roman(mut n: u32) -> String {
    const TABLE: [(u32, &str); 7] = [(1000, "M"), (500, "D"), (100, "C"), (50, "L"), (10, "X"), (5, "V"), (1, "I")];
    let mut out = String::new();
    for (value, digits) in TABLE {
        while n >= value {
            out.push_str(digits);
            n -= value;
        }
    }
    out
}
TAUTLY_EOF
if (cd "$dir" && cargo test --quiet --offline) >/dev/null 2>&1; then
  rm -rf "$dir"
  echo 'the tests pass against a broken src/lib.rs'
  exit 1
fi
rm -rf "$dir"
# The new tests must fail against broken implementation 2.
dir=$(mktemp -d)
cp -R . "$dir/"
cat > "$dir/src/lib.rs" <<'TAUTLY_EOF'
pub fn to_roman(mut n: u32) -> String {
    const TABLE: [(u32, &str); 13] = [(1000, "M"), (900, "CM"), (500, "D"), (400, "CD"), (100, "C"), (90, "XC"), (50, "L"), (40, "XL"), (10, "X"), (9, "IX"), (5, "V"), (4, "IV"), (1, "I")];
    let mut out = String::new();
    for (value, digits) in TABLE {
        if n >= value {
            out.push_str(digits);
            n -= value;
        }
    }
    out
}
TAUTLY_EOF
if (cd "$dir" && cargo test --quiet --offline) >/dev/null 2>&1; then
  rm -rf "$dir"
  echo 'the tests pass against a broken src/lib.rs'
  exit 1
fi
rm -rf "$dir"
