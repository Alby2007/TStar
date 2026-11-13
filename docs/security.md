# Security model

This document describes the security boundaries of Pylair as implemented
(`crates/tauri-plugin-pylair`, `python/pylair`, `packages/client`), and —
just as important — what it does **not** claim. Where a control is planned
but not shipped yet, it says so.

## 1. Threat model

Three trust domains, separated by real process boundaries:

```
webview (least trusted)  →  Tauri IPC  →  Rust host (the TCB)
                                           ↓ stdio frames
                                      Python runtime (semi-trusted)
                                           ↓ subprocess
                                      isolated workers (per-call)
```

- **The webview is the least-trusted layer.** We assume the JavaScript
  running in it may be hostile — an XSS payload, a compromised frontend
  dependency, or content the app didn't intend to load. Everything the
  webview can do must survive that assumption.
- **The Python runtime is semi-trusted.** It is the developer's own code,
  but it runs third-party dependencies and processes untrusted input that
  arrived through exposed functions. A buggy or compromised runtime must
  not be able to hang, corrupt, or exhaust the host — it can still do
  anything *its own process privileges* allow (see §8).
- **The Rust host is the trusted computing base.** It enforces
  capabilities, owns the wire protocol, bounds every resource, and
  supervises the child. Its own attack surface is the frame channel, so
  the channel is designed so nothing can corrupt it (§5).

