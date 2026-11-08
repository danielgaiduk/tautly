#!/usr/bin/env bash
# Makes the repository of task py-rename-function in the current (empty) directory.
set -euo pipefail
cat > 'billing.py' <<'TAUTLY_EOF'
def calc(prices, tax=0.0):
    """The total of prices with tax (a fraction) added."""
    return round(sum(prices) * (1 + tax), 2)
TAUTLY_EOF
cat > 'invoice.py' <<'TAUTLY_EOF'
from billing import calc


def invoice_total(lines):
    return calc([price * qty for price, qty in lines], tax=0.2)
TAUTLY_EOF
cat > 'test_billing.py' <<'TAUTLY_EOF'
import unittest

from billing import calculate_total
from invoice import invoice_total


class Totals(unittest.TestCase):
    def test_total(self):
        self.assertEqual(calculate_total([1, 2]), 3)

    def test_invoice(self):
        self.assertEqual(invoice_total([(10, 2)]), 24.0)


if __name__ == "__main__":
    unittest.main()
TAUTLY_EOF
git init -q
git add -A
git -c user.name=tautly-eval -c user.email=eval@tautly.invalid commit -qm 'task py-rename-function'
