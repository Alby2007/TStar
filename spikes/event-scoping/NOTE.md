# Investigation: `pylair://stream` event scoping across webviews

**Status: investigation only — no shipped code changed.** The review's claim:
`emit_to(label, "pylair://stream", …)` does not isolate windows, because the
client's `listen()` defaults to target `{kind: "Any"}`. Both halves reproduce
on Windows.

## Repro (`spikes/event-scoping/`)

Minimal Tauri app, not shipped: two webview windows (`win-a`, `win-b`), the
real `tauri-plugin-pylair` + real Python runtime (`testapp::counter`), and
capabilities identical to the scaffold template (`core:default` +
`pylair:default`). Window A runs the client's dispatch verbatim (tasks map +
early buffer + default-target `listen("pylair://stream")`). Window B runs the
same default listener plus a forger. Run with:

```sh
cd spikes/event-scoping/src-tauri
PYLAIR_APP_MODULE=testapp PYLAIR_PYTHON=python \
  PYLAIR_PYTHONPATH="<repo>/python/pylair/src;<repo>/python/pylair/tests" \
  cargo run
```

Evidence lands in `spikes/event-scoping/out.log` (webview → `emit("repro-log")`
→ Rust file sink). From the actual run on Windows 11:

```
[win-b] B saw task=1 seq=0 value=0                     <- leak
[win-a] A accepted task=1 seq=999 value="FORGED-BY-B"          <- forged chunk accepted
[win-a] A accepted task=1 seq=998 value="FORGED-EMITTO-BY-B"   <- emitTo variant accepted
[win-a] A accepted task=1 TERMINAL "FORGED-RESULT-BY-B"        <- forged RESULT accepted
[win-a] A accepted task=2 TERMINAL "FORGED-EARLY-BY-B"         <- early-buffer forgery
```

## Findings — both reproduced

1. **Leak: B receives A's stream events.** Every chunk the plugin emitted via
   `app.emit_to("win-a", …)` arrived at B's default-target listener. ~480
   `B saw` lines.
2. **Forged chunk/result accepted by A.** B's plain `emit("pylair://stream",
   {task: <A's live id>, …})` was delivered to A's client dispatch and yielded
   mid-stream; a forged `done` terminal resolved A's call with B's value. The
   real Python terminal arrived later and was discarded (task already closed).
   `emitTo("win-a", …)` from B also landed — `core:event:allow-emit-to` works.
3. **Early-buffer forgery (worse variant found).** Task ids are sequential
   (`next_task: AtomicU64` from 1). B pre-fired a `done` event for `task+1`;
   it sat in the client's `early` map (30 s TTL) and was replayed when A's
   *next* call registered that id — the call resolved with the forged result
   before Python could answer.
4. **Forged events are indistinguishable.** B's listener cannot tell its own
   forged events from plugin-emitted ones (no emitter identity reaches JS);
   B's forger self-triggered into a runaway, forging each new task id it saw.

## Why (tauri 2.12.5 source)

- `emit_to` builds a filter over **listener-declared targets**
  (`manager/mod.rs::emit_to` → `emit_js_filter`); it iterates *every* webview.
  `match_any_or_filter` short-circuits `EventTarget::Any` before the label
  check — the client's default `listen()` is exactly that.
- Plain `emit` passes `filter: None` (`emit_js`) → `unwrap_or(true)` — every
  handler in every webview fires regardless of declared target. Webview-
  originated emits ride the same bus.
- `core:default` includes `core:event:default` = `allow-listen`,
  `allow-unlisten`, `allow-emit`, `allow-emit-to` — a scaffolded app's
  webviews may both eavesdrop and inject by default.

## Stopgaps (no wire change)

- **Client listens with an explicit self target**
  (`target: {kind:"Webview", label:<self>}` via `getCurrentWebview().label`):
  blocks *passive* eavesdropping under `emit_to` — but a hostile window can
  `listen` with *A's* label and opt back in, and plain `emit` ignores listener
  targets entirely. Weak.
- **Deny webview emit in capabilities** (`core:event:deny-emit`,
  `core:event:deny-emit-to`): deny is origin-checked and wins over allows
  (`ipc/authority.rs::denied_on` runs first). Kills forging outright — no
  window can inject events. Nothing in `@pylair/client` emits, so the template
  could add these today; cost: apps lose app-level `emit`/`emitTo` (they can
  re-grant per-window). Does **not** fix the leak — passive listeners still
  receive `emit_to`-targeted events.
- Neither stopgap is sufficient alone; combined (deny emit + self-targeted
  listen) they close both holes for *cooperative* windows, but `allow-listen`
  is unscoped — a hostile window can still opt into A's label.

## Design note: `tauri::ipc::Channel` for stream/result delivery

The real fix: deliver stream events over a per-call IPC channel instead of
the shared event bus. A `Channel` is bound to the invoking webview's IPC
context — messages reach only that webview, no event name, no bus, nothing to
eavesdrop or forge.

- **Plugin command**: `python_call_stream(app, window, fn_name, input,
  on_event: Channel<StreamEvent>) -> Result<u64, ErrorObj>`. The spawned
  forwarder calls `on_event.send(payload)` instead of `app.emit_to(&label, …)`.
  The `Registry` (`subscribed`/`broadcast_targets`) bookkeeping for stream
  delivery goes away; window-close cleanup comes free (the channel dies with
  the webview — the semantics the registry manually emulates today).
- **Client API**: unchanged signatures — `call()`/`stream()` construct
  `new Channel<StreamEvent>()`, set `channel.onmessage`, pass it as the
  `onEvent` invoke arg. The shared listener, `tasks` map, `early` buffer and
  `holdEarly`/`register` machinery delete outright: each call receives only
  its own events, so no routing by task id is needed (the task id is still
  returned for `cancel`/`grant_credit`, which keep their label-ownership
  checks server-side).
- **Wire protocol (host↔Python)**: unchanged — `StreamItem` flow, credit,
  terminals are identical; only the Rust→JS hop changes.
- **`pylair://state` lifecycle events** are a separate question: they are
  genuinely broadcast and carry no per-call data — they can stay events (or
  move to a per-subscribe channel later); scope them out of this change.
- **Compatibility**: adding a required invoke arg breaks direct
  `invoke("plugin:pylair|python_call_stream")` callers and any client older
  than the plugin — acceptable pre-1.0, but the version pin bump must land in
  the same release notes. Optionally keep `pylair://stream` emitting as a
  deprecated compatibility shim for one RC — cheap to do, but it re-opens the
  exact leak this removes; recommend a clean cut.
- **Residual**: a hostile webview can still `invoke` commands — ownership
  checks (`window.label()` server-side) remain the boundary for
  cancel/grant/stream-start, exactly as today. Nothing about Channel weakens
  or strengthens that.

**Verdict for the docs**: per-window `emit_to` scoping is best-effort, not a
security boundary — PR #19's wording is accurate as merged.
