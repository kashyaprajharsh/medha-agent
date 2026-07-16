#!/bin/sh
# Asserts calc.sh adds correctly. Exits non-zero (fails) until the bug is fixed.
got=$(sh calc.sh 2 3)
if [ "$got" = "5" ]; then
  echo "PASS: 2 + 3 = $got"
  exit 0
else
  echo "FAIL: expected 2 + 3 = 5, got $got"
  exit 1
fi
