# Pylair wire protocol v1

The contract between the host (Rust supervisor, inside the Tauri app) and
the managed Python runtime process. Implemented in M1; streaming and
cancellation messages are specified here but land in M3. Everything here
is deliberately boring — JSON-RPC 2.0 shaped, length-prefixed, stdio.

Goals: unambiguous framing, a versioned handshake, an error model that
maps cleanly to TypeScript, and no way for a malformed or hostile peer to
hang or OOM the host.

## 1. Transport

- Bytes flow over the child process's stdio: host writes to child stdin,
  child writes to child stdout. Stderr is a free-form log stream — the
  host forwards it to diagnostics; it is *never* part of the protocol.
- Behind a `Transport` trait in Rust and a matching abstraction in
  Python, so unix sockets / Windows named pipes can replace stdio later
  without touching message semantics.
- **stdout carries frames and nothing else.** Any byte on the frame
  channel that is not a well-formed frame is a protocol violation → the
  host treats the runtime as crashed (kill + restart). But libraries
  `print()`, and native code can write straight to fd 1, so the runtime
  does not rely on discipline — it performs **stdout hygiene at startup,
  before any third-party import can run**:
  - `dup()` the real stdout to a private fd — that fd is the frame
    channel;
  - `dup2(stderr, 1)` — fd 1 now points at stderr, so stray writes
    (Python `print`, C-extension `fwrite`, anything) land in the log
    stream instead of corrupting frames;
  - on Windows, both stdin and the frame fd are put in binary mode
    (`_setmode(..., O_BINARY)`) — CRLF translation would otherwise
    corrupt length-prefixed frames.
- **The host drains stderr continuously** on a dedicated task/thread.
  A full stderr pipe deadlocks the child, so draining is unconditional,
  not tied to diagnostics being enabled; if consumers are slow the host
  drops oldest lines with a `[dropped N lines]` marker rather than let
  the pipe fill.

## 2. Framing

```
+--------+--------------+
| u32 BE |  JSON body   |   u32 = byte length of the JSON body
+--------+--------------+
```

- Big-endian length prefix, then exactly that many bytes of UTF-8 JSON.
- `MAX_FRAME` defaults to **16 MiB**, configurable via
  `PYLAIR_MAX_FRAME` (bytes). A length prefix above the cap →
  protocol violation → runtime killed and restarted (it is either buggy
  or hostile). The cap is not meant for data throughput — large binary
  payloads (images, audio) currently travel base64-in-JSON (+33% size);
  a temp-file-handle transport for bulk data is the documented future
  escape hatch (`docs/ideas.md`).
- A peer that goes silent mid-frame is just slow; no read timeout on
  frames. Startup has its own deadline (§4) and steady-state liveness is
  covered by ping/pong (§5).
- **Wire JSON encoding** (`_json.py`, docs/codegen.md): beyond plain JSON
  types the runtime encodes `bytes`/`bytearray` as `{"$b64": "<base64>"}`
  (decoded symmetrically on input), `@dataclass` as its field dict,
  `datetime`/`date`/`time` as ISO 8601 strings, `Enum` as its value, and
  `set` as a list. Incoming params are coerced back by annotation — a
  dataclass-annotated parameter receives an instance, not a dict.
- **Serialization happens on the event loop**, before a message reaches
  the writer thread. A result that can't serialize (numpy/tensor
  objects, `NaN`/`Infinity` — `allow_nan=False` since bare `NaN` is not
  valid JSON and serde_json would reject the frame) fails *that one
  call* with `INTERNAL`; the writer thread sees only valid bytes and
  can never die silently.
- Param coercion failures are caller errors: `INVALID_PARAMS`, not a
  function exception (docs/codegen.md).

## 3. Message shapes

All messages carry `"jsonrpc": "2.0"`.

### Requests (host → runtime)

| method | params | response result |
|---|---|---|
| `call` | `{fn, input, task}` | fn's return value (JSON); for generator fns, see §3.1 |
| `cancel` | `{task}` | `{cancelled: true/false, reason?}` |
| `shutdown` | `{grace_ms}` | `{}` then process exits 0 |
| `ping` | `{seq}` | must produce a `pong {seq}` notification — see §5 |

