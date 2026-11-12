"""Isolated-worker entry point: `python -m pylair._worker <module> <fn>`.

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
import os
import signal
import sys
import threading
import time
import traceback

from pylair import _json
from pylair import _protocol as proto


def _exit_with_parent() -> None:
    """Reap this worker's whole process group if the runtime disappears.

    Workers run in their own session (so a single task can be cancelled by
    killing exactly its group), which means a hard kill of the runtime's own
    process group cannot reach them. When the parent is gone the worker is
    re-parented; SIGKILL the group (worker + any grandchildren, SIGTERM
    ignorers included) rather than run on unsupervised. On Windows the host's
    Job Object already contains workers.
    """
    if sys.platform == "win32":
        return
    parent = os.getppid()

    # Linux: kernel-level backstop for the watcher below — PR_SET_PDEATHSIG
    # fires even if a native call is holding the GIL (which would stall the
    # polling thread). If the runtime died before prctl ran, getppid already
    # changed: kill ourselves now — the race the ppid check exists for.
    if sys.platform.startswith("linux"):
        try:
            import ctypes

            ctypes.CDLL("libc.so.6", use_errno=True).prctl(1, signal.SIGKILL)
            if os.getppid() != parent:
                os.kill(os.getpid(), signal.SIGKILL)
        except (ImportError, OSError) as e:
            # No ctypes or a libc without prctl — the ppid watcher below
            # remains the fallback, so a setup failure is not fatal.
            print(f"[pylair] worker: pdeathsig setup failed: {e!r}", file=sys.stderr)

    def watch() -> None:
        while True:
            time.sleep(0.5)
            if os.getppid() != parent:
                try:
                    os.killpg(0, signal.SIGKILL)
                finally:
                    os._exit(1)

    threading.Thread(target=watch, daemon=True, name="pylair-parent-watch").start()


def main() -> int:
    _exit_with_parent()
    if len(sys.argv) != 3:
        print("usage: python -m pylair._worker <module> <fn>", file=sys.stderr)
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
        input = _json.loads(raw) if raw else {}
        result = fn(**_json.coerce_kwargs(fn, input))
        payload = {"result": result}
    except _json.InvalidParams as e:
        # Caller sent values the signature can't hold — a params error,
        # not a worker failure; the parent maps `code` through.
        payload = {
            "error": {
                "code": proto.INVALID_PARAMS,
                "message": str(e),
                "data": {"type": "InvalidParams"},
            }
        }
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

    os.write(out_fd, _json.dumps(payload).encode())
    return 0


if __name__ == "__main__":
    sys.exit(main())
