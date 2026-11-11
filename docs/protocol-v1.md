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
- **stdout carries frames and nothing else.** Any byte on stdout that is
  not a well-formed frame is a protocol violation → the host treats the
  runtime as crashed (kill + restart). `tauri_plus` runtime code must
  never `print()`; logging goes to stderr.

## 2. Framing

```
+--------+--------------+
| u32 BE |  JSON body   |   u32 = byte length of the JSON body
+--------+--------------+
```

- Big-endian length prefix, then exactly that many bytes of UTF-8 JSON.
- `MAX_FRAME = 16 MiB`. A length prefix above the cap → protocol
  violation → runtime killed and restarted (it is either buggy or hostile).
- A peer that goes silent mid-frame is just slow; no read timeout on
  frames. Startup has its own deadline (§4).

## 3. Message shapes

All messages carry `"jsonrpc": "2.0"`.

### Requests (host → runtime)

| method | params | response result |
|---|---|---|
| `call` | `{fn, input, task}` | fn's return value (JSON) |
| `cancel` | `{task}` | `{cancelled: true/false, reason?}` |
| `shutdown` | `{grace_ms}` | `{}` then process exits 0 |

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

### Notifications (runtime → host, no `id`)

| method | params | meaning |
|---|---|---|
| `hello` | see §4 | handshake — first frame on stdout, once |
| `stream.chunk` | `{task, seq, value}` | one item of an async-generator result |
| `stream.end` | `{task}` | generator finished normally |
| `log` | `{level, msg}` | structured log line (also goes to stderr) |

Streams: for a call whose Python result is an async generator, the
runtime sends `stream.chunk` notifications (ordered by `seq`, starting 0)
then exactly one terminal message: `stream.end`, or a response-shaped
error notification `{method:"stream.error", params:{task, error:{...}}}`.
The host reassembles them into the `AsyncIterable` the TS client exposes.

### Requests (runtime → host)

None in v1. The runtime never initiates calls into the host.

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

## 5. Supervisor policy (host-side, not on the wire)

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

## 6. Error codes

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
| -32020 | BACKPRESSURE_LIMIT | stream exceeded buffer cap |
| -32030 | RUNTIME_UNAVAILABLE | host-side: not `ready`, restarting, or dead |
| -32031 | RUNTIME_CRASHED | process died mid-call |
| -32032 | PROTOCOL_VIOLATION | garbage on stdout / bad hello |

## 7. Concurrency, ordering, backpressure

- Multiple `call`s may be in flight; `task` ids are unique per process
  lifetime and never reused.
- `stream.chunk` ordering is guaranteed **per task only** (seq-ordered);
  chunks from different tasks may interleave.
- v0.1 backpressure: the runtime buffers per task up to
  `min(1024 chunks, 8 MiB)`. Exceeding → task aborted with
  `BACKPRESSURE_LIMIT`, error surfaces on the TS side. Credit-based flow
  control is a later optimization (docs/ideas.md) — the wire shape
  supports it without change (a `stream.credit {task, n}` notification).

## 8. Versioning

- `protocol` is a single integer. Host accepts exact match only in v1
  (a minor-version field can be added when there's a second version to
  talk to).
- Unknown top-level fields are ignored both directions (forward
  compatibility within a major version).
- The Python runtime refuses to start for an incompatible host the same
  way (host sends its version in an env var `TAURI_PLUS_PROTOCOL=1`).

## 9. What this does not cover (yet)

- Type/schema details of `exposes[].signature` — M2 codegen doc.
- Cancellation *guarantees* per execution mode — `docs/cancellation.md`.
- Auth/attestation of the child — the child is spawned by the host with
  stdio wired directly; nothing else can write its stdout by design.
  Threat model in `docs/security.md`.