- `fn`: name registered via `@expose`. Anything else →
  `FUNCTION_NOT_FOUND`. There is no way to reach unexposed callables —
  the allowlist lives in Python (`_registry`), enforced again by the
  host's permission layer.
- `task`: opaque id assigned by the host; scopes streaming + cancel.
- `input`: the single JSON argument object, matching the generated TS
  client call shape `python.<fn>({...})`.

### Responses (runtime → host)

```json
{"jsonrpc":"2.0","id":3,"result":{...}}
{"jsonrpc":"2.0","id":3,"error":{"code":-32010,"message":"task cancelled","data":{...}}}
```

`id` echoes the request. `error.data` may carry a Python traceback and
a `type` string (the Python exception class) for diagnostics.

### Notifications

| method | dir | params | meaning |
|---|---|---|---|
| `hello` | runtime→host | see §4 | handshake — first frame, once |
| `stream.chunk` | runtime→host | `{task, seq, value}` | one item of an async-generator result |
| `stream.credit` | host→runtime | `{task, n}` | consumer grants the producer n chunks of credit |
| `pong` | runtime→host | `{seq}` | liveness reply to `ping` |
| `log` | runtime→host | `{level, msg}` | structured log line (also goes to stderr) |

### 3.1 Calls and streaming

A `call` request always gets **exactly one response**.

- Non-streaming functions: the response is the return value
  (`result` or `error`), as usual.
- Streaming functions (`kind: "generator"` in `hello.exposes`): the
  `call`'s response **is the stream's terminal message** — it arrives
  after the last `stream.chunk` and carries either
  `result: {"done": true}` or an `error` (e.g. `TASK_CANCELLED`,
  `INTERNAL` raised mid-generation). The front end knows a call streams
  from the handshake allowlist metadata, not from message-shape
  inference — `python.<fn>` for a generator always yields an
  `AsyncIterable` whose items are the `stream.chunk` values and whose
  end/error is the call response.

**Flow control is pull-based and mandatory.** Async generators are
pull-based, so the wire is too: the runtime may only call `__anext__`
on a generator while it holds credit for that task.

- The consumer grants credit with `stream.credit {task, n}`. Credit is
  in *chunks*; each `stream.chunk` sent decrements outstanding credit
  by one.
- On stream open the host grants an initial credit of
  `INITIAL_STREAM_CREDIT` (default 64). The TS client replenishes as
  items are consumed — a throttled or backgrounded webview simply stops
  granting, and the generator pauses at the next `__anext__`. The
  stream stalls, it does not die.
- Safety cap for misbehaving producers: sending a `stream.chunk` with
  no outstanding credit, or buffering more than `MAX_STREAM_BUFFER`
  (default 8 MiB) of un-granted data, aborts the task with
  `PROTOCOL_VIOLATION` and counts as a protocol violation. This is the
  only remaining abort path — a well-behaved producer can never hit a
  limit by streaming "too fast"; it can only run out of credit, which
  pauses it.
- `cancel` is independent of credit: a task is cancellable whether it
  is producing, waiting on credit, or fully drained.

## 4. Handshake and lifecycle

Host-side states (emitted to the UI as events):
`starting → ready → crashed → restarting → failed`, plus `unresponsive`
(liveness warning, §5) and `stopped` (clean terminal after `shutdown` —
host-side only, never emitted by the runtime).

1. Host spawns the process, state `starting`.
2. Runtime's **first stdout frame must be `hello`**:
   `{protocol: 1, runtime_version, python_version, exposes: [{name, kind, signature, doc}]}`
   - `kind`: `"sync" | "async" | "generator" | "isolated"`
   - `signature`: `{params: [{name, kind, required, schema}], returns: schema}`
     where `schema` is the annotation map defined in docs/codegen.md (the
     same schema `pylair codegen` consumes). `doc` is the function's
     docstring or null — used for generated JSDoc.
3. `HELLO_TIMEOUT = 15s`. No hello → kill → `crashed` → backoff →
   `restarting`.
