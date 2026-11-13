"""RC.8A: bounded-output and limit-error tests.

The e2e test at the bottom drives a real spawned runtime: a host that
grants credit and then never drains stdout is functionally gone — the
runtime must not accumulate frames without bound; it logs a distinct
reason and exits.
"""

from __future__ import annotations

import asyncio
import os
import queue
import subprocess
import sys
import threading
import time

import pytest

from conftest import _LEN, HERE
from pylair import _protocol as proto
from pylair._server import _STOP, Server, _BoundedOut
from pylair._transport import MAX_FRAME


class _FakeChannel:
    def __init__(self) -> None:
        self.written: list[bytes] = []

    def read_frame(self):
        return None

    def write_frame(self, body: bytes) -> None:
        self.written.append(body)

    def close_write(self) -> None:
        pass


# ---- _BoundedOut ------------------------------------------------------------


def test_bounded_out_frame_cap_raises_full():
    q = _BoundedOut(max_frames=2, max_bytes=1 << 30)
    q.put_nowait(b"a")
    q.put_nowait(b"b")
    with pytest.raises(queue.Full):
        q.put_nowait(b"c")


def test_bounded_out_byte_cap_raises_full():
    q = _BoundedOut(max_frames=1000, max_bytes=8)
    q.put_nowait(b"12345")
    with pytest.raises(queue.Full):
        q.put_nowait(b"4567")  # 5+4 > 8


def test_bounded_out_close_always_delivers_stop():
    q = _BoundedOut(max_frames=1, max_bytes=1 << 30)
    q.put_nowait(b"x")
    q.close()  # sentinel bypasses the cap — shutdown must be deliverable
    assert q.get() == b"x"
    assert q.get() is _STOP


# ---- Server._send ------------------------------------------------------------


def _server_with_loop() -> tuple[Server, asyncio.AbstractEventLoop, threading.Thread]:
    loop = asyncio.new_event_loop()
    t = threading.Thread(target=loop.run_forever, daemon=True)
    t.start()
    s = Server(_FakeChannel())
    s._loop = loop
    return s, loop, t


def test_send_full_marks_out_full_and_injects_eof(capsys):
    s, loop, _t = _server_with_loop()
    s._out = _BoundedOut(max_frames=1, max_bytes=1 << 30)
    s._send({"jsonrpc": "2.0", "method": "pong", "params": {"seq": 1}})
    # Second frame does not fit: _send must NOT block — it flags the
    # distinct reason and pushes the host-gone sentinel to the loop.
    s._send({"jsonrpc": "2.0", "method": "pong", "params": {"seq": 2}})
    assert s._out_full is True

    async def get():
        return await asyncio.wait_for(s._in.get(), 5)

    fut = asyncio.run_coroutine_threadsafe(get(), loop)
    assert fut.result(timeout=5).__class__.__name__ == "object"  # _EOF sentinel
    loop.call_soon_threadsafe(loop.stop)
    err = capsys.readouterr().err
    assert "outbound queue full" in err
    assert "host not draining" in err


def test_oversized_response_becomes_limit_error():
    s, loop, _t = _server_with_loop()
    s._send({"jsonrpc": "2.0", "id": 7, "result": "x" * (MAX_FRAME + 1)})
    body = s._out.get()
    import json as _j

    msg = _j.loads(body)
    assert msg["id"] == 7
    assert msg["error"]["code"] == proto.LIMIT_EXCEEDED
    assert msg["error"]["data"]["limit"] == "max_frame"
    loop.call_soon_threadsafe(loop.stop)


def test_oversized_notification_is_dropped_to_out():
    # Notifications have no caller to report to — the oversized frame is
    # still bounded by the byte cap (this is the documented asymmetry: a
    # chunk stream relies on the host-side caps to bound sizes).
    s, loop, _t = _server_with_loop()
    big = {
        "jsonrpc": "2.0",
        "method": "stream.chunk",
        "params": {"task": 1, "seq": 0, "value": "v"},
    }
    s._send(big)
    body = s._out.get()
    assert b"stream.chunk" in body
    loop.call_soon_threadsafe(loop.stop)


# ---- e2e: stalled host -------------------------------------------------------


def test_stalled_host_exits_with_distinct_reason():
    """Host grants credit then never drains stdout: the runtime's outbound
    queue fills, it logs the distinct reason and exits (never blocks the
    event loop)."""
    env = dict(os.environ)
    env.pop("PYTHONPATH", None)
    # Tiny cap so the flood fills it quickly; the byte cap stays large.
    env["PYLAIR_OUT_MAX_FRAMES"] = "32"
    env["PYLAIR_OUT_MAX_BYTES"] = str(1 << 30)
    proc = subprocess.Popen(
        [sys.executable, "-m", "pylair", "testapp"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        cwd=HERE,
        env=env,
    )
    try:
        stderr_lines: list[str] = []
        threading.Thread(
            target=lambda: [
                stderr_lines.append(l.decode(errors="replace")) for l in proc.stderr
            ],
            daemon=True,
        ).start()

        # hello frame, then a stream call + generous credit — stdout is
        # intentionally never read after this point.
        hdr = proc.stdout.read(4)
        (n,) = _LEN.unpack(hdr)
        proc.stdout.read(n)
        call = proto.request(
            1, "call", {"fn": "infinite_counted", "task": 1, "input": {}}
        )
        for msg in (
            call,
            proto.notification("stream.credit", {"task": 1, "n": 4096}),
        ):
            body = __import__("json").dumps(msg).encode()
            proc.stdin.write(_LEN.pack(len(body)) + body)
            proc.stdin.flush()

        deadline = time.time() + 30
        while proc.poll() is None and time.time() < deadline:
            time.sleep(0.1)
        assert proc.poll() is not None, "runtime did not exit on stalled host"
        joined = "".join(stderr_lines)
        assert "outbound queue full" in joined, joined[-2000:]
    finally:
        if proc.poll() is None:
            proc.kill()


# ---- depth cap (_send) --------------------------------------------------------


def _nest(n: int):
    out = 1
    for _ in range(n):
        out = {"d": out}
    return out


def test_deep_response_becomes_limit_error():
    s, loop, _t = _server_with_loop()
    s._send({"jsonrpc": "2.0", "id": 7, "result": _nest(200)})
    body = s._out.get()
    import json as _j

    msg = _j.loads(body)
    assert msg["id"] == 7
    assert msg["error"]["code"] == proto.LIMIT_EXCEEDED
    assert msg["error"]["data"]["limit"] == "max_depth"
    loop.call_soon_threadsafe(loop.stop)


def test_deep_notification_is_dropped_not_queued(capsys):
    # A notification has no caller to report to: dropping it (with a log)
    # beats writing a frame the host cannot parse.
    s, loop, _t = _server_with_loop()
    s._send(
        {
            "jsonrpc": "2.0",
            "method": "stream.chunk",
            "params": {"task": 1, "seq": 0, "value": _nest(200)},
        }
    )
    assert s._out.stats()[0] == 0
    loop.call_soon_threadsafe(loop.stop)
    assert "depth" in capsys.readouterr().err
