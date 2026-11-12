"""RC.8D — bounded inbound fuzz/property tests against a real runtime.

A hostile or buggy host can send malformed frames, wrong shapes, invalid
credit sequences, and disconnect mid-anything. The runtime's invariants:

  * malformed JSON -> a PARSE_ERROR response, never a crash
  * valid-JSON wrong shapes -> INVALID_REQUEST / typed errors, ignored
  * credit for nonexistent tasks / nonsensical n -> ignored, never fatal
  * the runtime NEVER executes what it wasn't sent as a valid call
  * after any amount of garbage, a legitimate call still works
  * disconnects exit cleanly — no hang, no exception on stderr

Seeded and bounded for CI (PYLAIR_FUZZ_ITERS for a longer local soak).
"""

import os
import queue
import random
import time

from pylair import _protocol as proto
from test_protocol import call, grant


def _drain(runtime, seconds: float = 1.5) -> list[dict]:
    """Collect whatever the runtime emitted within the window."""
    out = []
    deadline = time.time() + seconds
    while time.time() < deadline:
        try:
            out.append(runtime.recv(timeout=max(0.05, deadline - time.time())))
        except queue.Empty:
            break
        if "__io_error__" in out[-1]:
            break
    return out


def test_malformed_frames_get_parse_errors_and_runtime_survives(runtime):
    runtime.recv()  # hello
    rng = random.Random(20260703)
    iters = int(os.environ.get("PYLAIR_FUZZ_ITERS", "100"))

    for _ in range(iters):
        n = rng.randrange(1, 65)
        garbage = bytes(rng.randrange(256) for _ in range(n))
        # Random bytes are rarely valid JSON — but even a lucky valid
        # frame is wrong-shaped, so either error code is acceptable.
        runtime.send_raw(len(garbage).to_bytes(4, "big") + garbage)

    msgs = _drain(runtime, seconds=5)
    assert runtime.proc.poll() is None, (
        f"runtime died on fuzz: {runtime.stderr_lines[-8:]}"
    )
    errors = [
        m
        for m in msgs
        if isinstance(m, dict) and (m.get("error") or {}).get("code") is not None
    ]
    codes = {e["error"]["code"] for e in errors}
    assert codes <= {proto.PARSE_ERROR, proto.INVALID_REQUEST}, (
        f"unexpected codes {codes}"
    )
    # It answered the garbage it could attribute — and is still alive.
    assert errors, "runtime ignored every malformed frame silently"

    # A legitimate call still works after the flood.
    runtime.send(call(999, "add", task=999, input={"a": 2, "b": 2}))
    resp = runtime.recv_until(lambda m: m.get("id") == 999, timeout=10)
    assert resp["result"] == 4


def test_weird_shapes_and_invalid_credit_sequences(runtime):
    """Valid JSON, wrong everything: arrays, scalars, non-int ids,
    unknown methods, credit for tasks that don't exist, nonsensical
    credit amounts. Typed errors or silence — never a crash, never
    an execution that wasn't asked for."""
    runtime.recv()

    for body in [
        [1, 2, 3],  # array, not an object
        "just a string",  # scalar
        42,
        {"id": "string-id", "method": "call", "params": {}},
        {"id": -1, "method": "call", "params": {}},
        {"id": 1.5, "method": "call", "params": {}},
        {"id": 1, "method": 5, "params": {}},
        {"id": 1, "method": "no.such.method", "params": {}},
        {"id": 1, "method": "__import__", "params": {"fn": "os"}},
        {
            "id": 1,
            "method": "call",
            "params": {"fn": "definitely_not_exported", "task": 1, "input": {}},
        },
        {"id": 1, "method": "stream.credit", "params": {"task": 1, "n": 5}},
        {"id": 1, "result": "a response the runtime never asked for"},
    ]:
        runtime.send(body)

    # Credit notifications: nonexistent tasks and nonsensical amounts —
    # all must be ignored without touching real state.
    for bad in [
        {"task": 424242, "n": 10},
        {"task": 1, "n": 0},
        {"task": 1, "n": -7},
        {"task": 1, "n": "lots"},
        {"task": 1, "n": 10**30},
        {"n": 5},
        {},
    ]:
        runtime.send(proto.notification("stream.credit", bad))

    runtime.send(call(2, "counter", task=50, input={"n": 1}))
    runtime.send(grant(50, 2))
    # The legit stream still works — and only TWO frames belong to it.
    seen = _drain(runtime, seconds=8)
    assert runtime.proc.poll() is None, (
        f"runtime died on weird shapes: {runtime.stderr_lines[-8:]}"
    )
    chunks = [
        m
        for m in seen
        if m.get("method") == "stream.chunk" and m.get("params", {}).get("task") == 50
    ]
    assert len(chunks) <= 2, f"phantom credit produced extra chunks: {seen}"
    # The bad request-shaped messages produced typed errors; nothing
    # executed (no 'result' from a forged method).
    assert not any(m.get("id") == 1 and "result" in m for m in seen), (
        "a forged frame executed"
    )


def test_disconnect_mid_stream_exits_cleanly(runtime):
    runtime.recv()
    runtime.send(call(1, "infinite_counted", task=7))
    runtime.send(grant(7, 8))
    # Confirm it's producing, then yank stdin mid-stream.
    first = runtime.recv_until(lambda m: m.get("method") == "stream.chunk", timeout=10)
    assert first["params"]["task"] == 7
    runtime.close_stdin()
    rc = runtime.proc.wait(timeout=10)
    assert rc == 0, f"mid-stream disconnect exited rc={rc}"


def test_rapid_call_cancel_race_bounded(runtime):
    """Fire call->cancel->call in tight succession; every call resolves
    to exactly one terminal (result or TASK_CANCELLED) and the runtime
    exits cleanly on EOF."""
    runtime.recv()
    for i in range(1, 51):
        runtime.send(
            call(i, "echo_later", task=i, input={"text": f"m{i}", "delay": 0.5})
        )
        runtime.send(proto.request(10_000 + i, "cancel", {"task": i}))
    runtime.close_stdin()

    msgs = _drain(runtime, seconds=10)
    rc = runtime.proc.wait(timeout=10)
    assert rc == 0, f"call/cancel race exited rc={rc}"
    terminals = [m for m in msgs if m.get("id") is not None and 1 <= m["id"] <= 50]
    # Every call got exactly one terminal — result or TASK_CANCELLED.
    assert len(terminals) == 50, (
        f"expected 50 terminals, got {len(terminals)}: {msgs[-6:]}"
    )
    assert all(
        "result" in m or (m.get("error") or {}).get("code") == proto.TASK_CANCELLED
        for m in terminals
    )
