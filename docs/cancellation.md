# Cancellation semantics (stub)

To be written in M3 — cancellation is designed *first*, it is the hardest
correctness problem in the project (PROJECT_BRIEF.md section 4).

Must define, per execution mode:

- **Async tasks**: cancelled via `asyncio.Task.cancel()`; what the Python
  side guarantees (coroutine unwound, resources released), what the TS side
  observes (typed `Cancelled` error vs. stream termination).
- **`isolated=True` blocking tasks**: run in a worker subprocess the
  supervisor can `terminate()`/`kill()`; guarantees for partial results
  (discarded vs. delivered), and worst-case cleanup time.
- **Non-isolated blocking sync functions**: document that these cannot be
  interrupted safely and the guidance is to mark them isolated.
- What "cancelled" means while the runtime is restarting after a crash.

Tests per mode are part of the M3 exit criteria.
