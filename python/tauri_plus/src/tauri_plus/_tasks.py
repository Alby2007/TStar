"""Task registry: in-flight calls, credit-gated stream pumps, isolated workers."""

from __future__ import annotations

import asyncio
import json
import os
import signal
import sys
from collections.abc import AsyncGenerator, Callable
from dataclasses import dataclass, field
from typing import Any

from tauri_plus import _protocol as proto

ISOLATED_GRACE_S = 3.0


def _log(msg: str) -> None:
    print(f"[tauri_plus] {msg}", file=sys.stderr, flush=True)


@dataclass
class Task:
    id: int | str
    fn_name: str
    kind: str
    call_id: int | str  # the wire request id this task answers
    seq: int = 0
    credit: int = 0
    credit_event: asyncio.Event = field(default_factory=asyncio.Event)
    asyncio_task: asyncio.Task | None = None
    worker: asyncio.subprocess.Process | None = None
    done: bool = False


class TaskRegistry:
    def __init__(self, send: Callable[[dict], None]) -> None:
        self._send = send
        self._tasks: dict[int | str, Task] = {}

    def get(self, task_id: int | str) -> Task | None:
        return self._tasks.get(task_id)

    def all(self) -> list[Task]:
        return list(self._tasks.values())

    async def start(
        self,
        task_id: int | str,
        call_id: int | str,
        fn_name: str,
        kind: str,
        fn: Callable[..., Any],
        input: dict,
    ) -> None:
        task = Task(id=task_id, fn_name=fn_name, kind=kind, call_id=call_id)
        self._tasks[task_id] = task

        if kind == "generator":
            task.credit = proto.INITIAL_STREAM_CREDIT
            task.asyncio_task = asyncio.create_task(self._pump_stream(task, fn, input))
        elif kind == "async":
            task.asyncio_task = asyncio.create_task(self._run_async(task, fn, input))
        elif kind == "isolated":
            task.asyncio_task = asyncio.create_task(
                self._run_isolated(task, fn_name, input)
            )
        else:  # sync inline — uncancellable per docs/cancellation.md
            task.asyncio_task = asyncio.create_task(self._run_sync(task, fn, input))

    # ---- execution modes -------------------------------------------------

    async def _run_async(self, task: Task, fn: Callable[..., Any], input: dict) -> None:
        try:
            result = await fn(**input)
            self._finish(task, proto.response(task.call_id, result))
        except asyncio.CancelledError:
            self._finish(
                task,
                proto.error_response(
                    task.call_id, proto.TASK_CANCELLED, "task cancelled"
                ),
            )
        except Exception as e:  # noqa: BLE001 - user boundary
            self._finish(
                task,
                proto.error_response(
                    task.call_id, proto.INTERNAL, str(e), {"type": type(e).__name__}
                ),
            )

    async def _run_sync(self, task: Task, fn: Callable[..., Any], input: dict) -> None:
        loop = asyncio.get_running_loop()
        try:
            result = await loop.run_in_executor(None, lambda: fn(**input))
            self._finish(task, proto.response(task.call_id, result))
        except asyncio.CancelledError:
            # Inline sync can't be interrupted — the executor thread keeps
            # running and its result is discarded, but the caller still
            # observes TASK_CANCELLED now (docs/cancellation.md).
            self._finish(
                task,
                proto.error_response(
                    task.call_id, proto.TASK_CANCELLED, "task cancelled"
                ),
            )
        except Exception as e:  # noqa: BLE001 - user boundary
            self._finish(
                task,
                proto.error_response(
                    task.call_id, proto.INTERNAL, str(e), {"type": type(e).__name__}
                ),
            )

    async def _pump_stream(
        self, task: Task, fn: Callable[..., Any], input: dict
    ) -> None:
        agen: AsyncGenerator = fn(**input)
        try:
            while True:
                # Pull-based flow control: only advance while we hold credit.
                await self._wait_for_credit(task)
                value = await agen.__anext__()
                task.credit -= 1
                self._send(
                    proto.notification(
                        "stream.chunk",
                        {"task": task.id, "seq": task.seq, "value": value},
                    )
                )
                task.seq += 1
        except StopAsyncIteration:
            self._finish(task, proto.response(task.call_id, {"done": True}))
        except asyncio.CancelledError:
            await _aclose_quietly(agen)
            self._finish(
                task,
                proto.error_response(
                    task.call_id, proto.TASK_CANCELLED, "task cancelled"
                ),
            )
        except Exception as e:  # noqa: BLE001 - user boundary
            await _aclose_quietly(agen)
            self._finish(
                task,
                proto.error_response(
                    task.call_id, proto.INTERNAL, str(e), {"type": type(e).__name__}
                ),
            )

    async def _wait_for_credit(self, task: Task) -> None:
        while task.credit <= 0:
            task.credit_event.clear()
            await task.credit_event.wait()

    async def _run_isolated(self, task: Task, fn_name: str, input: dict) -> None:
        module = os.environ["TAURI_PLUS_APP_MODULE"]
        env = {**os.environ}
        kwargs: dict[str, Any] = {
            "stdin": asyncio.subprocess.PIPE,
            "stdout": asyncio.subprocess.PIPE,
        }
        if sys.platform == "win32":
            kwargs["creationflags"] = 0  # taskkill /T handles the tree
        else:
            kwargs["start_new_session"] = True  # own process group -> killpg
        task.worker = await asyncio.create_subprocess_exec(
            sys.executable,
            "-m",
            "tauri_plus._worker",
            module,
            fn_name,
            env=env,
            **kwargs,
        )
        try:
            out, _ = await task.worker.communicate(json.dumps(input).encode())
            if task.worker.returncode != 0:
                tail = out.decode(errors="replace")[-2000:]
                self._finish(
                    task,
                    proto.error_response(
                        task.call_id,
                        proto.WORKER_DIED,
                        f"worker exited {task.worker.returncode}",
                        {"output": tail} if tail else None,
                    ),
                )
                return
            payload = json.loads(out)
            if "error" in payload:
                self._finish(
                    task,
                    proto.error_response(
                        task.call_id,
                        proto.INTERNAL,
                        payload["error"]["message"],
                        payload["error"].get("data"),
                    ),
                )
            else:
                self._finish(task, proto.response(task.call_id, payload["result"]))
        except asyncio.CancelledError:
            await self._kill_worker(task)
            self._finish(
                task,
                proto.error_response(
                    task.call_id, proto.TASK_CANCELLED, "task cancelled"
                ),
            )

    async def _kill_worker(self, task: Task) -> None:
        proc = task.worker
        if proc is None or proc.returncode is not None:
            return
        try:
            if sys.platform == "win32":
                await asyncio.create_subprocess_exec(
                    "taskkill", "/PID", str(proc.pid), "/T", "/F"
                )
            else:
                os.killpg(proc.pid, signal.SIGTERM)
            await asyncio.wait_for(proc.wait(), timeout=ISOLATED_GRACE_S)
        except (TimeoutError, ProcessLookupError):
            try:
                proc.kill()
            except ProcessLookupError:
                pass
            await proc.wait()

    # ---- cancel / credit / finish -----------------------------------------

    def grant_credit(self, task_id: int | str, n: int) -> bool:
        task = self._tasks.get(task_id)
        if task is None or task.done:
            return False
        task.credit += n
        task.credit_event.set()
        return True

    async def cancel(self, task_id: int | str) -> dict:
        task = self._tasks.get(task_id)
        if task is None or task.done:
            return {"cancelled": False, "reason": "task_not_found"}
        if task.asyncio_task is not None and not task.asyncio_task.done():
            task.asyncio_task.cancel()
            asyncio.create_task(self._ensure_terminal(task))
        # Ack immediately; the terminal response follows via the task's own
        # handler or the finisher below — exactly once, _finish dedupes.
        return {"cancelled": True}

    async def _ensure_terminal(self, task: Task) -> None:
        """Guarantee a terminal response even if the handler never ran.

        A task cancelled before its first step never enters its handler's
        except block, so nothing would emit TASK_CANCELLED.
        """
        try:
            await asyncio.wait_for(asyncio.shield(task.asyncio_task), timeout=5.0)
        # CancelledError is BaseException, not Exception — catch broadly so the
        # task.done check below always runs.
        except BaseException as e:  # noqa: BLE001
            _log(f"finisher: task {task.id} ended with {e!r} before marking done")
        if not task.done:
            self._finish(
                task,
                proto.error_response(
                    task.call_id, proto.TASK_CANCELLED, "task cancelled"
                ),
            )

    async def cancel_all(self) -> None:
        for task in self._tasks.values():
            if not task.done:
                await self.cancel(task.id)

    def _finish(self, task: Task, terminal: dict) -> None:
        if task.done:
            return  # a cancelled task's late result is discarded
        task.done = True
        self._send(terminal)
        self._tasks.pop(task.id, None)


async def _aclose_quietly(agen: AsyncGenerator) -> None:
    try:
        await agen.aclose()
    except Exception as e:  # noqa: BLE001 - cleanup must not kill the loop
        _log(f"generator aclose raised: {e!r}")
