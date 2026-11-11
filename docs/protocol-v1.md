# Tauri+ wire protocol v1

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
  `TAURI_PLUS_MAX_FRAME` (bytes). A length prefix above the cap →
  protocol violation → runtime killed and restarted (it is either buggy
  or hostile). The cap is not meant for data throughput — large binary
  payloads (images, audio) currently travel base64-in-JSON (+33% size);
  a temp-file-handle transport for bulk data is the documented future
  escape hatch (`docs/ideas.md`).
- A peer that goes silent mid-frame is just slow; no read timeout on
  frames. Startup has its own deadline (§4) and steady-state liveness is
  covered by ping/pong (§5).

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
`starting → ready → crashed → restarting → failed`.

1. Host spawns the process, state `starting`.
2. Runtime's **first stdout frame must be `hello`**:
   `{protocol: 1, runtime_version, python_version, exposes: [{name, kind, signature}]}`
   - `kind`: `"sync" | "async" | "generator" | "isolated"`
   - `signature`: JSON-schema-ish arg/return shape (full schema export is
     M2's codegen input; v1 records names + kinds for the allowlist).
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
  `PING_TIMEOUT` (default 10s) — the reply is emitted by the event
  loop's frame handler, so it proves the loop is alive.
- A missed deadline → state `unresponsive` (event emitted). The host
  keeps pinging.
- `MAX_MISSES` consecutive misses (default 3) → treated as `crashed`:
  killed and the restart policy applies.
- **Known limit:** a C extension holding the GIL starves the frame loop
  too, so `pong` can be late during legitimate long native calls. That
  is exactly why the spec pushes CPU-heavy work to `isolated=True`
  workers — whose liveness is process exit, not pong. `unresponsive`
  is a warning state so the UI can show "backend busy/stuck" before the
  supervisor gives up.
- `ping`/`pong` carries no data; `seq` matching just guards staleness.

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
- **POSIX:** runtime and workers get their own process group; the
  supervisor kills `-pgid`, and on Linux `PR_SET_PDEATHSIG` is set as a
  belt-and-suspenders. macOS has no parent-death signal — stdin EOF is
  the mechanism there (and everywhere, really).

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
  way (host sends its version in an env var `TAURI_PLUS_PROTOCOL=1`).

## 11. What this does not cover (yet)

- Type/schema details of `exposes[].signature` — M2 codegen doc.
- Cancellation *guarantees* per execution mode — `docs/cancellation.md`.
- Bulk binary transport (temp-file handles) — `docs/ideas.md`; base64
  in JSON is the v1 answer.
- Warm/model-persistent isolated workers — the `worker` option on
  `@expose` is reserved now (see `docs/cancellation.md`); v1 isolated
  workers are per-call.
- Auth/attestation of the child — the child is spawned by the host with
  stdio wired directly; nothing else can write its frame fd by design.
  Threat model in `docs/security.md`.
