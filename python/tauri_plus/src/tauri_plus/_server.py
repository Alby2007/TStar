"""The runtime frame loop (protocol v1).

Threads: a reader thread blocks on stdin and pushes parsed messages to the
event loop; a writer thread serializes outbound frames so the loop never
blocks on a full pipe. stdin EOF = host gone -> cancel everything and
exit (protocol §6).
"""

from __future__ import annotations

import asyncio
import json
import queue
import sys
import threading
import traceback
from typing import Any

from tauri_plus import _protocol as proto
from tauri_plus._expose import exposed_functions, exposes_manifest
from tauri_plus._tasks import TaskRegistry
from tauri_plus._transport import FrameChannel, FrameTooLarge

_EOF = object()
_STOP = object()


def _log(msg: str) -> None:
    print(f"[tauri_plus] {msg}", file=sys.stderr, flush=True)


class Server:
    def __init__(self, channel: FrameChannel) -> None:
        self._channel = channel
        self._out: queue.Queue = queue.Queue()
        self._in: asyncio.Queue = asyncio.Queue()
        self._tasks = TaskRegistry(self._send)
        self._stopping = False

    def _send(self, msg: dict) -> None:
        self._out.put(msg)

    # ---- io threads --------------------------------------------------------

    def _writer(self) -> None:
        while True:
            msg = self._out.get()
            if msg is _STOP:
                return
            try:
                self._channel.write_frame(msg)
            except OSError:
                return  # frame fd dead — host is gone

    def _reader(self, loop: asyncio.AbstractEventLoop) -> None:
        try:
            while True:
                body = self._channel.read_frame()
                if body is None:
                    loop.call_soon_threadsafe(self._in.put_nowait, _EOF)
                    return
                try:
                    msg = json.loads(body)
                except json.JSONDecodeError:
                    msg = {
                        "__parse_error__": True,
                        "raw": body[:200].decode(errors="replace"),
                    }
                loop.call_soon_threadsafe(self._in.put_nowait, msg)
        except FrameTooLarge as e:
            _log(f"protocol violation: {e} — treating host as gone")
            loop.call_soon_threadsafe(self._in.put_nowait, _EOF)
        except OSError:
            loop.call_soon_threadsafe(self._in.put_nowait, _EOF)

    # ---- dispatch ----------------------------------------------------------

    async def _handle(self, msg: dict) -> None:
        if msg.get("__parse_error__"):
            _log(f"unparseable frame: {msg['raw']!r}")
            self._send(
                proto.error_response(None, proto.PARSE_ERROR, "malformed JSON frame")
            )
            return

        if proto.is_notification(msg):
            method, params = msg["method"], msg.get("params") or {}
            if method == "stream.credit":
                task_id = params.get("task")
                n = params.get("n", 0)
                if isinstance(task_id, (int, str)) and isinstance(n, int) and n > 0:
                    self._tasks.grant_credit(task_id, n)
            else:
                _log(f"unknown notification: {method}")
            return

        if proto.is_response(msg):
            _log("unexpected response from host (runtime sends no requests)")
            return

        if not proto.is_request(msg):
            _log(f"invalid message shape: {msg!r}")
            self._send(
                proto.error_response(
                    msg.get("id"), proto.INVALID_REQUEST, "invalid request"
                )
            )
            return

        rid, method, params = msg["id"], msg["method"], msg.get("params") or {}
        try:
            if method == "ping":
                self._send(proto.notification("pong", {"seq": params.get("seq")}))
            elif method == "call":
                await self._call(rid, params)
            elif method == "cancel":
                result = await self._tasks.cancel(params.get("task"))
                self._send(proto.response(rid, result))
            elif method == "shutdown":
                self._send(proto.response(rid, {}))
                self._stopping = True
                grace = params.get("grace_ms", 5000) / 1000
                await self._drain(grace)
            elif method == "stream.credit":  # tolerate request-shaped credit
                self._send(
                    proto.error_response(
                        rid, proto.INVALID_REQUEST, "stream.credit is a notification"
                    )
                )
            else:
                self._send(
                    proto.error_response(
                        rid, proto.FUNCTION_NOT_FOUND, f"unknown method {method!r}"
                    )
                )
        except Exception as e:  # noqa: BLE001 - dispatch must not die
            _log(f"dispatch error for {method}: {e}\n{traceback.format_exc(limit=10)}")
            self._send(
                proto.error_response(
                    rid, proto.INTERNAL, str(e), {"type": type(e).__name__}
                )
            )

    async def _call(self, rid: Any, params: dict) -> None:
        fn_name = params.get("fn")
        task_id = params.get("task")
        input = params.get("input") or {}
        if not isinstance(input, dict):
            self._send(
                proto.error_response(
                    rid, proto.INVALID_PARAMS, "input must be an object"
                )
            )
            return
        ex = exposed_functions().get(fn_name)
        if ex is None:
            self._send(
                proto.error_response(
                    rid, proto.FUNCTION_NOT_FOUND, f"no such function {fn_name!r}"
                )
            )
            return
        if task_id is None:
            self._send(
                proto.error_response(
                    rid, proto.INVALID_PARAMS, "call requires a task id"
                )
            )
            return
        await self._tasks.start(task_id, rid, fn_name, ex.kind, ex.fn, input)
        # Handlers emit the response: value calls on completion, stream
        # calls at the terminal message (protocol §3.1).

    # ---- lifecycle ---------------------------------------------------------

    async def _drain(self, grace_s: float) -> None:
        """Cancel tasks, then exit. Called on shutdown and stdin EOF."""
        try:
            await asyncio.wait_for(self._tasks.cancel_all(), timeout=grace_s)
        except TimeoutError:
            _log("grace period expired during drain")
            for t in self._tasks.all():
                await self._tasks._kill_worker(
                    t
                )  # isolated workers must not be orphaned
        for t in self._tasks.all():
            if t.asyncio_task is not None:
                t.asyncio_task.cancel()

    async def run(self) -> None:
        loop = asyncio.get_running_loop()
        writer = threading.Thread(target=self._writer, name="tp-writer", daemon=True)
        reader = threading.Thread(
            target=self._reader, args=(loop,), name="tp-reader", daemon=True
        )
        writer.start()
        self._send(proto.hello(exposes_manifest()))
        reader.start()

        while not self._stopping:
            msg = await self._in.get()
            if msg is _EOF:
                _log("stdin EOF — host gone, draining")
                break
            await self._handle(msg)

        await self._drain(grace_s=3.0)
        self._out.put(_STOP)
        self._channel.close_write()
        writer.join(timeout=2)


def run(channel: FrameChannel) -> None:
    asyncio.run(Server(channel).run())
