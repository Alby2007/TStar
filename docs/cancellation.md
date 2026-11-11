# Cancellation semantics

Cancellation is the hardest correctness problem in Tauri+ (PROJECT_BRIEF
section 4), so it is specified before it is built. This document is the
contract: `docs/protocol-v1.md` says *how* cancel travels on the wire;
this says *what it guarantees*. Implemented in M3; tests in M3 must prove
each guarantee below, per mode.

## The question

When the front end drops a call (AbortSignal, navigation, user cancel),
what exactly happens to the Python work? The answer differs by execution
mode — Python has no safe way to kill a thread — so the mode is chosen
at `@expose` time, not at cancel time.

## Execution modes

| mode | declared as | runs in | cancel mechanism |
|---|---|---|---|
| async task | `@expose` + `async def` | runtime event loop | `asyncio.Task.cancel()` |
| async generator | `@expose` + `async def` + `yield` | runtime event loop | cancel + generator `aclose()` |
| isolated sync | `@expose(isolated=True)` `def` | worker subprocess | `terminate()` → `kill()` |
| inline sync | `@expose` `def` | runtime worker thread | **cannot interrupt** — see below |

### Async tasks and generators

- Cancel sends `Task.cancel()` at the next `await` point. The coroutine
  unwinds normally: `try/finally` and `async with` blocks run, resources
  are released.
- **Guarantee:** if the Python code is well-behaved async code, cancel is
  clean and complete. `CancelledError` propagating out of the function
  maps to wire error `TASK_CANCELLED`.
- **Limit:** a coroutine blocked in *native* code (a sync C call inside a
  library — e.g., a torch op not awaited) cannot be preempted; cancel
  takes effect when control returns to the event loop. Docs must say:
  for CPU-heavy or native-blocking work, use `isolated=True`.
- Generators: cancel closes the stream; the TS `AsyncIterable` ends. If
  the generator is mid-`yield` the runtime discards the pending chunk.
  Partial results are *delivered* (already-sent chunks stand), never
  rolled back.

### Isolated sync functions (`isolated=True`)

- Each call runs in a dedicated worker subprocess (spawned per call,
  pooled later if profiling says so — v1 is per-call, which is the only
  mode with a clean kill story).
- Cancel → `terminate()` (SIGTERM/SIGKILL-equivalent), 3s grace →
  `kill()`. Wire result: `TASK_CANCELLED` (or `WORKER_DIED` if the worker
  already died on its own).
- **Guarantee:** the worker *will* die — the supervisor owns its process
  handle and kills the whole process group, so children the function
  spawned die too (no leaked grandchildren — M3 test asserts this).
- **Partial results:** discarded. An isolated call returns a value only
  on normal completion; cancelled calls deliver nothing.
- **Cleanup:** `finally` blocks in the worker run during the SIGTERM
  grace only. After `kill()` nothing in the worker runs again — files
  it wrote stay written (documented; cleanup is the app's business).

### Inline sync functions (non-isolated)

- **Cannot be safely cancelled.** Python cannot preempt a running
  sync function in a thread, full stop.
- The cancel request is *recorded*: the function keeps running, its
  result is computed and **discarded**, and the caller still observes
  `TASK_CANCELLED` immediately — the UI is never held hostage.
- Docs/API guidance: mark anything that can run longer than ~1s or touch
  native code `isolated=True`. `tauri-plus doctor` warns on exposed sync
  functions without the flag (M4).

## Cross-cutting cases

- **Cancel during restart:** task dies with the process →
  `RUNTIME_CRASHED`, not `TASK_CANCELLED` (distinct errors matter —
  one is a user action, one is a failure).
- **Cancel an unknown/finished task:** `TASK_NOT_FOUND` /
  `{cancelled: false}` — never an error for the caller.
- **Cancel ack timeout:** if the runtime doesn't confirm within 5s, the
  host treats the call as cancelled from its side (result discarded,
  handle released) and notes it in diagnostics. For isolated tasks the
  worker is killed regardless of ack.
- **Cancel is one-way and idempotent:** re-cancel is a no-op. There is
  no un-cancel.
- **In-flight during shutdown:** graceful shutdown drains up to
  `grace_ms`; tasks past the deadline get `TASK_CANCELLED` (async) or
  process-group kill (isolated workers are children of the runtime —
  killing the runtime must not orphan them; supervisor tracks and
  reaps).

## Leak guarantees (what the M3 tests assert)

- No child processes outlive the host or a cancelled isolated call.
- No `asyncio.Task` objects leak after cancel (runtime-side assertion
  via task accounting).
- Every cancelled stream emits exactly one terminal message
  (`TASK_CANCELLED` or `stream.end` if it finished first — race is
  resolved task-side, both orders are legal).

## Wire summary

- `cancel {task}` → `{cancelled: bool, reason?}` or `TASK_NOT_FOUND`.
- Task end states observable by caller: `result`, `INTERNAL`,
  `TASK_CANCELLED`, `RUNTIME_CRASHED`, `WORKER_DIED`,
  `BACKPRESSURE_LIMIT`. Exactly one, always.