A second machine, or other processes on the user's machine, are out of
scope except where noted (wrap mode's loopback server, §9).

## 2. What the webview can reach — and nothing else

The plugin's entire IPC surface is eight commands:

| command | what it does | default permission |
|---|---|---|
| `python_call` | invoke an exposed function, await result | `pylair:default` |
| `python_call_stream` | invoke a streaming function | `pylair:default` |
| `python_grant_credit` | grant stream credit to an owned task | `pylair:default` |
| `python_cancel` | cancel an owned task | `pylair:default` |
| `python_state` | read lifecycle state | `pylair:default` |
| `python_retry` | respawn a `failed` runtime | opt-in only |
| `python_shutdown` | stop the runtime | opt-in only |
| `python_selftest_report` | end the installed-webview self-test | template grants; inert w/o env flag |

There are **no** fs, env, shell, exec, or network commands in the plugin.
The only way from the webview into Python is `call`/`call_stream`, and the
only functions reachable that way are ones the developer marked `@expose`
(§3). `python_selftest_report` is additionally inert unless the host
itself was launched with `PYLAIR_SELF_TEST_WEBVIEW`, so granting its
permission by accident opens nothing.

`pylair:default` deliberately excludes `python_retry` and
`python_shutdown` — supervisor control is a power an app opts into with
`pylair:allow-python-retry` / `pylair:allow-python-shutdown`, not part of
the everyday set.

## 3. The `@expose` allowlist — no arbitrary execution

The callable allowlist lives in Python (`_registry`), populated only by
the `@expose` decorator at import time. On a `call`, the runtime:

1. looks up `fn` in the registry — anything else fails with
   `FUNCTION_NOT_FOUND` (`-32601`);
2. requires `input` to be a JSON object;
3. coerces each argument against the function's signature — failures are
   `INVALID_PARAMS` (`-32602`), caller errors, not function exceptions.

There is no eval/exec/getattr-by-wire path anywhere. Even the isolated
worker — which resolves its target via `getattr(module, fn)` — receives a
`fn_name` that already passed the registry check, and a module name the
host set via `PYLAIR_APP_MODULE`, not a wire value.

**Honest corollary:** `@expose` *is* the attack surface. An exposed
function runs with the Python process's full privileges, and its `input`
is JSON supplied by a possibly-hostile webview. Exposed functions should
validate their inputs the way a server-side endpoint would —
`INVALID_PARAMS` coercion checks types, not intent. If you expose a
function that runs user-chosen code, you have re-introduced arbitrary
execution yourself; the framework can't prevent that and doesn't try.

## 4. Tauri capabilities gate every command

Pylair is a Tauri plugin and uses Tauri's capability/permission model
unchanged:

- A webview can invoke `python_call` only if a capability grants it
  `pylair:allow-python-call` (directly or via `pylair:default`). The
  scaffolded template's capability covers `windows: ["main"]` only —
  its permissions are `core:default`, `pylair:default`, and
  `pylair:allow-python-selftest-report` (inert, §2).
- **Remote origins get no IPC.** A capability never covers remote URLs
  unless it explicitly adds `remote.urls` — verified in the wrap-mode
  spike: from a remote page `window.__TAURI__` is undefined and every
  `__TAURI_INTERNALS__.invoke` call (pylair commands included) is
  rejected. The decision record makes this probe a permanent
  acceptance-job check, so a future capability change that opens remote
  IPC fails CI.

### What capabilities do not scope (known limitation)

Invocation is capability-level: within a granted capability, every
covered webview can invoke every exposed function. Pylair adds no
per-window or per-function ACL on invocation. Finer scoping is planned
with wrap mode.

**Task control, however, is owner-scoped.** The supervisor records
`{task → call id, owner webview}` per runtime generation, where the owner
is the Tauri-reported window label — never a JS-supplied value:

- `cancel`/`grant_credit` from a non-owner webview → rejected;
- a task id from a dead runtime generation → `not_found` (it can never
  control a new task);
- destroying a window cancels all of its tasks.

Event delivery matches: stream chunks go only to the webview that started
the stream; state/stderr/exposes go only to webviews that have used the
bridge. Nothing is broadcast. A multi-window app cannot leak one window's
stream into another (P1-8, covered by `tests/event_scoping.rs`).

## 5. Malformed input, in both directions

The frame channel is a private fd pair the host wired at spawn — nothing
else holds the write end, so "messages" can only come from the peer, and
the only attacks are malformed frames and resource exhaustion.

**Runtime → host.** Any byte that isn't a well-formed frame (oversized
length prefix, bad JSON, wrong shape) is a protocol violation: all
in-flight calls fail with `PROTOCOL_VIOLATION` (`-32032`), the process is
killed and reaped, and normal crash/restart accounting applies — a peer
that can't hold the protocol is treated as buggy-or-hostile, not
negotiated with. The handshake is stricter still: the first frame must be
`hello` within 15 s, and a `protocol` version mismatch goes straight to
`failed` with no restart (retrying can't fix version skew).

**Host → runtime.** A corrupt host isn't the threat model, but a corrupt
channel can't wedge the runtime either: bad JSON → `PARSE_ERROR`;
non-object or wrong-shaped messages → `INVALID_REQUEST`; unknown methods
→ `FUNCTION_NOT_FOUND`; bad call inputs → `INVALID_PARAMS`. A length
prefix over `MAX_FRAME` (default 16 MiB) makes the runtime treat the host
as gone — it cancels tasks and exits rather than allocate on demand.
Dispatch itself cannot die: any handler exception becomes an `INTERNAL`
error for that one call.

**stdout hygiene** keeps the channel honest at the source. Before any
third-party import runs, the runtime dups real stdout to a private fd
(the frame channel) and points fd 1 at stderr — so `print()`, C-extension
writes, and library logging land in the log stream and can never corrupt
a frame. The host drains stderr unconditionally and drops oldest lines
with a `[dropped N lines]` marker under backpressure, so a flooding
runtime can't deadlock on a full pipe either. Both sides use binary mode
on Windows so CRLF translation can't corrupt length prefixes.

## 6. Bounded everything

A hostile or buggy peer must not be able to hang or OOM the other side.
Every queue, buffer, and counter is bounded (`Limits` on
`SupervisorConfig`, env-overridable via `PYLAIR_MAX_*`, logged at
startup): concurrent tasks and streams, queued commands, per-stream and
total buffered stream bytes, outstanding stream credit, isolated workers,
and frame size. A hit fails the offending call/stream with
`LIMIT_EXCEEDED` (`-32040`, `data.limit` names the cap) — one failure,
bounded and explicit, never a silent stall.

Runtime-side mirrors: an inbound bound applies backpressure through the
OS pipe; the outbound queue is frame-and-byte bounded, and when it fills
the runtime concludes the host is gone, drains, and exits — callers see
`RUNTIME_CRASHED`, not a hang.

Two more bounded paths matter:

- **Control frames have their own lane.** `cancel`, `stream.credit`, and
  `shutdown` drain ahead of data frames, so a stream flood cannot starve
  or silently drop control.
- **Liveness is enforced, not assumed.** The host pings every 5 s; pongs
  are emitted by the runtime's reader thread (not the event loop), so a
  missed pong means the *process* is truly hung. Three consecutive misses
  → killed as `crashed`. Event-loop stalls are reported separately via
  `loop_stale_ms` as a diagnostic; `loop_stall_kill` (off by default)
  makes a long stall fatal for apps that prefer it.

Known gap: limits are per-runtime, not per-window — a webview that floods
the bridge can starve other windows of the same runtime.

## 7. Process boundaries and orphan prevention

"No child outlives the host" is a mechanism, not a hope (protocol §6):

- **stdin EOF is the portable guarantee** — a dead host closes the pipe,
  the runtime drains and exits, killing its isolated workers first.
- **Windows:** the runtime sits in a Job Object with
  `KILL_ON_JOB_CLOSE`; the kernel kills the whole tree when the last
  handle closes. Fallback: `taskkill /T`.
- **Linux:** process-group kill plus `PR_SET_PDEATHSIG` on both the
  runtime and each worker — the kernel kills them even if the host dies
  before any cleanup runs.
- **macOS:** no parent-death signal exists. A *healthy* runtime exits on
  stdin EOF and isolated workers carry their own parent watcher, but a
  runtime wedged in native code can survive an abrupt host kill — the
  documented gap (README → Known limitations).

Isolated workers (`@expose(isolated=True)`) are per-call subprocesses in
their own process group/session so exactly one task's tree can be
killed on cancel. The group kill reaches SIGTERM-ignoring grandchildren
but not descendants that deliberately detach (`setsid`, double-fork) —
documented, accepted.

## 8. What the runtime process itself can do

Pylair does **not** sandbox Python. The runtime — and anything it imports
— runs with the app's normal user privileges and can read the filesystem,
environment, and network. The security claims are about the *edges*:

- the webview can't reach Python except through `@expose`d functions;
- the host can't be corrupted, hung, or exhausted by the runtime;
- no process outlives the host.

Environment hygiene narrows the edge further: packaged spawns scrub the
`PYTHON*` config vars (`PYTHONHOME`, `PYTHONSTARTUP`, `PYTHONEXECUTABLE`,
`PYTHONUSERBASE`, `PYTHONINSPECT`, `PYTHONPLATLIBDIR`, `PYTHONWARNINGS`,
`PYTHONBREAKPOINT`) and set `PYTHONNOUSERSITE`/`PYTHONUTF8`, so the
bundled interpreter can't be redirected by whatever launched the app.
`PYTHONSAFEPATH=1` keeps the child's cwd off `sys.path` — a `json.py` or
`pylair/` in the launch directory would otherwise shadow the stdlib or
the runtime itself for `python -m` — and the packaged cwd is pinned to
the bundled resources dir rather than the directory the app was
launched from. That dir is not necessarily writable (a signed macOS
`.app`, an installed package): app code should resolve the OS app-data
directory for files it writes, never relative paths off cwd. Isolated
workers inherit all of this. AppImage `APPDIR` entries are
stripped from `LD_LIBRARY_PATH`/`PATH` (exact-prefix match — sibling dirs
survive). Dev overrides (`PYLAIR_APP_MODULE`, `PYLAIR_PYTHON`,
`PYLAIR_PYTHONPATH`) are honored **in debug builds only** — a packaged
app ignores them, so the launching environment can't swap which
interpreter or module runs. On Windows the interpreter spawns with
`CREATE_NO_WINDOW` so the GUI app shows no stray console; set
`PYLAIR_CONSOLE` (any value) in the launching environment to keep the
window for debugging — it grants no extra privilege, just visibility.

## 9. Wrap mode (W1, in progress)

Wrap mode shows a Python web framework's own UI (Gradio, Streamlit,
FastAPI…) in the window, served on `127.0.0.1` on a random port. Its
posture, per `docs/decisions/0003-wrap-mode.md`:

