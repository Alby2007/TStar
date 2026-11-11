# pylair-ollama

Optional Ollama integration (M5) — never a dependency of Pylair core.

Apps opt in by depending on `pylair-ollama` and `@expose`-ing the helpers
themselves; detection, status, and streaming then ride the normal bridge —
including pull-credit backpressure and task cancellation (cancelling a
stream aborts the HTTP request mid-token).

```python
from collections.abc import AsyncIterator

from pylair import expose
from pylair_ollama import Message, OllamaStatus, chat, pull, status


@expose
async def ollama_status() -> OllamaStatus:
    """installed / running / version / models — for the app's UI."""
    return await status()


@expose
async def ask(prompt: str, model: str = "qwen3:8b") -> AsyncIterator[str]:
    """Token-by-token through the bridge; cancel from the UI aborts the
    Ollama request mid-stream."""
    async for tok in chat(model, [Message(role="user", content=prompt)]):
        yield tok


@expose
async def pull_model(model: str) -> AsyncIterator[dict]:
    """First-run UX: stream download progress to the UI."""
    async for ev in pull(model):
        yield ev
```

`pylair codegen` types `OllamaStatus`/`Message` automatically, so the UI
gets typed stubs for free.

## What it reports

`status()` never raises: `installed` (binary on PATH), `running` (API
responds), `version`, `models`, `error` with a plain-language message
("Ollama is not running — start it with `ollama serve`").

`chat` / `pull` raise `OllamaError` with UI-ready messages; unreachable
host → "Ollama is not running", API error → "Ollama error N: <detail>".

## Out of scope (deliberate)

- Bundling an inference runtime — Ollama stays an external dependency the
  user installs (brief §12 open question 4). `pull()` covers the "model
  not downloaded yet" UX without shipping llama.cpp.
- `ollama serve` auto-start: `status()` reports it; the app decides.
- Embeddings/images/tool-call structured output: the wire shapes are
  there when an app needs them; this package stays small.
