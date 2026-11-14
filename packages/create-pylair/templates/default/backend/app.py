"""Example backend — @expose'd functions become the app's Python API."""

import asyncio
import sys
from collections.abc import AsyncIterator

from pylair import expose


@expose
def hello(name: str = "world") -> str:
    return f"hello, {name} — from python"


@expose
async def add(a: int, b: int) -> int:
    return a + b


@expose
async def count(n: int = 5) -> AsyncIterator[int]:
    for i in range(n):
        yield i
        await asyncio.sleep(0.2)


@expose
def log_stderr(msg: str) -> None:
    # The two-window self-test writes a marker line and asserts it reaches
    # only the owner window's pylair://stderr subscription.
    print(msg, file=sys.stderr)
