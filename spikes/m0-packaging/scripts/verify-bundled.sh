#!/usr/bin/env bash
# Verify the *bundled* python payload runs with no system Python involved.
# Usage: verify-bundled.sh <dir-containing-python+site-packages+backend>
# The dir layout must match what lands in the installed app:
#   <dir>/python/{bin/python3|python.exe}
#   <dir>/site-packages/
#   <dir>/backend/app.py
set -euo pipefail

ROOT="$(cd "$1" && pwd)"
if [ -x "$ROOT/python/bin/python3" ]; then
  PY="$ROOT/python/bin/python3"
elif [ -f "$ROOT/python/python.exe" ]; then
  PY="$ROOT/python/python.exe"
else
  echo "no bundled interpreter found under $ROOT/python" >&2
  exit 1
fi

OUT="$(PYTHONPATH="$ROOT/site-packages" PYTHONNOUSERSITE=1 "$PY" "$ROOT/backend/app.py" --self-test)"
echo "$OUT"

# Assert the response really came from the bundled tree.
case "$OUT" in
  *"\"executable\": \"$ROOT"*) ;;
  *) echo "FAIL: executable path is not inside the bundle" >&2; exit 1 ;;
esac
case "$OUT" in
  *'"numpy":'*'"ok": true'*|*'"ok": true'*'"numpy":'*) ;;
  *) echo "FAIL: numpy did not import in the bundled env" >&2; exit 1 ;;
esac
echo "PASS: bundled python + numpy verified at $ROOT"