- The wrapped page is a **remote origin → zero Tauri IPC** (verified in
  the spike; designated a permanent acceptance-job check). A compromised
  wrapped page cannot reach `python_*` or any plugin command.
- W1 locks in-window navigation to the app origin via exact origin match
  (`same_origin` — implemented, parse-based, so `evil.com` subdomains,
  `user@host` tricks, and default-port confusion all fail) and sends
  other links to the system browser. The navigation handler itself is
  part of the W1 work.
- Adapters force `share=False`, disable tunnels and framework telemetry.
- **Stated exposure, no more:** a loopback server is reachable by other
  local processes and by pages in the user's browser — the same
  exposure as `streamlit run` on a developer machine. A token-checking
  reverse proxy is a documented future hardening (and noted as
  insufficient alone, since the app's own port stays open).

## 10. Supply chain and distribution

- The bundled interpreter is a pinned python-build-standalone build plus
  a `uv`-locked environment — the app doesn't depend on whatever Python
  the user's machine happens to have.
- `pylair build` output is **unsigned** unless the app developer
  configures code signing; expect Gatekeeper/SmartScreen warnings, as
  documented in the README. Bundles ship `THIRD-PARTY-NOTICES` for the
  CPython build and Python dependencies they contain.
- Signing keys and secrets never live in the repo; CI signing uses
  repository secrets (project brief §11).
- `cargo audit` reports **RUSTSEC-2024-0429** (`glib 0.18.5`, unsound) —
  a transitive dependency of Tauri's Linux GTK chain
  (`tauri → wry → webkit2gtk → gtk`), so it affects Linux builds only.
  There is no fix we can apply until Tauri moves off gtk-rs 0.18; the
  advisory is tracked upstream and disclosed here as accepted residual
  risk.

## 11. Reporting

Report vulnerabilities through GitHub's private vulnerability reporting
on this repository — not in public issues or PRs. Pre-1.0, honest bug
reports about the boundaries above are as welcome as exploits.
