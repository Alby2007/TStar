"""Isolated-worker entry point: `python -m tauri_plus._worker <module> <fn>`.

Internal protocol (not the wire protocol): reads the call input as one JSON
document on stdin, runs the named @expose'd function from the given user
module, writes {"result": ...} or {"error": ...} as one JSON document on a
PRIVATE dup of stdout, exits 0. Non-zero exit = WORKER_DIED to the parent.

The result channel gets the same stdout hygiene as the runtime's frame
channel (protocol §1): fd 1 is redirected to stderr before user code runs,
so a `print()` inside an isolated function can never corrupt the result.
"""

from __future__ import annotations

import importlib
import json
import os
import sys
import traceback


def main() -> int:
    if len(sys.argv) != 3:
        print("usage: python -m tauri_plus._worker <module> <fn>", file=sys.stderr)
        return 2
    module_name, fn_name = sys.argv[1], sys.argv[2]

    if sys.platform == "win32":
        import msvcrt

        msvcrt.setmode(0, os.O_BINARY)

    # Private result channel: dup the real stdout, then send fd 1 to stderr.
    out_fd = os.dup(1)
    os.dup2(2, 1)
    sys.stdout = sys.stderr

    try:
        module = importlib.import_module(module_name)
        fn = getattr(module, fn_name)
        raw = sys.stdin.buffer.read()
        input = json.loads(raw) if raw else {}
        result = fn(**input)
        payload = {"result": result}
    except BaseException as e:  # noqa: BLE001 - boundary: report everything
        payload = {
            "error": {
                "message": f"{type(e).__name__}: {e}",
                "data": {
                    "type": type(e).__name__,
                    "traceback": traceback.format_exc(limit=20),
                },
            }
        }

    os.write(out_fd, json.dumps(payload).encode())
    return 0


if __name__ == "__main__":
    sys.exit(main())
