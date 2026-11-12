"""End-to-end protocol tests against a real spawned runtime.

Each test gets a fresh runtime via the `runtime` fixture (conftest.py).
Frames are asserted at the wire level — including the stdout-hygiene and
credit-flow guarantees.
"""

import os
import queue
import signal
import struct
import subprocess
import sys
import time

import pytest

from pylair import _protocol as proto

_LEN = struct.Struct(">I")


def call(rid: int, fn: str, task: int, input: dict | None = None) -> dict:
    return proto.request(rid, "call", {"fn": fn, "task": task, "input": input or {}})


def grant(task: int, n: int = proto.INITIAL_STREAM_CREDIT) -> dict:
    """The host's initial window — it owns the grant (protocol §3.1)."""
    return proto.notification("stream.credit", {"task": task, "n": n})


# ---- handshake ------------------------------------------------------------


def test_hello_is_first_frame_and_lists_exposes(runtime):
    hello = runtime.recv()
    assert hello["method"] == "hello"
    p = hello["params"]
    assert p["protocol"] == proto.PROTOCOL_VERSION
    names = {e["name"]: e["kind"] for e in p["exposes"]}
    assert names["add"] == "sync"
    assert names["echo_later"] == "async"
    assert names["counter"] == "generator"
    assert names["isolated_add"] == "isolated"


# ---- calls ----------------------------------------------------------------


def test_sync_call(runtime):
    runtime.recv()  # hello
    runtime.send(call(1, "add", task=1, input={"a": 2, "b": 3}))
    resp = runtime.recv()
    assert resp == {"jsonrpc": "2.0", "id": 1, "result": 5}


def test_async_call(runtime):
    runtime.recv()
    runtime.send(call(1, "echo_later", task=1, input={"text": "hi"}))
    assert runtime.recv()["result"] == "hi"


def test_unknown_function(runtime):
    runtime.recv()
    runtime.send(call(1, "nope", task=1))
    resp = runtime.recv()
    assert resp["error"]["code"] == proto.FUNCTION_NOT_FOUND


def test_malformed_frame(runtime):
    runtime.recv()
    body = b"this is not json"
    runtime.send_raw(_LEN.pack(len(body)) + body)
    resp = runtime.recv()
    assert resp["error"]["code"] == proto.PARSE_ERROR


def test_print_and_fd1_writes_cannot_corrupt_frames(runtime):
    """Stray stdout writes land on stderr; the next frame still parses."""
    runtime.recv()
    runtime.send(call(1, "print_then_answer", task=1, input={"text": "ok"}))
    resp = runtime.recv()
    assert resp["result"] == "ok"
    # and the stream is still clean for the NEXT call
    runtime.send(call(2, "add", task=2, input={"a": 1, "b": 1}))
    assert runtime.recv()["result"] == 2


def test_ping_pong(runtime):
    runtime.recv()
    runtime.send(proto.request(7, "ping", {"seq": 42}))
    msg = runtime.recv()
    assert msg["method"] == "pong" and msg["params"]["seq"] == 42
    # additive field (protocol §5): how long the asyncio loop has been stuck
    assert isinstance(msg["params"]["loop_stale_ms"], int)


# ---- streaming + credit ----------------------------------------------------


def test_stream_terminal_is_the_call_response(runtime):
    runtime.recv()
    runtime.send(call(9, "counter", task=5, input={"n": 3}))
    runtime.send(grant(5))
    chunks = [
        runtime.recv_until(lambda m: m.get("method") == "stream.chunk")
        for _ in range(3)
    ]
    assert [c["params"]["value"] for c in chunks] == [0, 1, 2]
    assert [c["params"]["seq"] for c in chunks] == [0, 1, 2]
    terminal = runtime.recv_until(lambda m: m.get("id") == 9)
    assert terminal["result"] == {"done": True}


def test_producer_runs_only_with_credit(runtime):
    """An infinite generator must pause at the initial credit boundary."""
    runtime.recv()
    runtime.send(call(1, "infinite", task=1))
    runtime.send(grant(1))
    got = 0
    # the host granted INITIAL_STREAM_CREDIT; drain exactly that many
    for _ in range(proto.INITIAL_STREAM_CREDIT):
        runtime.recv_until(lambda m: m.get("method") == "stream.chunk", timeout=5)
        got += 1
    # now no more chunks should arrive without credit
    with pytest.raises(queue.Empty):
        runtime.recv(timeout=1.5)
    # grant 5 more -> exactly 5 more chunks, then silence again
    runtime.send(proto.notification("stream.credit", {"task": 1, "n": 5}))
    for _ in range(5):
        runtime.recv_until(lambda m: m.get("method") == "stream.chunk", timeout=5)
        got += 1
    with pytest.raises(queue.Empty):
        runtime.recv(timeout=1.5)
    assert got == proto.INITIAL_STREAM_CREDIT + 5


# ---- cancellation -----------------------------------------------------------


def test_cancel_async_task(runtime):
    runtime.recv()
    runtime.send(call(1, "sleep_forever", task=1))
    runtime.send(proto.request(2, "cancel", {"task": 1}))
    # cancel ack may interleave with the task's terminal response
    ack = runtime.recv_until(lambda m: m.get("id") == 2)
    assert ack["result"]["cancelled"] is True
    terminal = runtime.recv_until(lambda m: m.get("id") == 1)
    assert terminal["error"]["code"] == proto.TASK_CANCELLED


