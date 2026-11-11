"""Thin async Ollama client for Pylair apps.

Streams plain content deltas — when an app wraps `chat` in an async
generator and marks it @expose(kind="stream"), tokens flow through the
normal bridge: pull-credit backpressure, and cancellation aborts the HTTP
request because task cancellation propagates into httpx.
"""

from __future__ import annotations

import json
import shutil
from collections.abc import AsyncIterator
from dataclasses import dataclass, field

import httpx

DEFAULT_HOST = "http://127.0.0.1:11434"

# Generation streams can run for minutes; only the connect is bounded.
_TIMEOUT = httpx.Timeout(None, connect=5.0)


class OllamaError(RuntimeError):
    """Raised for unreachable/denied/erroring Ollama — the message is
    written for app UIs, not logs."""


@dataclass
class Message:
    role: str  # "system" | "user" | "assistant" | "tool"
    content: str


@dataclass
class OllamaStatus:
    installed: bool  # `ollama` binary on PATH
    running: bool  # API responded
    version: str | None = None
    models: list[str] = field(default_factory=list)
    error: str | None = None


def _err(e: Exception) -> OllamaError:
    if isinstance(e, httpx.ConnectError):
        return OllamaError(
            "Ollama is not running — start it with `ollama serve` "
            "(or install it from https://ollama.com)"
        )
    if isinstance(e, httpx.HTTPStatusError):
        try:
            detail = e.response.json().get("error", e.response.text)
        except (json.JSONDecodeError, ValueError, AttributeError):
            detail = e.response.text
        return OllamaError(f"Ollama error {e.response.status_code}: {detail}")
    return OllamaError(f"Ollama request failed: {e}")


async def status(host: str = DEFAULT_HOST, timeout: float = 2.0) -> OllamaStatus:
    """Installed? Running? Version, models — for app UIs and `pylair doctor`
    parity. Never raises; failures land in `error`."""
    s = OllamaStatus(installed=shutil.which("ollama") is not None, running=False)
    try:
        async with httpx.AsyncClient(base_url=host, timeout=timeout) as c:
            v = await c.get("/api/version")
            v.raise_for_status()
            s.running = True
            s.version = v.json().get("version")
            t = await c.get("/api/tags")
            t.raise_for_status()
            s.models = [m["name"] for m in t.json().get("models", [])]
    except httpx.HTTPError as e:
        s.error = str(_err(e))
        if not s.running and s.installed:
            s.error += " (binary found on PATH but API is down)"
    return s


async def chat(
    model: str,
    messages: list[Message],
    *,
    host: str = DEFAULT_HOST,
    options: dict | None = None,
) -> AsyncIterator[str]:
    """Yield assistant content deltas for a chat completion. Cancelling the
    consuming task aborts the HTTP request mid-stream."""
    payload: dict = {
        "model": model,
        "messages": [{"role": m.role, "content": m.content} for m in messages],
        "stream": True,
    }
    if options:
        payload["options"] = options
    try:
        async with (
            httpx.AsyncClient(base_url=host, timeout=_TIMEOUT) as c,
            c.stream("POST", "/api/chat", json=payload) as r,
        ):
            r.raise_for_status()
            async for line in r.aiter_lines():
                if not line:
                    continue
                chunk = json.loads(line)
                if err := chunk.get("error"):
                    raise OllamaError(err)
                if delta := chunk.get("message", {}).get("content"):
                    yield delta
                if chunk.get("done"):
                    return
    except OllamaError:
        raise
    except httpx.HTTPError as e:
        raise _err(e) from e


async def pull(model: str, *, host: str = DEFAULT_HOST) -> AsyncIterator[dict]:
    """Yield Ollama pull progress events ({status, completed, total}).
    For 'download the model on first run' UX."""
    try:
        async with (
            httpx.AsyncClient(base_url=host, timeout=_TIMEOUT) as c,
            c.stream("POST", "/api/pull", json={"model": model, "stream": True}) as r,
        ):
            r.raise_for_status()
            async for line in r.aiter_lines():
                if not line:
                    continue
                ev = json.loads(line)
                if err := ev.get("error"):
                    raise OllamaError(err)
                yield ev
    except OllamaError:
        raise
    except httpx.HTTPError as e:
        raise _err(e) from e
