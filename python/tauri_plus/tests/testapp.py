"""Test application module loaded by the runtime under test.

Exercises every execution mode plus the stdout-hygiene guarantees.
"""

import asyncio
import os
import sys
import time

from tauri_plus import expose


@expose
def add(a: int, b: int) -> int:
    return a + b


@expose
def print_then_answer(text: str) -> str:
    # Both print() and a raw fd-1 write must land on stderr, never the
    # frame channel (protocol §1 stdout hygiene).
    print(f"print: {text}")
    os.write(1, b"raw fd-1 write\n")
    return text


@expose
async def echo_later(text: str, delay: float = 0.01) -> str:
    await asyncio.sleep(delay)
    return text


@expose
async def counter(n: int):
    for i in range(n):
        yield i


@expose
async def infinite():
    i = 0
    while True:
        yield i
        i += 1


@expose
async def sleep_forever() -> None:
    await asyncio.sleep(3600)


@expose
def sync_sleep(seconds: float) -> str:
    time.sleep(seconds)
    return "slept"


@expose(isolated=True)
def isolated_sleep(seconds: float) -> str:
    time.sleep(seconds)
    return "slept"


@expose(isolated=True)
def isolated_add(a: int, b: int) -> int:
    return a + b


@expose(isolated=True)
def isolated_print(text: str) -> str:
    # stdout hygiene inside the worker: print() must not corrupt the result.
    print(f"worker print: {text}")
    return text


@expose(isolated=True)
def orphan_maker(pidfile: str) -> str:
    """Spawn a grandchild that IGNORES SIGTERM, then sleep.

    Cancelling must kill the whole process group — the grandchild surviving
    is the leaked-orphan failure mode.
    """
    import subprocess

    proc = subprocess.Popen(
        [
            sys.executable,
            "-c",
            (
                "import signal, time\n"
                "signal.signal(signal.SIGTERM, signal.SIG_IGN)\n"
                "time.sleep(600)\n"
            ),
        ]
    )
    with open(pidfile, "w") as f:
        f.write(str(proc.pid))
    time.sleep(600)
    return "never"


@expose
async def wedge_the_loop() -> None:
    # Starves the event loop in native code — the fault-injection case for
    # ping/pong liveness. Never returns on its own.
    time.sleep(3600)  # noqa: ASYNC251 - intentional event-loop starvation
