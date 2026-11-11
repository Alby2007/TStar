"""M0 spike backend.

Protocol (throwaway, NOT the M1 design): length-prefixed frames on
stdin/stdout. 4-byte big-endian length + a JSON body shaped like JSON-RPC:
{"jsonrpc": "2.0", "id": N, "method": "ping", "params": {...}}

IMPORTANT: nothing may write to stdout except response frames — prints
corrupt the stream. Diagnostics go to stderr.
"""

import json
import struct
import sys


def read_frame() -> bytes | None:
    hdr = sys.stdin.buffer.read(4)
    if len(hdr) < 4:
        return None
    (n,) = struct.unpack(">I", hdr)
    body = sys.stdin.buffer.read(n)
    return body if len(body) == n else None


def write_frame(obj: dict) -> None:
    body = json.dumps(obj).encode()
    sys.stdout.buffer.write(struct.pack(">I", len(body)) + body)
    sys.stdout.buffer.flush()


def handle_ping(params: dict) -> dict:
    import numpy

    arr = numpy.arange(5).sum()  # force the native extension to load
    return {
        "python": sys.version.split()[0],
        "executable": sys.executable,
        "prefix": sys.prefix,
        "numpy": numpy.__version__,
        "numpy_sum": int(arr),
        "echo": params,
    }


def self_test() -> int:
    """Run the ping handler once and print the result.

    Used by clean-machine verification: `<bundled-python> app.py --self-test`
    proves the interpreter + site-packages work with zero framing involved.
    """
    result = handle_ping({"message": "self-test"})
    print(json.dumps({"ok": True, "result": result}))
    return 0


def main() -> None:
    if "--self-test" in sys.argv:
        sys.exit(self_test())

    while True:
        frame = read_frame()
        if frame is None:
            return
        try:
            req = json.loads(frame)
        except json.JSONDecodeError as e:
            write_frame({"jsonrpc": "2.0", "id": None, "error": {"code": -32700, "message": str(e)}})
            continue

        resp = {"jsonrpc": "2.0", "id": req.get("id")}
        try:
            if req.get("method") != "ping":
                raise ValueError(f"unknown method: {req.get('method')}")
            resp["result"] = handle_ping(req.get("params") or {})
        except Exception as e:  # noqa: BLE001 - boundary: report all errors as protocol errors
            resp["error"] = {"code": -32000, "message": f"{type(e).__name__}: {e}"}
        write_frame(resp)


if __name__ == "__main__":
    main()
