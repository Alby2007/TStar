"""Tests run against a stub Ollama on a real localhost port — same wire
behavior as the real API (JSON-lines streaming included)."""

import asyncio
import json
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest

from pylair_ollama import Message, OllamaError, chat, pull, status


class Stub(BaseHTTPRequestHandler):
    hang_chat = False  # chat sends one chunk then blocks

    def log_message(self, *a):
        pass

    def _json(self, obj, code=200):
        b = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(b)))
        self.end_headers()
        self.wfile.write(b)

    def do_GET(self):
        if self.path == "/api/version":
            self._json({"version": "0.5.1"})
        elif self.path == "/api/tags":
            self._json({"models": [{"name": "qwen3:8b"}, {"name": "llama3.1:8b"}]})
        else:
            self._json({"error": "nope"}, 404)

    def do_POST(self):
        n = int(self.headers.get("Content-Length", 0))
        self.rfile.read(n)  # drain the request body
        if self.path == "/api/chat":
            self.send_response(200)
            self.send_header("Content-Type", "application/x-ndjson")
            self.end_headers()
            chunks = [
                {"message": {"role": "assistant", "content": "Hel"}, "done": False},
                {"message": {"role": "assistant", "content": "lo"}, "done": False},
            ]
            for c in chunks:
                self.wfile.write(json.dumps(c).encode() + b"\n")
                self.wfile.flush()
            if self.hang_chat:
                time.sleep(60)  # cancelled mid-stream; client must abort
            self.wfile.write(
                json.dumps(
                    {"message": {"role": "assistant", "content": "!"}, "done": True}
                ).encode()
                + b"\n"
            )
            self.wfile.flush()
        elif self.path == "/api/pull":
            self.send_response(200)
            self.send_header("Content-Type", "application/x-ndjson")
            self.end_headers()
            for ev in [
                {"status": "pulling", "completed": 50, "total": 100},
                {"status": "success"},
            ]:
                self.wfile.write(json.dumps(ev).encode() + b"\n")
            self.wfile.flush()
        else:
            self._json({"error": "nope"}, 404)


@pytest.fixture(scope="module")
def server():
    Stub.hang_chat = False
    srv = ThreadingHTTPServer(("127.0.0.1", 0), Stub)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    yield f"http://127.0.0.1:{srv.server_port}"
    srv.shutdown()


def run(coro):
    loop = asyncio.new_event_loop()
    try:
        return loop.run_until_complete(coro)
    finally:
        loop.close()


def test_status_running(server):
    s = run(status(server))
    assert s.running and s.version == "0.5.1"
    assert set(s.models) == {"qwen3:8b", "llama3.1:8b"}
    assert s.error is None


def test_status_down():
    s = run(status("http://127.0.0.1:1", timeout=0.3))  # nothing listens on :1
    assert not s.running
    assert "not running" in s.error


def test_chat_streams_deltas(server):
    async def go():
        out = []
        async for tok in chat(
            "qwen3:8b", [Message(role="user", content="hi")], host=server
        ):
            out.append(tok)
        return out

    assert run(go()) == ["Hel", "lo", "!"]


def test_chat_cancellation_aborts_stream(server):
    Stub.hang_chat = True
    try:

        async def go():
            gen = chat("m", [Message(role="user", content="x")], host=server)
            assert await gen.__anext__() == "Hel"
            assert await gen.__anext__() == "lo"
            # Third read blocks — the stub sleeps before the final chunk.
            task = asyncio.ensure_future(gen.__anext__())
            await asyncio.sleep(0.1)
            task.cancel()
            with pytest.raises(asyncio.CancelledError):
                await asyncio.wait_for(task, 5)
            await gen.aclose()

        run(go())
    finally:
        Stub.hang_chat = False


def test_chat_unreachable_host():
    async def go():
        async for _ in chat(
            "m", [Message(role="user", content="x")], host="http://127.0.0.1:1"
        ):
            pass

    with pytest.raises(OllamaError, match="not running"):
        run(go())


def test_pull_yields_progress(server):
    async def go():
        return [ev async for ev in pull("qwen3:8b", host=server)]

    evs = run(go())
    assert evs[0]["completed"] == 50
    assert evs[-1]["status"] == "success"
