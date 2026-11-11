# Ideas (not in v0.1 scope)

Parking lot for ideas that come up during development. Do not implement
from this file — promote to a milestone or a decision doc first.

- PyInstaller as an opt-in alternative packaging backend
  (evaluate after M0 findings; PROJECT_BRIEF.md section 4).
- Bundled inference runtime (llama.cpp or similar) instead of requiring
  external Ollama — owner question, section 12.
- Unix socket / Windows named pipe transport behind the transport trait.
- Svelte template (`templates/svelte-ts`).
- Temp-file-handle transport for bulk binary payloads (images, audio)
  to replace base64-in-JSON (+33% wire cost) — protocol note §2.
- Warm/model-persistent isolated workers (`@expose(worker={"mode":
  "warm"})`): keep imports + loaded model resident; kill on cancel,
  lazily respawn on next call. The API is already reserved; per-call
  workers reload everything — seconds with torch — so this is what
  makes `isolated=True` usable for AI apps.
- Answer `pong` from the runtime's reader THREAD, not the asyncio loop:
  today a healthy runtime is killed when user code blocks the loop inside
  an `async def` (common: sync model call awaited directly). Thread-side
  pongs prove the host<->runtime channel is alive; report event-loop lag
  separately as a diagnostic metric instead of a kill signal.
  (Review 2026-02 — tracked, not M1-blocking.)
- Event-loop-lag diagnostic: heartbeat timestamp in `pong` params so the
  host can warn "loop busy" without restarting a healthy runtime.