4. `protocol` mismatch → `failed` immediately (no restart — retrying
   can't fix a version skew). Error message names both versions.

## 5. Liveness

Supervision based on process exit alone misses the most common real
failure: a runtime **wedged** in native code (a C extension spinning, a
deadlock) never crashes, so it never restarts.

- The host sends `ping {seq}` every `PING_INTERVAL` (default 5s).
  The runtime answers with a `pong {seq}` notification within
  `PING_TIMEOUT` (default 10s).
- **Pongs are emitted by the reader thread, not the event loop.** A
  pong therefore proves the frame pipeline (and process) is alive —
  *not* that asyncio is making progress. Blocking code inside an
  `async def` (a very common AI-app pattern: model calls inside an
  async handler) starves the loop but must not fail liveness, or a
  healthy runtime would be killed on its first real workload.
- Event-loop lag is reported **separately as a diagnostic**: the
  runtime logs `event loop lag Ns` lines on stderr (forwarded to the
  UI via `pylair://stderr`) when the loop falls >1s behind. Calls
  queued behind a busy loop simply wait — same as any overloaded
  async service.
- A missed deadline → state `unresponsive` (event emitted). The host
  keeps pinging. `unresponsive` is entered **only after a real miss** —
  an in-flight ping below its deadline does not flap the UI state.
- `MAX_MISSES` consecutive misses (default 3) → treated as `crashed`:
  killed and the restart policy applies. Since pongs come from the
  reader thread, missing them means the *process* is truly hung
  (deadlocked native code, interpreter fault) — the correct thing to
  kill.
- **Transport health is not serving health.** The `pong` also carries
  `loop_stale_ms`: how long the runtime's asyncio loop has made no
  progress (measured by a heartbeat task on the loop, read by the reader
  thread). A stalled loop cannot serve calls or cancels, but a stall is
  not proof of a wedge — legitimate blocking work (loading a multi-GB
  model inside an `async def`) can stall it for minutes. The host
  therefore reports stalls past ~1s as a diagnostic (`Unresponsive`
  state + a stderr note) and keeps serving once the loop frees. Only a
  configured `loop_stall_kill` (**off by default**) treats a stall at or
  beyond that duration as wedged: in-flight calls fail with
  `RUNTIME_WEDGED` and the runtime is killed and restarted. The field is
  additive — hosts that ignore it keep the older behaviour.
- `seq` matching guards staleness.
- `cancel()` waits at most `cancel_timeout` (default 5s) for the runtime's
  acknowledgement and then fails with `RUNTIME_WEDGED`; a blocked runtime
  cannot be relied on to answer. The client's `timeoutMs` call option is
  an application-level deadline: it cancels the task and rejects with the
  client-side code `-32013` (`DEADLINE_EXCEEDED`) without waiting for that
  acknowledgement.

## 6. Orphan prevention

"No child process outlives the host" needs an actual mechanism, per
platform:

- **stdin EOF is the portable guarantee.** The runtime's frame loop
  exits when stdin reaches EOF (host dead → pipe closed), and it kills
  its isolated workers first. Workers likewise exit on their own
  control-channel EOF. The host therefore only has to *stop* cleanly;
  if it dies, everything downstream notices within one pipe read.
- **Windows:** "process group" doesn't exist — the host puts the
  runtime in a **Job Object** with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`;
  when the host's last job handle closes (clean or crash), the kernel
  kills everything in the job. The runtime does the same for workers.
- **POSIX:** the runtime gets its own process group and the supervisor
  kills `-pgid` — both on a forced kill and when the runtime exits on its
  own (so group members cannot outlive a dead leader). Isolated workers
  run in their *own session* (so one task can be cancelled by killing
  exactly its group), which a kill of the runtime's group cannot reach.
  Each worker therefore watches its parent and, when re-parented,
  SIGKILLs its whole group (worker plus any grandchildren, SIGTERM
  ignorers included). Limitation: a worker wedged in native code holding
  the GIL cannot run that watcher. There is no parent-death signal on
  macOS, so no `PR_SET_PDEATHSIG` is used on any POSIX platform.
- **Shutdown completes before it is acknowledged.** `shutdown()` resolves
  only after the runtime's process tree has been reaped, and the plugin
  blocks application exit on it (`RunEvent::Exit`). A shutdown requested
  during a restart backoff is honoured immediately.
- **Windows without a Job Object:** if assignment fails the host says so
  on the stderr event stream and falls back to `taskkill /T` on kill.

## 7. Supervisor policy (host-side, not on the wire)

- Restart backoff: exponential, `250ms → 8s` cap, `×2` factor, `±20%`
  jitter.
- Crash-loop breaker: **5 restarts in 60s → `failed`**. `failed` requires
  an explicit UI/app action (a "retry" path is allowed, it resets the
  counter).
- In-flight calls when the process dies → immediately fail with
  `RUNTIME_CRASHED`; they never hang waiting on a dead pipe. Whether a
  call is retried is the app's choice — the supervisor never replays
  calls (no idempotency guarantees in Python user code).
- Graceful `shutdown` → `grace_ms` deadline → SIGTERM → 5s → SIGKILL.
- Calls issued while the runtime is *restarting* (crash backoff) or
  *failed* fail fast with `RUNTIME_UNAVAILABLE`; calls issued while it is
  merely *starting* queue until the handshake completes.
- A stream consumer that stops draining (buffer full) or disappears ends
  with an explicit `INTERNAL` terminal error and the Python producer is
  cancelled. The terminal result is delivered on its own slot, so it is
  never lost to a full chunk buffer.

## 8. Error codes

JSON-RPC reserved plus app-specific, all in `-32000..-32099`:

| code | name | when |
|---|---|---|
| -32700 | PARSE_ERROR | malformed JSON / bad frame |
| -32600 | INVALID_REQUEST | wrong shape, missing fields |
| -32601 | FUNCTION_NOT_FOUND | `fn` not in `@expose` registry |
| -32602 | INVALID_PARAMS | input fails signature validation |
| -32000 | INTERNAL | uncaught Python exception (`data.type` = class) |
| -32001 | TASK_NOT_FOUND | cancel/shutdown for unknown `task` |
| -32010 | TASK_CANCELLED | task ended via cancel (also stream terminus) |
| -32011 | WORKER_DIED | isolated worker subprocess exited non-zero |
| -32012 | RUNTIME_WEDGED | liveness deadline exceeded `MAX_MISSES` times |
| -32030 | RUNTIME_UNAVAILABLE | host-side: not `ready`, restarting, or dead |
| -32031 | RUNTIME_CRASHED | process died mid-call |
| -32032 | PROTOCOL_VIOLATION | garbage on frame fd / bad hello / credit overrun |

## 9. Concurrency and ordering

- Multiple `call`s may be in flight; `task` ids are unique per process
  lifetime and never reused.
- `stream.chunk` ordering is guaranteed **per task only** (seq-ordered);
  chunks from different tasks may interleave.
- Flow control is credit-based (§3.1), not buffer-cap-based — producers
  are paused, never aborted for streaming "too much". The byte-wise
  `MAX_STREAM_BUFFER` remains only as a misbehavior tripwire.

## 10. Versioning

- `protocol` is a single integer. Host accepts exact match only in v1
  (a minor-version field can be added when there's a second version to
  talk to).
- Unknown top-level fields are ignored both directions (forward
  compatibility within a major version).
- The Python runtime refuses to start for an incompatible host the same
  way (host sends its version in an env var `PYLAIR_PROTOCOL=1`).

## 11. What this does not cover (yet)

- Type/schema details of `exposes[].signature` — docs/codegen.md.
- Cancellation *guarantees* per execution mode — `docs/cancellation.md`.
- Bulk binary transport (temp-file handles) — `docs/ideas.md`; base64
  in JSON is the v1 answer.
- Warm/model-persistent isolated workers — the `worker` option on
  `@expose` is reserved now (see `docs/cancellation.md`); v1 isolated
  workers are per-call.
- Auth/attestation of the child — the child is spawned by the host with
  stdio wired directly; nothing else can write its frame fd by design.
  Threat model in `docs/security.md`.