def test_cancel_unknown_task(runtime):
    runtime.recv()
    runtime.send(proto.request(1, "cancel", {"task": 999}))
    resp = runtime.recv()
    assert resp["result"]["cancelled"] is False


def test_cancel_stream(runtime):
    runtime.recv()
    runtime.send(call(7, "infinite", task=7))
    runtime.send(grant(7))
    for _ in range(5):
        runtime.recv_until(lambda m: m.get("method") == "stream.chunk")
    runtime.send(proto.request(8, "cancel", {"task": 7}))
    terminal = runtime.recv_until(lambda m: m.get("id") == 7)
    assert terminal["error"]["code"] == proto.TASK_CANCELLED


def test_isolated_call(runtime):
    runtime.recv()
    runtime.send(call(1, "isolated_add", task=1, input={"a": 4, "b": 5}))
    assert runtime.recv()["result"] == 9


def test_isolated_print_does_not_corrupt_result(runtime):
    runtime.recv()
    runtime.send(call(1, "isolated_print", task=1, input={"text": "hi"}))
    resp = runtime.recv()
    assert resp["result"] == "hi", resp


def test_isolated_call_and_cancel_kills_worker(runtime):
    runtime.recv()
    runtime.send(call(1, "isolated_sleep", task=1, input={"seconds": 60}))
    time.sleep(1.0)  # let the worker spawn
    t0 = time.time()
    runtime.send(proto.request(2, "cancel", {"task": 1}))
    # The ack now reports the actual kill outcome, so it waits for the
    # terminate/reap path — the task's own terminal can arrive first.
    # Collect both regardless of order.
    ack = terminal = None
    deadline = time.time() + 15
    while ack is None or terminal is None:
        m = runtime.recv(timeout=max(0.1, deadline - time.time()))
        if m.get("id") == 2:
            ack = m
        elif m.get("id") == 1:
            terminal = m
    elapsed = time.time() - t0
    assert ack["result"]["cancelled"] is True
    assert ack["result"]["status"] == "terminated", ack
    assert terminal["error"]["code"] == proto.TASK_CANCELLED
    assert elapsed < 10, "worker should be terminated promptly, not at 60s"


@pytest.mark.skipif(
    sys.platform == "win32", reason="process-group kill semantics are unix-specific"
)
def test_cancel_isolated_kills_sigterm_immune_grandchild(runtime, tmp_path):
    """A grandchild ignoring SIGTERM must still die: group SIGKILL."""
    runtime.recv()
    pidfile = tmp_path / "grandchild.pid"
    runtime.send(call(1, "orphan_maker", task=1, input={"pidfile": str(pidfile)}))
    deadline = time.time() + 15
    while not pidfile.exists():
        assert time.time() < deadline, "grandchild never spawned"
        time.sleep(0.1)
    gpid = int(pidfile.read_text().strip())
    os.kill(gpid, 0)  # alive

    runtime.send(proto.request(2, "cancel", {"task": 1}))
    terminal = runtime.recv_until(lambda m: m.get("id") == 1)
    assert terminal["error"]["code"] == proto.TASK_CANCELLED

    deadline = time.time() + 5
    while time.time() < deadline:
        try:
            os.kill(gpid, 0)
        except ProcessLookupError:
            break
        time.sleep(0.1)
    else:
        pytest.fail(f"grandchild {gpid} survived the group kill")


@pytest.mark.skipif(sys.platform == "win32", reason="posix process groups")
def test_runtime_killed_hard_reaps_isolated_worker_and_grandchildren(runtime, tmp_path):
    """Workers live in their own session, outside the runtime's process
    group, so a forced kill of the runtime (what the host does) cannot reach
    them. The worker must reap its own group when its parent disappears."""
    runtime.recv()
    pidfile = tmp_path / "grandchild.pid"
    runtime.send(call(1, "orphan_maker", task=1, input={"pidfile": str(pidfile)}))
    deadline = time.time() + 15
    while not pidfile.exists():
        assert time.time() < deadline, "grandchild never spawned"
        time.sleep(0.1)
    gpid = int(pidfile.read_text().strip())
    os.kill(gpid, 0)  # alive, and it ignores SIGTERM

    runtime.proc.kill()  # SIGKILL the runtime only — no cleanup runs
    runtime.proc.wait(timeout=10)

    deadline = time.time() + 10
    while time.time() < deadline:
        try:
            os.kill(gpid, 0)
        except ProcessLookupError:
            return
        time.sleep(0.2)
    os.kill(gpid, signal.SIGKILL)  # don't leak it from the test itself
    pytest.fail(f"grandchild {gpid} outlived a hard kill of the runtime")


# ---- lifecycle --------------------------------------------------------------


def test_stdin_eof_exits_cleanly(runtime):
    runtime.recv()
    runtime.close_stdin()
    rc = runtime.proc.wait(timeout=10)
    assert rc == 0


def test_oversized_frame_rejected():
    """A >MAX_FRAME length prefix must not allocate/read that much."""
    from pathlib import Path

    proc = subprocess.Popen(
        [sys.executable, "-m", "pylair", "testapp"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        cwd=Path(__file__).parent,
    )
    (hello_len,) = struct.unpack(">I", proc.stdout.read(4))
    proc.stdout.read(hello_len)
    proc.stdin.write(_LEN.pack(proto.MAX_STREAM_BUFFER * 8))  # huge len, no body
    proc.stdin.flush()
    proc.stdin.close()
    # runtime should exit (violation -> EOF path), not hang waiting for 64 MiB
    rc = proc.wait(timeout=10)
    assert rc is not None
